//! Owner-maintained price table for usage cost *estimates*
//! (`GET|PUT /v2/usage/prices`). Prices are per 1M tokens; each entry may
//! name its own currency (USD / AUD / CNY, default: the table currency)
//! and aliases. Manual entries win over the price library (`catalog.rs`).
//!
//! Storage (docs/usage-pricing.md §Rollback):
//! - `<cutex home>/usage-prices-v2.json` — authoritative table
//!   (`{version: 2, currency, models, legacySha256}`).
//! - `<cutex home>/usage-prices.json` — the r216 format (`{currency,
//!   models: {k: {input, cachedInput?, output}}}`), rewritten as a
//!   projection on every save: entries converted to the table currency,
//!   aliases expanded to extra keys. r216 and older read only this file.
//!   When its bytes no longer match `legacySha256` an older binary (or a
//!   hand edit) changed it after our last save: it is imported again and
//!   wins (losing aliases / per-entry currencies; the `modelMap` of the
//!   v2 file is kept, the older binary cannot have edited it).
//!
//! `modelMap` (capability `usage-model-map`): `{"<from>": "<to>"}` — a
//! sample whose model matches `from` is priced as model `to` through the
//! same manual → library → built-in chain (one hop, keys ignore ASCII case).

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;

use crate::config::atomic::write_private_bytes_atomic;
use crate::config::usage_pricing::Currency;
use crate::config::usage_pricing::ExchangeRates;

use super::model::TokenUsageBreakdown;

