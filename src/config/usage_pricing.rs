//! Usage cost estimate settings (`config.json` `usage`, `/v2/settings`
//! `usage`; capabilities `usage-price-catalog`, `usage-currency`).
//! See docs/usage-pricing.md.
//!
//! Values are stored as plain strings / numbers and read leniently
//! ([`UsageSettings::effective`]) so a config written by a newer cutex with
//! a value this build does not know never makes `config.json` unreadable.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Library the daemon fetches model prices from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PriceSource {
    Litellm,
    ModelsDev,
    /// Only the owner's price table.
    Manual,
}

impl PriceSource {
    pub const ALL: [Self; 3] = [Self::Litellm, Self::ModelsDev, Self::Manual];

    pub fn key(self) -> &'static str {
        match self {
            Self::Litellm => "litellm",
            Self::ModelsDev => "models.dev",
            Self::Manual => "manual",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|source| source.key() == value)
    }

    /// Public URL of the library (`None` for manual).
    pub fn default_url(self) -> Option<&'static str> {
        match self {
            Self::Litellm => Some(
                "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json",
            ),
            Self::ModelsDev => Some("https://models.dev/api.json"),
            Self::Manual => None,
        }
    }
}

/// Currencies the estimates can be shown in and manual prices entered in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Currency {
    Usd,
    Aud,
    Cny,
}

impl Currency {
    pub const ALL: [Self; 3] = [Self::Usd, Self::Aud, Self::Cny];

    pub fn code(self) -> &'static str {
        match self {
            Self::Usd => "USD",
            Self::Aud => "AUD",
            Self::Cny => "CNY",
        }
    }

    /// ISO code, ASCII case-insensitive.
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|currency| currency.code().eq_ignore_ascii_case(value.trim()))
    }
}

/// Units of each currency per 1 USD.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExchangeRates {
    pub aud: f64,
    pub cny: f64,
}

/// Owner-editable defaults (approximate, early October 2026).
pub const DEFAULT_AUD_PER_USD: f64 = 1.52;
pub const DEFAULT_CNY_PER_USD: f64 = 7.12;
const MAX_RATE: f64 = 1_000_000.0;

impl Default for ExchangeRates {
    fn default() -> Self {
        Self { aud: DEFAULT_AUD_PER_USD, cny: DEFAULT_CNY_PER_USD }
    }
}

impl ExchangeRates {
    pub fn per_usd(&self, currency: Currency) -> f64 {
        match currency {
            Currency::Usd => 1.0,
            Currency::Aud => self.aud,
            Currency::Cny => self.cny,
        }
    }

    pub fn convert(&self, amount: f64, from: Currency, to: Currency) -> f64 {
        if from == to {
            return amount;
        }
        amount / self.per_usd(from) * self.per_usd(to)
    }

    /// `{"USD": 1, "AUD": .., "CNY": ..}`.
    pub fn view(&self) -> Value {
        json!({"USD": 1.0, "AUD": self.aud, "CNY": self.cny})
    }

    fn valid(rate: f64) -> bool {
        rate.is_finite() && rate > 0.0 && rate <= MAX_RATE
    }

    /// Valid rates from a `{code: rate}` map; missing / invalid fall back.
    pub fn from_map(map: &BTreeMap<String, f64>, fallback: Self) -> Self {
        let pick = |code: &str, fallback: f64| {
            map.iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(code))
                .map(|(_, rate)| *rate)
                .filter(|rate| Self::valid(*rate))
                .unwrap_or(fallback)
        };
        Self { aud: pick("AUD", fallback.aud), cny: pick("CNY", fallback.cny) }
    }
}

