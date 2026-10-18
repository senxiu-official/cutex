//! Public price libraries for usage cost estimates (capability
//! `usage-price-catalog`): LiteLLM `model_prices_and_context_window.json`
//! or models.dev `api.json`, fetched by the daemon (background thread, at
//! most once per `usage.catalogRefreshHours`, conditional requests) and
//! cached normalized under `<runtime>/usage-pricing/`. A failed fetch keeps
//! the previous cache; reports only ever read the cache (never the network).
//! Optional exchange-rate auto-fetch (Frankfurter, ECB reference rates)
//! uses the same cache / fallback rules. See docs/usage-pricing.md.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::SystemTime;

use anyhow::Context;
use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::config::usage_pricing::Currency;
use crate::config::usage_pricing::EffectiveUsageSettings;
use crate::config::usage_pricing::ExchangeRates;
use crate::config::usage_pricing::PriceSource;
use crate::profiles::provider_category::ProviderCategory;

const CACHE_VERSION: u32 = 1;
const MAX_BODY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PRICE_PER_MILLION: f64 = 1_000_000.0;
/// Retry a failed fetch after this long even with a longer interval.
const FAILURE_RETRY_HOURS: i64 = 1;
pub const FX_URL: &str = "https://api.frankfurter.dev/v1/latest?base=USD&symbols=AUD,CNY";
const FX_REFRESH_HOURS: i64 = 24;

/// Library price per 1M tokens in USD.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryPrice {
    pub input: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_input: Option<f64>,
    pub output: f64,
    /// Rates for requests whose input exceeds `aboveTokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_context: Option<LongContextPrice>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LongContextPrice {
    pub above_tokens: u64,
    pub input: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_input: Option<f64>,
    pub output: f64,
}

/// Cached, normalized library (`<runtime>/usage-pricing/catalog-<source>.json`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogCache {
    pub version: u32,
    pub source: String,
    pub url: String,
    /// Last successful download (HTTP 200).
    pub fetched_at: Option<String>,
    /// Last attempt (200, 304 or failure).
    pub checked_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
    /// Error of the last attempt; the models are from `fetchedAt`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Lowercased model key → price.
    pub models: BTreeMap<String, LibraryPrice>,
}

/// Cached exchange rates (`<runtime>/usage-pricing/fx.json`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FxCache {
    pub version: u32,
    pub url: String,
    pub fetched_at: Option<String>,
    pub checked_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Publication date of the rates (from the source).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    /// Units per 1 USD.
    #[serde(default)]
    pub rates: BTreeMap<String, f64>,
}

// ---------------------------------------------------------------------------
// Parsers

fn per_million(per_token: f64) -> Option<f64> {
    let value = per_token * 1_000_000.0;
    (value.is_finite() && (0.0..=MAX_PRICE_PER_MILLION).contains(&value)).then(|| round(value))
}

fn valid_per_million(value: f64) -> Option<f64> {
    (value.is_finite() && (0.0..=MAX_PRICE_PER_MILLION).contains(&value)).then(|| round(value))
}

/// Round away float noise from per-token × 1e6 (e.g. 2.9999999999999996).
fn round(value: f64) -> f64 {
    (value * 1e9).round() / 1e9
}