const PRICES_FILE: &str = "usage-prices.json";
const PRICES_V2_FILE: &str = "usage-prices-v2.json";
pub const MAX_PRICED_MODELS: usize = 256;
pub const MAX_ALIASES_PER_MODEL: usize = 32;
pub const MAX_MODEL_MAP_ENTRIES: usize = 256;
const MAX_MODEL_KEY_LEN: usize = 160;
const MAX_PRICE_PER_MILLION: f64 = 1_000_000.0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelPrice {
    /// Uncached input per 1M tokens.
    pub input: f64,
    /// Cached input per 1M tokens; absent = same as `input`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input: Option<f64>,
    /// Cache-write input per 1M tokens; absent = same as `input`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_input: Option<f64>,
    /// Output (reasoning output is a subset of output) per 1M tokens.
    pub output: f64,
    /// USD / AUD / CNY; absent = the table currency.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    /// Other model names this price applies to (matched like keys).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}

impl ModelPrice {
    pub fn simple(input: f64, output: f64) -> Self {
        Self { input, cached_input: None, cache_write_input: None, output, currency: None, aliases: Vec::new() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsagePriceTable {
    /// Default currency of the entries.
    pub currency: String,
    #[serde(default)]
    pub models: BTreeMap<String, ModelPrice>,
    /// Pricing model map `from → to` (price `from` as `to`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub model_map: BTreeMap<String, String>,
}

impl Default for UsagePriceTable {
    fn default() -> Self {
        Self {
            currency: "USD".to_string(),
            models: BTreeMap::new(),
            model_map: BTreeMap::new(),
        }
    }
}

/// r216 entry shape (`deny_unknown_fields` there).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyModelPrice {
    input: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cached_input: Option<f64>,
    output: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyTable {
    currency: String,
    #[serde(default)]
    models: BTreeMap<String, LegacyModelPrice>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredTableV2 {
    version: u32,
    currency: String,
    #[serde(default)]
    models: BTreeMap<String, ModelPrice>,
    /// Pricing model map (newer than the v2 file itself; older v2 readers
    /// ignore it and drop it on their next save).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    model_map: BTreeMap<String, String>,
    /// SHA-256 (hex) of the `usage-prices.json` bytes written with it.
    #[serde(default)]
    legacy_sha256: Option<String>,
}

fn valid_name(name: &str) -> bool {
    !name.trim().is_empty()
        && name.trim() == name
        && name.len() <= MAX_MODEL_KEY_LEN
        && !name.chars().any(char::is_control)
}

fn valid_currency_code(currency: &str) -> bool {
    !currency.is_empty() && currency.len() <= 8 && currency.chars().all(|c| c.is_ascii_alphanumeric())
}

impl UsagePriceTable {
    /// Validation of a table written through the API: currencies must be
    /// USD / AUD / CNY.
    pub fn validate(&self) -> anyhow::Result<()> {
        self.validate_stored()?;
        anyhow::ensure!(
            Currency::parse(&self.currency).is_some(),
            "currency must be USD, AUD or CNY"
        );
        for (model, price) in &self.models {
            if let Some(currency) = &price.currency {
                anyhow::ensure!(
                    Currency::parse(currency).is_some(),
                    "models[{model}].currency must be USD, AUD or CNY"
                );
            }
        }
        Ok(())
    }

    /// Validation of a stored table: older tables may name any currency
    /// code (their entries are then only usable in that currency).
    pub fn validate_stored(&self) -> anyhow::Result<()> {
        if !valid_currency_code(&self.currency) {
            anyhow::bail!("currency must be 1-8 ASCII letters or digits (e.g. USD, CNY)");
        }
        if self.models.len() > MAX_PRICED_MODELS {
            anyhow::bail!("at most {MAX_PRICED_MODELS} models may be priced");
        }
        let mut lowered = std::collections::HashSet::new();
        for (model, price) in &self.models {
            if !valid_name(model) {
                anyhow::bail!(
                    "model keys must be non-empty, trimmed, at most {MAX_MODEL_KEY_LEN} bytes, without control characters"
                );
            }
            for (field, value) in [
                ("input", Some(price.input)),
                ("cachedInput", price.cached_input),
                ("cacheWriteInput", price.cache_write_input),
                ("output", Some(price.output)),
            ] {
                if let Some(value) = value {
                    if !value.is_finite() || !(0.0..=MAX_PRICE_PER_MILLION).contains(&value) {
                        anyhow::bail!(
                            "models[{model}].{field} must be a finite number between 0 and {MAX_PRICE_PER_MILLION}"
                        );
                    }
                }
            }
            if let Some(currency) = &price.currency {
                anyhow::ensure!(valid_currency_code(currency), "models[{model}].currency is invalid");
            }
            anyhow::ensure!(
                price.aliases.len() <= MAX_ALIASES_PER_MODEL,
                "models[{model}].aliases: at most {MAX_ALIASES_PER_MODEL}"
            );
            for alias in &price.aliases {
                anyhow::ensure!(
                    valid_name(alias),
                    "models[{model}].aliases must be non-empty, trimmed, at most {MAX_MODEL_KEY_LEN} bytes, without control characters"
                );
            }
            for name in std::iter::once(model).chain(&price.aliases) {
                if !lowered.insert(name.to_ascii_lowercase()) {
                    anyhow::bail!("model keys and aliases must be unique ignoring ASCII case: {name}");
                }
            }
        }
        self.validate_model_map(&lowered)
    }

    /// `modelMap`: at most 256 entries, names like model keys, `from`
    /// unique ignoring case, not a manual key / alias (that price would be
    /// unreachable), never `to` of another entry (one hop, no chains).
    fn validate_model_map(&self, manual_names: &std::collections::HashSet<String>) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.model_map.len() <= MAX_MODEL_MAP_ENTRIES,
            "modelMap: at most {MAX_MODEL_MAP_ENTRIES} entries"
        );
        let mut froms = std::collections::HashSet::new();
        for (from, to) in &self.model_map {
            for name in [from, to] {
                anyhow::ensure!(
                    valid_name(name),
                    "modelMap names must be non-empty, trimmed, at most {MAX_MODEL_KEY_LEN} bytes, without control characters"
                );
            }
            let lowered = from.to_ascii_lowercase();
            anyhow::ensure!(lowered != to.to_ascii_lowercase(), "modelMap[{from}] maps a model to itself");
            anyhow::ensure!(froms.insert(lowered.clone()), "modelMap keys must be unique ignoring ASCII case: {from}");
            anyhow::ensure!(
                !manual_names.contains(&lowered),
                "modelMap[{from}]: {from} has a manual price; remove the entry or the mapping"
            );
        }
        for (from, to) in &self.model_map {
            anyhow::ensure!(
                !froms.contains(&to.to_ascii_lowercase()),
                "modelMap[{from}] -> {to}: {to} is mapped itself (no chains)"
            );
        }
        Ok(())
    }

    /// Lowercased `from` → `to`.
    pub fn model_map_index(&self) -> HashMap<String, String> {
        self.model_map.iter().map(|(from, to)| (from.to_ascii_lowercase(), to.clone())).collect()
    }

    /// Lowercased key / alias → key.
    pub fn index(&self) -> HashMap<String, String> {
        let mut index = HashMap::new();
        for (key, price) in &self.models {
            for name in std::iter::once(key).chain(&price.aliases) {
                index.entry(name.to_ascii_lowercase()).or_insert_with(|| key.clone());
            }
        }
        index
    }

    /// Price for a sample. Lookup order: `"<provider>/<model>"`, then each
    /// normalized form of the model (`catalog::model_candidates`: lowercase,
    /// no `[1m]` suffix, no `vendor/` prefix, no date suffix), each as
    /// `"<provider>/<form>"` and bare; keys and aliases ignore ASCII case.
    pub fn price_for(&self, provider: Option<&str>, model: Option<&str>) -> Option<(&str, &ModelPrice)> {
        self.price_for_indexed(&self.index(), provider, model)
    }

    /// Literal configuration ID or explicit alias; do not strip `[400K]`.
    pub(super) fn exact_price_for_indexed<'a>(
        &'a self,
        index: &HashMap<String, String>,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> Option<(&'a str, &'a ModelPrice)> {
        let model = model?.trim().to_ascii_lowercase();
        let names = provider.map(|provider| format!("{}/{model}", provider.trim().to_ascii_lowercase()))
            .into_iter().chain(std::iter::once(model));
        names.filter_map(|name| index.get(&name))
            .find_map(|key| self.models.get_key_value(key).map(|(key, price)| (key.as_str(), price)))
    }

    pub fn price_for_indexed<'a>(
        &'a self,
        index: &HashMap<String, String>,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> Option<(&'a str, &'a ModelPrice)> {
        let model = model?;
        let provider = provider.map(|p| p.trim().to_ascii_lowercase());
        let mut names = Vec::new();
        if let Some(provider) = &provider {
            names.push(format!("{provider}/{}", model.trim().to_ascii_lowercase()));
        }
        for candidate in super::catalog::model_candidates(model) {
            if let Some(provider) = &provider {
                names.push(format!("{provider}/{candidate}"));
            }
            names.push(candidate);
        }
        names.iter().find_map(|name| {
            let key = index.get(name)?;
            self.models.get_key_value(key.as_str()).map(|(key, price)| (key.as_str(), price))
        })
    }

    /// Currency of an entry (`None` = not USD / AUD / CNY).
    pub fn entry_currency(&self, price: &ModelPrice) -> Option<Currency> {
        Currency::parse(price.currency.as_deref().unwrap_or(&self.currency))
    }

    /// The r216-format projection: entries in the table currency, aliases
    /// as extra keys (first come, case-unique, at most 256).
    fn legacy_projection(&self, rates: &ExchangeRates) -> LegacyTable {
        let table_currency = Currency::parse(&self.currency);
        let mut models = BTreeMap::new();
        let mut seen = std::collections::HashSet::new();
        let mut entries: Vec<(&String, &ModelPrice, bool)> = Vec::new();
        for (key, price) in &self.models {
            entries.push((key, price, true));
        }
        for (_, price) in &self.models {
            for alias in &price.aliases {
                entries.push((alias, price, false));
            }
        }
        // Mapped models whose target has a manual price: older binaries
        // price them through that entry, like aliases.
        let index = self.index();
        for (from, to) in &self.model_map {
            if let Some((_, price)) = self.price_for_indexed(&index, None, Some(to)) {
                entries.push((from, price, false));
            }
        }
        for (name, price, _) in entries {
            if models.len() >= MAX_PRICED_MODELS || !seen.insert(name.to_ascii_lowercase()) {
                continue;
            }
            let entry_currency = price.currency.as_deref().map(Currency::parse);
            let factor = match (entry_currency, table_currency) {
                (None, _) => 1.0,
                (Some(Some(from)), Some(to)) => rates.convert(1.0, from, to),
                (Some(from), _) if from == table_currency => 1.0,
                _ => continue,
            };
            let convert = |value: f64| (value * factor * 1e9).round() / 1e9;
            models.insert(
                name.clone(),
                LegacyModelPrice {
                    input: convert(price.input),
                    cached_input: price.cached_input.map(convert),
                    output: convert(price.output),
                },
            );
        }
        LegacyTable { currency: self.currency.clone(), models }
    }
}

/// Estimated cost of one usage breakdown in the entry's currency. Cached
/// and cache-write input use their own rates when given, else `input`.
pub fn estimate_cost(price: &ModelPrice, tokens: &TokenUsageBreakdown) -> f64 {
    token_cost(
        tokens,
        price.input,
        price.cached_input,
        price.cache_write_input,
        price.output,
    )
}

/// Shared per-1M token arithmetic (manual and library prices).
pub fn token_cost(
    tokens: &TokenUsageBreakdown,
    input: f64,
    cached_input: Option<f64>,
    cache_write_input: Option<f64>,
    output: f64,
) -> f64 {
    let cached = tokens.cached_input_tokens.min(tokens.input_tokens);
    let cache_write = tokens.cache_write_input_tokens.min(tokens.input_tokens - cached);
    let uncached = tokens.input_tokens - cached - cache_write;
    (uncached as f64 * input
        + cached as f64 * cached_input.unwrap_or(input)
        + cache_write as f64 * cache_write_input.unwrap_or(input)
        + tokens.output_tokens as f64 * output)
        / 1_000_000.0
}

pub fn prices_path() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::config_dir()?.join(PRICES_FILE))
}

fn v2_path(legacy_path: &Path) -> PathBuf {
    legacy_path.with_file_name(PRICES_V2_FILE)
}

fn sha256_hex(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// A loaded table and where it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedPrices {
    pub table: UsagePriceTable,
    /// `v2`, `legacy` (no v2 file yet) or `legacy-changed` (an older cutex
    /// edited `usage-prices.json` after our last save; imported again).
    pub origin: &'static str,
}

pub fn load_price_table() -> anyhow::Result<UsagePriceTable> {
    Ok(load_prices()?.table)
}

pub fn load_prices() -> anyhow::Result<LoadedPrices> {
    load_prices_at(&prices_path()?)
}

pub fn save_price_table(table: &UsagePriceTable, rates: &ExchangeRates) -> anyhow::Result<()> {
    save_price_table_at(&prices_path()?, table, rates)
}

fn read_optional(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("Failed to read usage prices: {}", path.display())),
    }
}