pub const DEFAULT_CATALOG_REFRESH_HOURS: u32 = 24;
pub const CATALOG_REFRESH_HOURS_RANGE: (u32, u32) = (1, 720);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UsageSettings {
    /// `litellm` (default) | `models.dev` | `manual`.
    pub price_source: String,
    /// `USD` (default) | `AUD` | `CNY`.
    pub display_currency: String,
    /// Units per 1 USD: `{"AUD": 1.52, "CNY": 7.12}`.
    pub exchange_rates: BTreeMap<String, f64>,
    /// Fetch rates daily (Frankfurter / ECB) instead of using
    /// `exchangeRates`; off by default.
    pub exchange_rate_auto_fetch: bool,
    /// Library refresh interval.
    pub catalog_refresh_hours: u32,
    /// Mirror URL for the selected library (same JSON format).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog_url: Option<String>,
    /// Extra usage sources: token usage of Claude Code / Codex runs outside
    /// cutex (`/v2/usage/sources`, management/v2/usage/external.rs). Not part
    /// of the `/v2/settings` view or patch.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extra_sources: Vec<ExtraUsageSource>,
}

/// One `usage.extraSources` entry, stored as plain strings and read
/// leniently (an unknown kind / billing from a newer cutex is skipped, never
/// makes `config.json` unreadable).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExtraUsageSource {
    pub id: String,
    /// `claude-code` | `codex`.
    pub kind: String,
    pub path: String,
    pub label: String,
    /// `subscription` (default) | `api`.
    pub billing: String,
    pub enabled: bool,
}

impl Default for ExtraUsageSource {
    fn default() -> Self {
        Self {
            id: String::new(),
            kind: String::new(),
            path: String::new(),
            label: String::new(),
            billing: "subscription".into(),
            enabled: true,
        }
    }
}

impl Default for UsageSettings {
    fn default() -> Self {
        Self {
            price_source: PriceSource::Litellm.key().into(),
            display_currency: Currency::Usd.code().into(),
            exchange_rates: [
                ("AUD".to_string(), DEFAULT_AUD_PER_USD),
                ("CNY".to_string(), DEFAULT_CNY_PER_USD),
            ]
            .into(),
            exchange_rate_auto_fetch: false,
            catalog_refresh_hours: DEFAULT_CATALOG_REFRESH_HOURS,
            catalog_url: None,
            extra_sources: Vec::new(),
        }
    }
}

/// Parsed, valid settings.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectiveUsageSettings {
    pub price_source: PriceSource,
    pub display_currency: Currency,
    pub exchange_rates: ExchangeRates,
    pub exchange_rate_auto_fetch: bool,
    pub catalog_refresh_hours: u32,
    pub catalog_url: Option<String>,
}

impl EffectiveUsageSettings {
    /// URL fetched for the selected library (`None` for manual).
    pub fn catalog_url(&self) -> Option<String> {
        self.price_source.default_url()?;
        self.catalog_url
            .clone()
            .or_else(|| self.price_source.default_url().map(str::to_string))
    }
}