/// LiteLLM `model_prices_and_context_window.json`: per-token USD costs.
/// Chat / responses / completion models with input and output costs only.
pub fn parse_litellm(value: &Value) -> anyhow::Result<BTreeMap<String, LibraryPrice>> {
    let object = value.as_object().context("LiteLLM price file must be a JSON object")?;
    let mut models = BTreeMap::new();
    for (key, entry) in object {
        if key == "sample_spec" {
            continue;
        }
        let Some(entry) = entry.as_object() else { continue };
        if entry
            .get("mode")
            .and_then(Value::as_str)
            .is_some_and(|mode| !matches!(mode, "chat" | "responses" | "completion"))
        {
            continue;
        }
        let number = |name: &str| entry.get(name).and_then(Value::as_f64).and_then(per_million);
        let (Some(input), Some(output)) =
            (number("input_cost_per_token"), number("output_cost_per_token"))
        else {
            continue;
        };
        // Smallest `*_above_<N>k_tokens` threshold with input and output.
        let mut long_context: Option<LongContextPrice> = None;
        for name in entry.keys() {
            let Some(threshold) = name
                .strip_prefix("input_cost_per_token_above_")
                .and_then(|rest| rest.strip_suffix("k_tokens"))
                .and_then(|k| k.parse::<u64>().ok())
            else {
                continue;
            };
            let suffix = format!("above_{threshold}k_tokens");
            let (Some(input), Some(output)) = (
                number(&format!("input_cost_per_token_{suffix}")),
                number(&format!("output_cost_per_token_{suffix}")),
            ) else {
                continue;
            };
            let above_tokens = threshold * 1000;
            if long_context.as_ref().is_some_and(|current| current.above_tokens <= above_tokens) {
                continue;
            }
            long_context = Some(LongContextPrice {
                above_tokens,
                input,
                cached_input: number(&format!("cache_read_input_token_cost_{suffix}")),
                cache_write_input: number(&format!("cache_creation_input_token_cost_{suffix}")),
                output,
            });
        }
        models.insert(
            key.trim().to_ascii_lowercase(),
            LibraryPrice {
                input,
                cached_input: number("cache_read_input_token_cost"),
                cache_write_input: number("cache_creation_input_token_cost"),
                output,
                long_context,
            },
        );
    }
    anyhow::ensure!(!models.is_empty(), "LiteLLM price file contained no chat model prices");
    Ok(models)
}

/// Providers whose models.dev entries also answer bare model names, in
/// priority order (first provider listing a model wins).
const MODELS_DEV_BARE_PROVIDERS: &[&str] = &[
    "openai",
    "anthropic",
    "deepseek",
    "zai",
    "zhipuai",
    "moonshotai",
    "moonshotai-cn",
    "google",
    "xai",
    "mistral",
    "alibaba",
    "minimax",
];

/// models.dev `api.json`: `{provider: {models: {id: {cost: {input, output,
/// cache_read, cache_write, context_over_200k}}}}}` with per-1M USD costs.
/// Keys are `provider/model`; preferred providers also get the bare id.
pub fn parse_models_dev(value: &Value) -> anyhow::Result<BTreeMap<String, LibraryPrice>> {
    let providers = value.as_object().context("models.dev api.json must be a JSON object")?;
    let mut models = BTreeMap::new();
    let mut bare: BTreeMap<String, (usize, LibraryPrice)> = BTreeMap::new();
    for (provider_key, provider) in providers {
        let provider_id = provider
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or(provider_key)
            .trim()
            .to_ascii_lowercase();
        let Some(entries) = provider.get("models").and_then(Value::as_object) else { continue };
        let rank = MODELS_DEV_BARE_PROVIDERS.iter().position(|p| *p == provider_id);
        for (model_key, model) in entries {
            let Some(cost) = model.get("cost").and_then(Value::as_object) else { continue };
            let number =
                |object: &serde_json::Map<String, Value>, name: &str| object.get(name).and_then(Value::as_f64).and_then(valid_per_million);
            let (Some(input), Some(output)) = (number(cost, "input"), number(cost, "output")) else {
                continue;
            };
            let long_context = cost.get("context_over_200k").and_then(Value::as_object).and_then(|over| {
                Some(LongContextPrice {
                    above_tokens: 200_000,
                    input: number(over, "input")?,
                    cached_input: number(over, "cache_read"),
                    cache_write_input: number(over, "cache_write"),
                    output: number(over, "output")?,
                })
            });
            let price = LibraryPrice {
                input,
                cached_input: number(cost, "cache_read"),
                cache_write_input: number(cost, "cache_write"),
                output,
                long_context,
            };
            let model_id = model
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or(model_key)
                .trim()
                .to_ascii_lowercase();
            if let Some(rank) = rank {
                if bare.get(&model_id).is_none_or(|(current, _)| rank < *current) {
                    bare.insert(model_id.clone(), (rank, price.clone()));
                }
            }
            models.insert(format!("{provider_id}/{model_id}"), price);
        }
    }
    for (model_id, (_, price)) in bare {
        models.entry(model_id).or_insert(price);
    }
    anyhow::ensure!(!models.is_empty(), "models.dev api.json contained no model prices");
    Ok(models)
}

pub fn parse_library(source: PriceSource, body: &[u8]) -> anyhow::Result<BTreeMap<String, LibraryPrice>> {
    let value: Value = serde_json::from_slice(body).context("price library is not valid JSON")?;
    match source {
        PriceSource::Litellm => parse_litellm(&value),
        PriceSource::ModelsDev => parse_models_dev(&value),
        PriceSource::Manual => anyhow::bail!("the manual source has no library"),
    }
}