fn parse_legacy(path: &Path, bytes: &[u8]) -> anyhow::Result<UsagePriceTable> {
    let legacy: LegacyTable = serde_json::from_slice(bytes)
        .with_context(|| format!("Failed to parse usage prices: {}", path.display()))?;
    Ok(UsagePriceTable {
        currency: legacy.currency,
        models: legacy
            .models
            .into_iter()
            .map(|(key, price)| {
                (key, ModelPrice { cached_input: price.cached_input, ..ModelPrice::simple(price.input, price.output) })
            })
            .collect(),
        model_map: BTreeMap::new(),
    })
}

#[cfg(test)]
pub(super) fn load_price_table_at(path: &Path) -> anyhow::Result<UsagePriceTable> {
    Ok(load_prices_at(path)?.table)
}

pub(super) fn load_prices_at(legacy_path: &Path) -> anyhow::Result<LoadedPrices> {
    let v2_path = v2_path(legacy_path);
    let legacy = read_optional(legacy_path)?;
    let v2 = read_optional(&v2_path)?
        .map(|bytes| {
            serde_json::from_slice::<StoredTableV2>(&bytes)
                .with_context(|| format!("Failed to parse usage prices: {}", v2_path.display()))
        })
        .transpose()?;
    let loaded = match (v2, legacy) {
        (Some(v2), Some(bytes)) if v2.legacy_sha256.as_deref() != Some(sha256_hex(&bytes).as_str()) => {
            let mut table = parse_legacy(legacy_path, &bytes)?;
            // The legacy format has no map; keep ours unless the imported
            // entries now price a mapped name directly.
            let names: std::collections::HashSet<String> = table.index().into_keys().collect();
            table.model_map = v2.model_map.into_iter().filter(|(from, _)| !names.contains(&from.to_ascii_lowercase())).collect();
            LoadedPrices { table, origin: "legacy-changed" }
        }
        (Some(v2), _) => LoadedPrices {
            table: UsagePriceTable { currency: v2.currency, models: v2.models, model_map: v2.model_map },
            origin: "v2",
        },
        (None, Some(bytes)) => LoadedPrices { table: parse_legacy(legacy_path, &bytes)?, origin: "legacy" },
        (None, None) => LoadedPrices { table: UsagePriceTable::default(), origin: "legacy" },
    };
    loaded.table.validate_stored()?;
    Ok(loaded)
}

