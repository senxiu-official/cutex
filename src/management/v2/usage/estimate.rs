//! Per-sample cost estimate for `GET /v2/usage`: manual price table, then
//! the cached price library, then the built-in API rate cards; converted
//! into the display currency (capabilities `usage-price-catalog`,
//! `usage-currency`; docs/usage-pricing.md).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde::Serialize;
use serde_json::json;
use serde_json::Value;

use crate::config::usage_pricing::Currency;
use crate::config::usage_pricing::EffectiveUsageSettings;
use crate::config::usage_pricing::ExchangeRates;
use crate::config::usage_pricing::PriceSource;
use crate::profiles::provider_category::ProviderCategory;

use super::catalog;
use super::catalog::CatalogCache;
use super::catalog::FxCache;
use super::model::UsageAttribution;
use super::model::UsageSample;
use super::prices::estimate_cost;
use super::prices::token_cost;
use super::prices::UsagePriceTable;
use super::pricing::api_tier_multiplier;
use super::pricing::derive_sample_estimates;

/// Where a sample's price came from (`priceSources` in reports).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum PriceOrigin {
    #[serde(rename = "manual")]
    Manual,
    #[serde(rename = "litellm")]
    Litellm,
    #[serde(rename = "models.dev")]
    ModelsDev,
    /// cutex's built-in API rate cards (`apiEquivalentUsd`).
    #[serde(rename = "builtin")]
    Builtin,
}

impl PriceOrigin {
    pub fn key(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Litellm => "litellm",
            Self::ModelsDev => "models.dev",
            Self::Builtin => "builtin",
        }
    }

    fn library(source: PriceSource) -> Option<Self> {
        match source {
            PriceSource::Litellm => Some(Self::Litellm),
            PriceSource::ModelsDev => Some(Self::ModelsDev),
            PriceSource::Manual => None,
        }
    }
}

pub struct PriceContext {
    pub manual: Option<UsagePriceTable>,
    manual_index: HashMap<String, String>,
    /// Lowercased `from` → `to` (`modelMap`, capability `usage-model-map`).
    model_map: HashMap<String, String>,
    pub library: Option<Arc<CatalogCache>>,
    pub source: PriceSource,
    pub display: Currency,
    pub rates: ExchangeRates,
    /// Profile name → effective provider category (library key prefixes
    /// when the sample's provider name says nothing).
    pub profile_categories: HashMap<String, ProviderCategory>,
    /// Report metadata (`pricing` object).
    pub info: Value,
}

impl PriceContext {
    /// Context from explicit parts (tests, callers with their own data).
    pub fn new(
        settings: &EffectiveUsageSettings,
        manual: Option<UsagePriceTable>,
        library: Option<Arc<CatalogCache>>,
        rates: ExchangeRates,
    ) -> Self {
        let library = library.filter(|_| settings.price_source != PriceSource::Manual);
        let model_map = manual.as_ref().map(UsagePriceTable::model_map_index).unwrap_or_default();
        let manual = manual.filter(|table| !table.models.is_empty());
        let manual_index = manual.as_ref().map(UsagePriceTable::index).unwrap_or_default();
        Self {
            manual,
            manual_index,
            model_map,
            library,
            source: settings.price_source,
            display: settings.display_currency,
            rates,
            profile_categories: HashMap::new(),
            info: Value::Null,
        }
    }