/// Frankfurter `{"base":"USD","date":..,"rates":{"AUD":..,"CNY":..}}`.
pub fn parse_fx(body: &[u8]) -> anyhow::Result<(Option<String>, BTreeMap<String, f64>)> {
    let value: Value = serde_json::from_slice(body).context("exchange rates are not valid JSON")?;
    anyhow::ensure!(
        value.get("base").and_then(Value::as_str).is_none_or(|base| base.eq_ignore_ascii_case("USD")),
        "exchange rates are not based on USD"
    );
    let rates = value.get("rates").and_then(Value::as_object).context("exchange rates omitted rates")?;
    let mut out = BTreeMap::new();
    for currency in [Currency::Aud, Currency::Cny] {
        let rate = rates
            .get(currency.code())
            .and_then(Value::as_f64)
            .filter(|rate| rate.is_finite() && *rate > 0.0 && *rate <= 1_000_000.0)
            .with_context(|| format!("exchange rates omitted {}", currency.code()))?;
        out.insert(currency.code().to_string(), rate);
    }
    Ok((value.get("date").and_then(Value::as_str).map(str::to_string), out))
}

// ---------------------------------------------------------------------------
// Lookup / normalization

/// Lookup forms of a model name: lowercased, without a `[1m]`-style
/// suffix, without a `vendor/` prefix, without a date suffix
/// (`-20250929` / `-2025-09-29`).
pub fn model_candidates(model: &str) -> Vec<String> {
    let mut base = model.trim().to_ascii_lowercase();
    if let Some(start) = base.rfind('[') {
        if base.ends_with(']') {
            base.truncate(start);
        }
    }
    let base = base.trim().to_string();
    let mut out: Vec<String> = Vec::new();
    let mut push = |value: String| {
        if !value.is_empty() && !out.contains(&value) {
            out.push(value);
        }
    };
    let mut forms = vec![base.clone()];
    if let Some((_, tail)) = base.rsplit_once('/') {
        forms.push(tail.to_string());
    }
    for form in forms {
        push(form.clone());
        if let Some(stripped) = strip_date_suffix(&form) {
            push(stripped);
        }
    }
    out
}

fn strip_date_suffix(model: &str) -> Option<String> {
    let bytes = model.as_bytes();
    let digits = |slice: &[u8]| slice.iter().all(u8::is_ascii_digit);
    // -YYYYMMDD
    if bytes.len() > 9 && bytes[bytes.len() - 9] == b'-' && digits(&bytes[bytes.len() - 8..]) {
        return Some(model[..model.len() - 9].to_string());
    }
    // -YYYY-MM-DD
    if bytes.len() > 11 {
        let tail = &bytes[bytes.len() - 11..];
        if tail[0] == b'-' && digits(&tail[1..5]) && tail[5] == b'-' && digits(&tail[6..8]) && tail[8] == b'-' && digits(&tail[9..]) {
            return Some(model[..model.len() - 11].to_string());
        }
    }
    None
}

/// Library key prefixes for a provider family.
fn category_prefixes(category: ProviderCategory) -> &'static [&'static str] {
    match category {
        ProviderCategory::Codex => &["openai"],
        ProviderCategory::Claude => &["anthropic"],
        ProviderCategory::Deepseek => &["deepseek"],
        ProviderCategory::Glm => &["zhipuai", "zai"],
        ProviderCategory::Zai => &["zai", "zhipuai"],
        ProviderCategory::Kimi => &["moonshot", "moonshotai", "moonshotai-cn"],
        ProviderCategory::Other => &[],
    }
}

/// Key prefixes to try for a sample: the attribution provider itself, then
/// the family's library names.
pub fn provider_prefixes(provider: Option<&str>, category: Option<ProviderCategory>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |value: &str| {
        let value = value.trim().to_ascii_lowercase();
        if !value.is_empty() && !out.contains(&value) {
            out.push(value);
        }
    };
    if let Some(provider) = provider {
        push(provider);
    }
    let family = provider
        .and_then(crate::profiles::provider_category::category_for_provider_name)
        .or(category);
    if let Some(family) = family {
        for prefix in category_prefixes(family) {
            push(prefix);
        }
    }
    out
}