pub(super) fn save_price_table_at(
    legacy_path: &Path,
    table: &UsagePriceTable,
    rates: &ExchangeRates,
) -> anyhow::Result<()> {
    table.validate_stored()?;
    if let Some(parent) = legacy_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let legacy = serde_json::to_vec_pretty(&table.legacy_projection(rates))?;
    let v2 = StoredTableV2 {
        version: 2,
        currency: table.currency.clone(),
        models: table.models.clone(),
        model_map: table.model_map.clone(),
        legacy_sha256: Some(sha256_hex(&legacy)),
    };
    // Legacy first: a crash in between reads as "legacy changed" and only
    // loses the v2 extras of this save.
    write_private_bytes_atomic(legacy_path, &legacy)?;
    write_private_bytes_atomic(&v2_path(legacy_path), &serde_json::to_vec_pretty(&v2)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cutex-usage-prices-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn aliases_currencies_and_normalized_lookup() {
        let table: UsagePriceTable = serde_json::from_value(json!({
            "currency": "USD",
            "models": {
                "deepseek-chat": { "input": 2.0, "output": 3.0, "currency": "CNY", "aliases": ["deepseek-v3.2"] },
                "glm-5.3": { "input": 1.0, "cachedInput": 0.2, "cacheWriteInput": 1.25, "output": 4.0 }
            }
        }))
        .unwrap();
        table.validate().unwrap();
        let key = |provider: Option<&str>, model: &str| table.price_for(provider, Some(model)).map(|(key, _)| key);
        assert_eq!(key(None, "deepseek/deepseek-chat"), Some("deepseek-chat"));
        assert_eq!(key(Some("deepseek"), "DeepSeek-V3.2"), Some("deepseek-chat"));
        assert_eq!(key(None, "glm-5.3-20260901"), Some("glm-5.3"));
        assert_eq!(key(None, "GLM-5.3[1m]"), Some("glm-5.3"));
        assert_eq!(key(None, "glm-5"), None);
        let (_, price) = table.price_for(None, Some("deepseek-chat")).unwrap();
        assert_eq!(table.entry_currency(price), Some(Currency::Cny));

        // cache writes use their own rate.
        let tokens = TokenUsageBreakdown {
            total_tokens: 1_100_000,
            input_tokens: 1_000_000,
            cached_input_tokens: 400_000,
            cache_write_input_tokens: 100_000,
            output_tokens: 100_000,
            reasoning_output_tokens: 0,
        };
        let (_, glm) = table.price_for(None, Some("glm-5.3")).unwrap();
        // 0.5M*1 + 0.4M*0.2 + 0.1M*1.25 + 0.1M*4 = 0.5+0.08+0.125+0.4
        assert!((estimate_cost(glm, &tokens) - 1.105).abs() < 1e-9);

        for bad in [
            json!({ "currency": "EUR", "models": {} }),
            json!({ "currency": "USD", "models": { "m": { "input": 1.0, "output": 1.0, "currency": "EUR" } } }),
            json!({ "currency": "USD", "models": { "a": { "input": 1.0, "output": 1.0, "aliases": ["B"] }, "b": { "input": 1.0, "output": 1.0 } } }),
            json!({ "currency": "USD", "models": { "a": { "input": 1.0, "output": 1.0, "aliases": [" x"] } } }),
            json!({ "currency": "USD", "models": { "a": { "input": 1.0, "cacheWriteInput": -1.0, "output": 1.0 } } }),
        ] {
            let table: UsagePriceTable = serde_json::from_value(bad.clone()).unwrap();
            assert!(table.validate().is_err(), "{bad}");
        }
        // Stored legacy tables may use other currency codes.
        let eur: UsagePriceTable = serde_json::from_value(json!({ "currency": "EUR", "models": {} })).unwrap();
        assert!(eur.validate_stored().is_ok());
    }

    #[test]
    fn v2_file_keeps_extras_and_writes_an_r216_readable_projection() {
        let dir = temp_dir("v2");
        let path = dir.join(PRICES_FILE);
        let rates = ExchangeRates { aud: 1.5, cny: 7.0 };
        let table: UsagePriceTable = serde_json::from_value(json!({
            "currency": "USD",
            "models": {
                "deepseek-chat": { "input": 7.0, "output": 14.0, "currency": "CNY", "aliases": ["deepseek/deepseek-chat"] },
                "gpt-5.5": { "input": 5.0, "cachedInput": 0.5, "cacheWriteInput": 6.0, "output": 30.0 }
            }
        }))
        .unwrap();
        save_price_table_at(&path, &table, &rates).unwrap();
        let loaded = load_prices_at(&path).unwrap();
        assert_eq!((loaded.origin, &loaded.table), ("v2", &table));

        // The legacy file parses with the r216 shape (strict, no extras).
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        #[allow(dead_code)]
        struct R216Price {
            input: f64,
            #[serde(default)]
            cached_input: Option<f64>,
            output: f64,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        #[allow(dead_code)]
        struct R216Table {
            currency: String,
            models: BTreeMap<String, R216Price>,
        }
        let r216: R216Table = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(r216.currency, "USD");
        assert_eq!(r216.models.len(), 3);
        assert_eq!(r216.models["deepseek-chat"].input, 1.0);
        assert_eq!(r216.models["deepseek/deepseek-chat"].output, 2.0);

        // An older binary rewrote the legacy file: it wins on the next read.
        std::fs::write(&path, br#"{"currency":"AUD","models":{"x":{"input":1.0,"output":2.0}}}"#).unwrap();
        let loaded = load_prices_at(&path).unwrap();
        assert_eq!(loaded.origin, "legacy-changed");
        assert_eq!(loaded.table.currency, "AUD");
        assert_eq!(loaded.table.models.keys().collect::<Vec<_>>(), ["x"]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn model_map_validates_round_trips_and_survives_older_binaries() {
        let ok: UsagePriceTable = serde_json::from_value(json!({
            "currency": "USD",
            "models": { "house": { "input": 1.0, "output": 2.0, "aliases": ["house-alias"] } },
            "modelMap": { "gemini-3.8-flash-tiere": "gemini-3.8-flash", "relay-house": "house", "sonnet": "claude-sonnet-4-5" }
        }))
        .unwrap();
        ok.validate().unwrap();
        for bad in [
            json!({ "a": "a" }),
            json!({ "A": "a" }),
            json!({ "a": "b", "A": "c" }),
            json!({ "a": "b", "b": "c" }),
            json!({ " a": "b" }),
            json!({ "a": "" }),
            json!({ "house": "x" }),
            json!({ "HOUSE-ALIAS": "x" }),
        ] {
            let table: UsagePriceTable = serde_json::from_value(json!({
                "currency": "USD",
                "models": { "house": { "input": 1.0, "output": 2.0, "aliases": ["house-alias"] } },
                "modelMap": bad.clone()
            }))
            .unwrap();
            assert!(table.validate().is_err(), "{bad}");
        }
        let too_many: BTreeMap<String, String> =
            (0..=MAX_MODEL_MAP_ENTRIES).map(|i| (format!("m{i}"), "t".to_string())).collect();
        let table = UsagePriceTable { model_map: too_many, ..UsagePriceTable::default() };
        assert!(table.validate().is_err());

        let dir = temp_dir("map");
        let path = dir.join(PRICES_FILE);
        let rates = ExchangeRates::default();
        save_price_table_at(&path, &ok, &rates).unwrap();
        assert_eq!(load_prices_at(&path).unwrap().table, ok);
        // Older v2 readers (no modelMap field) still parse the v2 file.
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        #[allow(dead_code)]
        struct OldV2 {
            version: u32,
            currency: String,
            models: BTreeMap<String, ModelPrice>,
            legacy_sha256: Option<String>,
        }
        let old: OldV2 = serde_json::from_slice(&std::fs::read(v2_path(&path)).unwrap()).unwrap();
        assert_eq!(old.models.len(), 1);
        // The r216 projection prices a name mapped to a manual entry.
        let legacy: LegacyTable = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(legacy.models.keys().collect::<Vec<_>>(), ["house", "house-alias", "relay-house"]);
        // An older binary changed the legacy file: imported, map kept except
        // names that are now priced directly.
        std::fs::write(&path, br#"{"currency":"USD","models":{"relay-house":{"input":1.0,"output":2.0}}}"#).unwrap();
        let loaded = load_prices_at(&path).unwrap();
        assert_eq!(loaded.origin, "legacy-changed");
        assert_eq!(
            loaded.table.model_map.keys().collect::<Vec<_>>(),
            ["gemini-3.8-flash-tiere", "sonnet"]
        );
        std::fs::remove_dir_all(dir).ok();
    }
}