fn http_url(value: &str) -> Result<String, String> {
    let text = value.trim();
    let url = url::Url::parse(text).map_err(|error| format!("invalid URL: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("must be an http(s) URL with a host".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("must not embed credentials".into());
    }
    Ok(text.to_string())
}

impl UsageSettings {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Unknown / invalid hand-edited values fall back field by field.
    pub fn effective(&self) -> EffectiveUsageSettings {
        let (min, max) = CATALOG_REFRESH_HOURS_RANGE;
        EffectiveUsageSettings {
            price_source: PriceSource::parse(&self.price_source).unwrap_or(PriceSource::Litellm),
            display_currency: Currency::parse(&self.display_currency).unwrap_or(Currency::Usd),
            exchange_rates: ExchangeRates::from_map(&self.exchange_rates, ExchangeRates::default()),
            exchange_rate_auto_fetch: self.exchange_rate_auto_fetch,
            catalog_refresh_hours: if (min..=max).contains(&self.catalog_refresh_hours) {
                self.catalog_refresh_hours
            } else {
                DEFAULT_CATALOG_REFRESH_HOURS
            },
            catalog_url: self.catalog_url.as_deref().and_then(|url| http_url(url).ok()),
        }
    }

    /// Normalized stored form of effective settings.
    fn from_effective(settings: &EffectiveUsageSettings) -> Self {
        Self {
            price_source: settings.price_source.key().into(),
            display_currency: settings.display_currency.code().into(),
            exchange_rates: [
                ("AUD".to_string(), settings.exchange_rates.aud),
                ("CNY".to_string(), settings.exchange_rates.cny),
            ]
            .into(),
            exchange_rate_auto_fetch: settings.exchange_rate_auto_fetch,
            catalog_refresh_hours: settings.catalog_refresh_hours,
            catalog_url: settings.catalog_url.clone(),
            extra_sources: Vec::new(),
        }
    }

    /// Apply a partial `/v2/settings` `usage` object (strict).
    pub fn patched(&self, patch: &Value) -> Result<Self, String> {
        let object = patch.as_object().ok_or("usage: must be an object")?;
        let mut next = self.effective();
        for (key, value) in object {
            match key.as_str() {
                "priceSource" => {
                    next.price_source = value
                        .as_str()
                        .and_then(PriceSource::parse)
                        .ok_or("usage.priceSource: must be litellm, models.dev or manual")?;
                }
                "displayCurrency" => {
                    next.display_currency = value
                        .as_str()
                        .and_then(Currency::parse)
                        .ok_or("usage.displayCurrency: must be USD, AUD or CNY")?;
                }
                "exchangeRates" => {
                    let rates = value.as_object().ok_or("usage.exchangeRates: must be an object")?;
                    for (code, rate) in rates {
                        let currency = Currency::parse(code)
                            .ok_or(format!("usage.exchangeRates: unsupported currency {code}"))?;
                        let rate = rate.as_f64().filter(|rate| ExchangeRates::valid(*rate)).ok_or(
                            format!("usage.exchangeRates.{code}: must be a number > 0 and <= {MAX_RATE}"),
                        )?;
                        match currency {
                            Currency::Usd if (rate - 1.0).abs() > f64::EPSILON => {
                                return Err("usage.exchangeRates.USD: must be 1 (rates are per USD)".into())
                            }
                            Currency::Usd => {}
                            Currency::Aud => next.exchange_rates.aud = rate,
                            Currency::Cny => next.exchange_rates.cny = rate,
                        }
                    }
                }
                "exchangeRateAutoFetch" => {
                    next.exchange_rate_auto_fetch =
                        value.as_bool().ok_or("usage.exchangeRateAutoFetch: must be a boolean")?;
                }
                "catalogRefreshHours" => {
                    let (min, max) = CATALOG_REFRESH_HOURS_RANGE;
                    next.catalog_refresh_hours = value
                        .as_u64()
                        .filter(|hours| (u64::from(min)..=u64::from(max)).contains(hours))
                        .ok_or(format!("usage.catalogRefreshHours: must be {min}-{max}"))?
                        as u32;
                }
                "catalogUrl" => {
                    next.catalog_url = match value {
                        Value::Null => None,
                        Value::String(text) if text.trim().is_empty() => None,
                        Value::String(text) => {
                            Some(http_url(text).map_err(|message| format!("usage.catalogUrl: {message}"))?)
                        }
                        _ => return Err("usage.catalogUrl: must be a string or null".into()),
                    };
                }
                other => return Err(format!("usage: unsupported field {other}")),
            }
        }
        // Extra sources are edited through /v2/usage/sources only.
        Ok(Self { extra_sources: self.extra_sources.clone(), ..Self::from_effective(&next) })
    }

    /// `/v2/settings` view: `{usage, usageDefault, usageOptions}`.
    pub fn view(&self) -> Value {
        let (min, max) = CATALOG_REFRESH_HOURS_RANGE;
        json!({
            "usage": Self::from_effective(&self.effective()),
            "usageDefault": Self::default(),
            "usageOptions": {
                "priceSource": PriceSource::ALL.map(PriceSource::key),
                "displayCurrency": Currency::ALL.map(Currency::code),
                "catalogRefreshHours": [min, max],
            },
        })
    }
}

/// The current effective settings from `config.json`.
pub fn current() -> EffectiveUsageSettings {
    crate::config::store::load_codez_config().usage.effective()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_lenient_reads_and_strict_patches() {
        let settings = UsageSettings::default();
        assert!(settings.is_default());
        let effective = settings.effective();
        assert_eq!(effective.price_source, PriceSource::Litellm);
        assert_eq!(effective.display_currency, Currency::Usd);
        assert!(!effective.exchange_rate_auto_fetch);
        assert!(effective.catalog_url().unwrap().contains("litellm"));

        // A newer cutex's unknown values still parse and fall back.
        let future: UsageSettings = serde_json::from_value(json!({
            "priceSource": "openrouter", "displayCurrency": "EUR",
            "exchangeRates": {"AUD": -1, "CNY": 7.0, "EUR": 0.9}, "catalogRefreshHours": 0,
            "somethingNew": true
        }))
        .unwrap();
        let effective = future.effective();
        assert_eq!(effective.price_source, PriceSource::Litellm);
        assert_eq!(effective.display_currency, Currency::Usd);
        assert_eq!(effective.exchange_rates, ExchangeRates { aud: DEFAULT_AUD_PER_USD, cny: 7.0 });
        assert_eq!(effective.catalog_refresh_hours, DEFAULT_CATALOG_REFRESH_HOURS);

        let patched = settings
            .patched(&json!({"priceSource": "models.dev", "displayCurrency": "aud",
                "exchangeRates": {"AUD": 1.6}, "catalogRefreshHours": 6,
                "catalogUrl": "https://mirror.example/api.json"}))
            .unwrap();
        let effective = patched.effective();
        assert_eq!(effective.price_source, PriceSource::ModelsDev);
        assert_eq!(effective.display_currency, Currency::Aud);
        assert_eq!(effective.exchange_rates.aud, 1.6);
        assert_eq!(effective.exchange_rates.cny, DEFAULT_CNY_PER_USD);
        assert_eq!(effective.catalog_url().as_deref(), Some("https://mirror.example/api.json"));
        let manual = patched.patched(&json!({"priceSource": "manual"})).unwrap();
        assert_eq!(manual.effective().catalog_url(), None);
        for bad in [
            json!({"priceSource": "openrouter"}),
            json!({"displayCurrency": "EUR"}),
            json!({"exchangeRates": {"AUD": 0}}),
            json!({"exchangeRates": {"EUR": 1}}),
            json!({"exchangeRates": {"USD": 2}}),
            json!({"catalogRefreshHours": 0}),
            json!({"catalogUrl": "ftp://x"}),
            json!({"bogus": 1}),
        ] {
            assert!(settings.patched(&bad).is_err(), "{bad}");
        }
        let view = settings.view();
        assert_eq!(view["usage"]["priceSource"], "litellm");
        assert_eq!(view["usageOptions"]["displayCurrency"], json!(["USD", "AUD", "CNY"]));
    }

    #[test]
    fn currency_conversion_goes_through_usd() {
        let rates = ExchangeRates { aud: 1.5, cny: 7.0 };
        assert_eq!(rates.convert(10.0, Currency::Usd, Currency::Usd), 10.0);
        assert!((rates.convert(10.0, Currency::Usd, Currency::Aud) - 15.0).abs() < 1e-12);
        assert!((rates.convert(70.0, Currency::Cny, Currency::Usd) - 10.0).abs() < 1e-12);
        assert!((rates.convert(70.0, Currency::Cny, Currency::Aud) - 15.0).abs() < 1e-12);
        assert_eq!(Currency::parse(" cny "), Some(Currency::Cny));
        assert_eq!(Currency::parse("EUR"), None);
    }
}