impl CatalogCache {
    /// Library price of a model: `<prefix>/<candidate>` for each provider
    /// prefix, then the bare candidate, then the prefixes of the family the
    /// model name itself belongs to. Returns the matched key.
    pub fn lookup(
        &self,
        provider: Option<&str>,
        category: Option<ProviderCategory>,
        model: &str,
    ) -> Option<(&str, &LibraryPrice)> {
        let prefixes = provider_prefixes(provider, category);
        let candidates = model_candidates(model);
        for candidate in &candidates {
            for prefix in &prefixes {
                if let Some((key, price)) = self.models.get_key_value(&format!("{prefix}/{candidate}")) {
                    return Some((key.as_str(), price));
                }
            }
            if let Some((key, price)) = self.models.get_key_value(candidate) {
                return Some((key.as_str(), price));
            }
        }
        // A provider the library has no name for (a Claude account, a custom
        // provider id): the model's own family names the library prefix.
        let family = candidates.iter().find_map(|candidate| category_for_model_name(candidate))?;
        for candidate in &candidates {
            for prefix in category_prefixes(family) {
                if prefixes.iter().any(|tried| tried == prefix) {
                    continue;
                }
                if let Some((key, price)) = self.models.get_key_value(&format!("{prefix}/{candidate}")) {
                    return Some((key.as_str(), price));
                }
            }
        }
        None
    }
}

/// Provider family a model name belongs to (`glm-5.3`, `kimi-k2`,
/// `deepseek-chat`, …); `None` for names that do not say.
pub fn category_for_model_name(model: &str) -> Option<ProviderCategory> {
    let model = model.trim().to_ascii_lowercase();
    let starts = |prefixes: &[&str]| prefixes.iter().any(|prefix| model.starts_with(prefix));
    if starts(&["glm-", "chatglm"]) {
        Some(ProviderCategory::Glm)
    } else if starts(&["kimi-", "moonshot-"]) {
        Some(ProviderCategory::Kimi)
    } else if starts(&["deepseek-"]) {
        Some(ProviderCategory::Deepseek)
    } else if starts(&["claude-"]) {
        Some(ProviderCategory::Claude)
    } else if starts(&["gpt-", "codex-"]) {
        Some(ProviderCategory::Codex)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Cache files

pub fn cache_dir() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::runtime_dir()?.join("usage-pricing"))
}

pub fn catalog_file(dir: &Path, source: PriceSource) -> PathBuf {
    dir.join(format!("catalog-{}.json", source.key()))
}

pub fn fx_file(dir: &Path) -> PathBuf {
    dir.join("fx.json")
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec(value)?;
    crate::config::atomic::write_private_bytes_atomic(path, &bytes)
}

pub fn load_catalog_at(path: &Path) -> Option<CatalogCache> {
    read_json::<CatalogCache>(path).filter(|cache| cache.version == CACHE_VERSION)
}

pub fn load_fx_at(path: &Path) -> Option<FxCache> {
    read_json::<FxCache>(path).filter(|cache| cache.version == CACHE_VERSION)
}

type Memo = Option<(PathBuf, SystemTime, u64, Arc<CatalogCache>)>;
static CATALOG_MEMO: Mutex<Memo> = Mutex::new(None);

/// The cached library, re-parsed only when the file changed.
pub fn load_catalog_cached(path: &Path) -> Option<Arc<CatalogCache>> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let mut memo = CATALOG_MEMO.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some((memo_path, memo_modified, memo_len, cache)) = memo.as_ref() {
        if memo_path == path && *memo_modified == modified && *memo_len == meta.len() {
            return Some(cache.clone());
        }
    }
    let cache = Arc::new(load_catalog_at(path)?);
    *memo = Some((path.to_path_buf(), modified, meta.len(), cache.clone()));
    Some(cache)
}

// ---------------------------------------------------------------------------
// Fetching

#[derive(Debug, Clone, PartialEq)]
pub struct FetchResponse {
    /// 200 (with body) or 304.
    pub status: u16,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub body: Vec<u8>,
}

/// HTTP GET with optional conditional headers (injectable for tests).
pub trait Fetcher {
    fn get(&self, url: &str, etag: Option<&str>, last_modified: Option<&str>) -> anyhow::Result<FetchResponse>;
}