    /// Context from the daemon's settings and caches. Returns warnings.
    pub fn load_at(
        cache_dir: &Path,
        settings: &EffectiveUsageSettings,
        manual: Option<UsagePriceTable>,
    ) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let library = (settings.price_source != PriceSource::Manual)
            .then(|| catalog::load_catalog_cached(&catalog::catalog_file(cache_dir, settings.price_source)))
            .flatten();
        if settings.price_source != PriceSource::Manual {
            match &library {
                None => warnings.push(format!(
                    "price library {} not downloaded yet; built-in and manual prices only",
                    settings.price_source.key()
                )),
                Some(cache) if cache.models.is_empty() => warnings.push(format!(
                    "price library {} unavailable ({}); built-in and manual prices only",
                    settings.price_source.key(),
                    cache.last_error.as_deref().unwrap_or("no data")
                )),
                Some(_) => {}
            }
        }
        let (rates, rates_source, fx) = catalog::effective_rates(cache_dir, settings);
        let mut context = Self::new(settings, manual, library, rates);
        context.info = pricing_info(settings, context.library.as_deref(), &rates, rates_source, fx.as_ref());
        (context, warnings)
    }

    /// Model a sample is priced as through `modelMap`: the sample's pricing
    /// model (configuration id), then its display model, each tried as
    /// `<provider>/<model>` and through the normalized forms
    /// (`catalog::model_candidates`), like manual keys. One hop only.
    pub fn mapped_model(&self, sample: &UsageSample) -> Option<&str> {
        if self.model_map.is_empty() {
            return None;
        }
        let attribution = &sample.attribution;
        let provider = attribution.provider.as_deref().map(|p| p.trim().to_ascii_lowercase());
        let mut models = vec![attribution.pricing_model()?];
        if let Some(model) = attribution.model.as_deref() {
            if !models.contains(&model) {
                models.push(model);
            }
        }
        for model in models {
            let mut names = Vec::new();
            if let Some(provider) = &provider {
                names.push(format!("{provider}/{}", model.trim().to_ascii_lowercase()));
            }
            for candidate in catalog::model_candidates(model) {
                if let Some(provider) = &provider {
                    names.push(format!("{provider}/{candidate}"));
                }
                names.push(candidate);
            }
            if let Some(to) = names.iter().find_map(|name| self.model_map.get(name)) {
                return Some(to.as_str());
            }
        }
        None
    }

    /// Cost of one sample in the display currency and its origin; a mapped
    /// sample (`mapped_model`) is priced as its target model.
    pub fn price(&self, sample: &UsageSample) -> Option<(f64, PriceOrigin)> {
        self.price_mapped(sample).0
    }

    /// `price` plus the `modelMap` target the sample was priced as.
    pub fn price_mapped(&self, sample: &UsageSample) -> (Option<(f64, PriceOrigin)>, Option<&str>) {
        match self.mapped_model(sample) {
            Some(to) => {
                let mut attribution = sample.attribution.clone();
                attribution.model = Some(to.to_string());
                attribution.request_model = None;
                (self.price_as(&attribution, sample), Some(to))
            }
            None => (self.price_as(&sample.attribution, sample), None),
        }
    }

    fn price_as(&self, attribution: &UsageAttribution, sample: &UsageSample) -> Option<(f64, PriceOrigin)> {
        let model = attribution.pricing_model()?;
        if let Some(table) = &self.manual {
            // An explicit configuration price wins, but its normalized display
            // name must not accidentally select an unrelated model's price.
            let configured = attribution.request_model.as_ref().and_then(|_| {
                table.exact_price_for_indexed(&self.manual_index, attribution.provider.as_deref(), attribution.model.as_deref())
            });
            if let Some((_, price)) = configured.or_else(|| table.price_for_indexed(&self.manual_index, attribution.provider.as_deref(), Some(model))) {
                if let Some(currency) = table.entry_currency(price) {
                    let cost = estimate_cost(price, &sample.tokens);
                    return Some((self.rates.convert(cost, currency, self.display), PriceOrigin::Manual));
                }
            }
        }
        if let (Some(library), Some(origin)) = (&self.library, PriceOrigin::library(self.source)) {
            let category = sample.profile.as_ref().and_then(|profile| self.profile_categories.get(profile)).copied();
            if let Some((_, price)) = library.lookup(attribution.provider.as_deref(), category, model) {
                if let Some(multiplier) =
                    api_tier_multiplier(attribution.provider.as_deref(), model, attribution.service_tier.as_deref())
                {
                    let long = price
                        .long_context
                        .as_ref()
                        .filter(|long| sample.tokens.input_tokens > long.above_tokens);
                    let usd = match long {
                        Some(long) => token_cost(&sample.tokens, long.input, long.cached_input, long.cache_write_input, long.output),
                        None => token_cost(&sample.tokens, price.input, price.cached_input, price.cache_write_input, price.output),
                    } * multiplier;
                    return Some((self.rates.convert(usd, Currency::Usd, self.display), origin));
                }
            }
        }
        if self.source != PriceSource::Manual {
            if let Some(nanos) = derive_sample_estimates(attribution, &sample.tokens).api_equivalent_usd_nanos {
                let usd = nanos as f64 / 1e9;
                return Some((self.rates.convert(usd, Currency::Usd, self.display), PriceOrigin::Builtin));
            }
        }
        None
    }
}

/// `pricing` object of the usage report / prices resource.
pub fn pricing_info(
    settings: &EffectiveUsageSettings,
    library: Option<&CatalogCache>,
    rates: &ExchangeRates,
    rates_source: &str,
    fx: Option<&FxCache>,
) -> Value {
    json!({
        "priceSource": settings.price_source.key(),
        "displayCurrency": settings.display_currency.code(),
        "exchangeRates": rates.view(),
        "exchangeRatesSource": rates_source,
        "exchangeRatesAutoFetch": settings.exchange_rate_auto_fetch,
        "exchangeRatesFetchedAt": fx.and_then(|fx| fx.fetched_at.clone()),
        "exchangeRatesDate": fx.and_then(|fx| fx.date.clone()),
        "exchangeRatesError": fx.and_then(|fx| fx.last_error.clone()),
        "catalog": library.map(|cache| json!({
            "source": cache.source,
            "url": cache.url,
            "fetchedAt": cache.fetched_at,
            "checkedAt": cache.checked_at,
            "lastError": cache.last_error,
            "modelCount": cache.models.len(),
        })),
    })
}