pub struct HttpFetcher;

impl Fetcher for HttpFetcher {
    fn get(&self, url: &str, etag: Option<&str>, last_modified: Option<&str>) -> anyhow::Result<FetchResponse> {
        let agent = crate::config::proxy_outbound::OutboundProxy::host()
            .apply(
                ureq::Agent::config_builder()
                    .timeout_global(Some(std::time::Duration::from_secs(90)))
                    .max_redirects(3),
            )?
            .build()
            .new_agent();
        let mut request = agent
            .get(url)
            .header("Accept", "application/json")
            .header("User-Agent", concat!("cutex/", env!("CARGO_PKG_VERSION")));
        if let Some(etag) = etag {
            request = request.header("If-None-Match", etag);
        }
        if let Some(last_modified) = last_modified {
            request = request.header("If-Modified-Since", last_modified);
        }
        let mut response = request.call().map_err(|error| {
            if let Some(message) = crate::config::proxy_outbound::proxy_error_message(&error) {
                return anyhow::anyhow!("connection failed: {message}");
            }
            match error {
                ureq::Error::StatusCode(status) => anyhow::anyhow!("HTTP {status}"),
                other => anyhow::anyhow!("connection failed: {other}"),
            }
        })?;
        let status = response.status().as_u16();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        };
        let etag = header("etag");
        let last_modified = header("last-modified");
        if status == 304 {
            return Ok(FetchResponse { status, etag, last_modified, body: Vec::new() });
        }
        anyhow::ensure!(response.status().is_success(), "HTTP {status}");
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_BODY_BYTES)
            .read_to_vec()
            .map_err(|error| anyhow::anyhow!("reading the response failed: {error}"))?;
        Ok(FetchResponse { status, etag, last_modified, body })
    }
}

fn timestamp(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn hours_since(at: Option<&str>, now: DateTime<Utc>) -> Option<i64> {
    let at = DateTime::parse_from_rfc3339(at?).ok()?.with_timezone(&Utc);
    Some((now - at).num_minutes() / 60)
}

/// Whether a cache checked at `checked_at` (with `last_error`) is due.
fn due(checked_at: Option<&str>, failed: bool, interval_hours: i64, now: DateTime<Utc>) -> bool {
    let interval = if failed { interval_hours.min(FAILURE_RETRY_HOURS) } else { interval_hours };
    hours_since(checked_at, now).is_none_or(|hours| hours >= interval || hours < 0)
}

pub fn catalog_due(dir: &Path, settings: &EffectiveUsageSettings, now: DateTime<Utc>) -> bool {
    let Some(url) = settings.catalog_url() else { return false };
    match load_catalog_at(&catalog_file(dir, settings.price_source)) {
        Some(cache) if cache.url == url => due(
            cache.checked_at.as_deref(),
            cache.last_error.is_some(),
            i64::from(settings.catalog_refresh_hours),
            now,
        ),
        _ => true,
    }
}

pub fn fx_due(dir: &Path, now: DateTime<Utc>) -> bool {
    match load_fx_at(&fx_file(dir)) {
        Some(cache) => due(cache.checked_at.as_deref(), cache.last_error.is_some(), FX_REFRESH_HOURS, now),
        None => true,
    }
}

/// Refresh one library cache. Failures keep the previous models and are
/// recorded in `lastError` (also returned as `Err`).
pub fn refresh_catalog_at(
    dir: &Path,
    source: PriceSource,
    url: &str,
    fetcher: &dyn Fetcher,
    now: DateTime<Utc>,
) -> anyhow::Result<CatalogCache> {
    let path = catalog_file(dir, source);
    let mut cache = load_catalog_at(&path).unwrap_or_default();
    let same_url = cache.url == url && !cache.models.is_empty();
    let result = fetcher
        .get(
            url,
            same_url.then_some(cache.etag.as_deref()).flatten(),
            same_url.then_some(cache.last_modified.as_deref()).flatten(),
        )
        .and_then(|response| {
            if response.status == 304 {
                anyhow::ensure!(same_url, "HTTP 304 without a cached copy");
                return Ok((response, None));
            }
            let models = parse_library(source, &response.body)?;
            Ok((response, Some(models)))
        });
    cache.version = CACHE_VERSION;
    cache.source = source.key().to_string();
    cache.checked_at = Some(timestamp(now));
    let outcome = match result {
        Ok((response, models)) => {
            if let Some(models) = models {
                cache.models = models;
                cache.fetched_at = Some(timestamp(now));
            }
            cache.url = url.to_string();
            cache.etag = response.etag.or(cache.etag.take().filter(|_| same_url));
            cache.last_modified = response.last_modified.or(cache.last_modified.take().filter(|_| same_url));
            cache.last_error = None;
            Ok(())
        }
        Err(error) => {
            if !same_url && cache.url != url {
                // Keep the old URL's models (still the best estimate) but
                // record that the configured URL failed.
                cache.etag = None;
                cache.last_modified = None;
            }
            cache.last_error = Some(format!("{url}: {error:#}"));
            Err(error)
        }
    };
    write_json(&path, &cache)?;
    outcome.map(|()| cache)
}

pub fn refresh_fx_at(dir: &Path, url: &str, fetcher: &dyn Fetcher, now: DateTime<Utc>) -> anyhow::Result<FxCache> {
    let path = fx_file(dir);
    let mut cache = load_fx_at(&path).unwrap_or_default();
    let result = fetcher.get(url, None, None).and_then(|response| parse_fx(&response.body));
    cache.version = CACHE_VERSION;
    cache.url = url.to_string();
    cache.checked_at = Some(timestamp(now));
    let outcome = match result {
        Ok((date, rates)) => {
            cache.date = date;
            cache.rates = rates;
            cache.fetched_at = Some(timestamp(now));
            cache.last_error = None;
            Ok(())
        }
        Err(error) => {
            cache.last_error = Some(format!("{error:#}"));
            Err(error)
        }
    };
    write_json(&path, &cache)?;
    outcome.map(|()| cache)
}

/// Rates for conversions: fetched (when auto-fetch is on and a fetch ever
/// succeeded) else the configured ones. Returns `(rates, source, fx cache)`.
pub fn effective_rates(dir: &Path, settings: &EffectiveUsageSettings) -> (ExchangeRates, &'static str, Option<FxCache>) {
    if settings.exchange_rate_auto_fetch {
        if let Some(fx) = load_fx_at(&fx_file(dir)) {
            if fx.fetched_at.is_some() && !fx.rates.is_empty() {
                return (ExchangeRates::from_map(&fx.rates, settings.exchange_rates), "auto", Some(fx));
            }
            return (settings.exchange_rates, "config", Some(fx));
        }
    }
    (settings.exchange_rates, "config", None)
}

/// One refresher pass: due library and (opt-in) rates.
pub fn refresh_due(dir: &Path, settings: &EffectiveUsageSettings, fetcher: &dyn Fetcher, force: bool, now: DateTime<Utc>) -> Vec<String> {
    let mut errors = Vec::new();
    if let Some(url) = settings.catalog_url() {
        if force || catalog_due(dir, settings, now) {
            if let Err(error) = refresh_catalog_at(dir, settings.price_source, &url, fetcher, now) {
                errors.push(format!("price library {}: {error:#}", settings.price_source.key()));
            }
        }
    }
    if settings.exchange_rate_auto_fetch && (force || fx_due(dir, now)) {
        if let Err(error) = refresh_fx_at(dir, FX_URL, fetcher, now) {
            errors.push(format!("exchange rates: {error:#}"));
        }
    }
    errors
}

static REFRESHER_STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
const REFRESHER_FIRST_DELAY: std::time::Duration = std::time::Duration::from_secs(120);
const REFRESHER_TICK: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Daemon background refresher (idempotent). Sleeps between passes; a pass
/// reads one small settings file and the cache metadata, and fetches only
/// when something is due.
pub fn start_refresher() {
    if REFRESHER_STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("cutex-usage-prices".to_string())
        .spawn(|| {
            std::thread::sleep(REFRESHER_FIRST_DELAY);
            loop {
                if let Ok(dir) = cache_dir() {
                    let settings = crate::config::usage_pricing::current();
                    for error in refresh_due(&dir, &settings, &HttpFetcher, false, Utc::now()) {
                        eprintln!("usage prices: {error}");
                    }
                }
                std::thread::sleep(REFRESHER_TICK);
            }
        });
    if let Err(error) = spawned {
        eprintln!("usage prices: could not start refresher: {error}");
    }
}

#[cfg(test)]
#[path = "catalog_tests.rs"]
mod tests;
