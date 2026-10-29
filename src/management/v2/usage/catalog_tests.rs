//! Price library parsing, lookup, cache fallback and the per-sample
//! estimate. Fixtures only; no network.

use std::cell::RefCell;
use std::collections::HashMap;

use chrono::TimeZone;
use serde_json::json;

use super::*;
use crate::config::usage_pricing::UsageSettings;
use crate::management::v2::usage::estimate::PriceContext;
use crate::management::v2::usage::estimate::PriceOrigin;
use crate::management::v2::usage::model::TokenUsageBreakdown;
use crate::management::v2::usage::model::UsageAttribution;
use crate::management::v2::usage::model::UsageSample;
use crate::management::v2::usage::model::UsageTokenSource;
use crate::management::v2::usage::prices::UsagePriceTable;

fn litellm_fixture() -> serde_json::Value {
    json!({
        "sample_spec": { "input_cost_per_token": 0.0, "output_cost_per_token": 0.0, "mode": "chat" },
        "gpt-5.5": {
            "input_cost_per_token": 5e-06, "cache_read_input_token_cost": 5e-07,
            "output_cost_per_token": 3e-05, "litellm_provider": "openai", "mode": "chat",
            "input_cost_per_token_above_272k_tokens": 1e-05,
            "output_cost_per_token_above_272k_tokens": 4.5e-05,
            "cache_read_input_token_cost_above_272k_tokens": 1e-06
        },
        "deepseek/deepseek-chat": {
            "input_cost_per_token": 2.8e-07, "cache_read_input_token_cost": 2.8e-08,
            "output_cost_per_token": 4.2e-07, "litellm_provider": "deepseek", "mode": "chat"
        },
        "claude-sonnet-4-5": {
            "input_cost_per_token": 3e-06, "cache_read_input_token_cost": 3e-07,
            "cache_creation_input_token_cost": 3.75e-06, "output_cost_per_token": 1.5e-05,
            "litellm_provider": "anthropic", "mode": "chat"
        },
        "zai/glm-5.3": { "input_cost_per_token": 1.4e-06, "output_cost_per_token": 4.4e-06, "mode": "chat" },
        "text-embedding-3-small": { "input_cost_per_token": 2e-08, "output_cost_per_token": 0.0, "mode": "embedding" },
        "broken": { "input_cost_per_token": "x" }
    })
}

fn models_dev_fixture() -> serde_json::Value {
    json!({
        "openrouter": { "id": "openrouter", "models": {
            "gpt-5.5": { "id": "gpt-5.5", "cost": { "input": 9.0, "output": 99.0 } }
        }},
        "openai": { "id": "openai", "models": {
            "gpt-5.5": { "id": "gpt-5.5", "cost": { "input": 5.0, "cache_read": 0.5, "output": 30.0 } }
        }},
        "moonshotai": { "id": "moonshotai", "models": {
            "kimi-k3": { "id": "kimi-k3", "cost": { "input": 3.0, "cache_read": 0.3, "output": 15.0,
                "context_over_200k": { "input": 6.0, "output": 22.5 } } }
        }},
        "zhipuai": { "id": "zhipuai", "models": {
            "glm-5.3": { "id": "glm-5.3", "cost": { "input": 0.6, "output": 2.2 } }
        }},
        "zai": { "id": "zai", "models": {
            "glm-5.3": { "id": "glm-5.3", "cost": { "input": 1.4, "output": 4.4 } }
        }},
        "nocost": { "id": "nocost", "models": { "m": { "id": "m" } } }
    })
}

#[test]
fn litellm_is_normalized_to_per_million_usd() {
    let models = parse_litellm(&litellm_fixture()).unwrap();
    assert_eq!(models.len(), 4, "{:?}", models.keys().collect::<Vec<_>>());
    let gpt = &models["gpt-5.5"];
    assert_eq!((gpt.input, gpt.cached_input, gpt.output), (5.0, Some(0.5), 30.0));
    let long = gpt.long_context.as_ref().unwrap();
    assert_eq!((long.above_tokens, long.input, long.output, long.cached_input), (272_000, 10.0, 45.0, Some(1.0)));
    assert_eq!(models["deepseek/deepseek-chat"].input, 0.28);
    assert_eq!(models["claude-sonnet-4-5"].cache_write_input, Some(3.75));
    assert!(!models.contains_key("text-embedding-3-small"));
    assert!(parse_litellm(&json!({"sample_spec": {}})).is_err());
}

#[test]
fn models_dev_keys_by_provider_and_preferred_bare_ids() {
    let models = parse_models_dev(&models_dev_fixture()).unwrap();
    assert_eq!(models["openrouter/gpt-5.5"].input, 9.0);
    // Bare id from the preferred provider, not the first one listed.
    assert_eq!(models["gpt-5.5"].input, 5.0);
    // zai ranks before zhipuai.
    assert_eq!(models["glm-5.3"].input, 1.4);
    assert_eq!(models["zhipuai/glm-5.3"].input, 0.6);
    assert_eq!(models["moonshotai/kimi-k3"].long_context.as_ref().unwrap().input, 6.0);
    assert!(!models.contains_key("nocost/m"));
}

#[test]
fn model_names_are_normalized_for_lookup() {
    assert_eq!(model_candidates(" DeepSeek/DeepSeek-Chat "), ["deepseek/deepseek-chat", "deepseek-chat"]);
    assert_eq!(model_candidates("claude-sonnet-4-5-20250929"), ["claude-sonnet-4-5-20250929", "claude-sonnet-4-5"]);
    assert_eq!(model_candidates("gpt-5.5-2026-01-02"), ["gpt-5.5-2026-01-02", "gpt-5.5"]);
    assert_eq!(model_candidates("opus[1m]"), ["opus"]);
    assert_eq!(model_candidates("glm-5.3"), ["glm-5.3"]);

    let cache = CatalogCache { models: parse_models_dev(&models_dev_fixture()).unwrap(), ..Default::default() };
    let key = |provider: Option<&str>, category, model: &str| cache.lookup(provider, category, model).map(|(key, _)| key.to_string());
    // Provider name → family prefixes; bigmodel.cn (glm) prefers zhipuai.
    assert_eq!(key(Some("glm"), None, "GLM-5.3").as_deref(), Some("zhipuai/glm-5.3"));
    assert_eq!(key(Some("zai"), None, "glm-5.3").as_deref(), Some("zai/glm-5.3"));
    // Unknown provider name: the profile category decides.
    assert_eq!(key(Some("custom"), Some(ProviderCategory::Glm), "glm-5.3").as_deref(), Some("zhipuai/glm-5.3"));
    assert_eq!(key(Some("kimi"), None, "kimi-k3").as_deref(), Some("moonshotai/kimi-k3"));
    assert_eq!(key(None, None, "gpt-5.5-20260101").as_deref(), Some("gpt-5.5"));
    assert_eq!(key(Some("openrouter"), None, "gpt-5.5").as_deref(), Some("openrouter/gpt-5.5"));
    assert_eq!(key(None, None, "nope"), None);
}

#[test]
fn a_provider_the_library_does_not_name_falls_back_to_the_model_family() {
    // LiteLLM has no bare `glm-5.3`, only vendor keys.
    let mut models = parse_models_dev(&models_dev_fixture()).unwrap();
    models.remove("glm-5.3");
    let cache = CatalogCache { models, ..Default::default() };
    let key = |provider: Option<&str>, model: &str| cache.lookup(provider, None, model).map(|(key, _)| key.to_string());
    // A Claude account routed to Z.ai, a custom provider id: the model name says GLM.
    assert_eq!(key(Some("account:zai-individual-coding-plan"), "GLM-5.3").as_deref(), Some("zhipuai/glm-5.3"));
    assert_eq!(key(Some("zai-standard-api"), "glm-5.3").as_deref(), Some("zhipuai/glm-5.3"));
    assert_eq!(key(None, "glm-5.3").as_deref(), Some("zhipuai/glm-5.3"));
    // The provider's own prefix still wins when the library has it.
    assert_eq!(key(Some("zai"), "glm-5.3").as_deref(), Some("zai/glm-5.3"));
    // A name with no family stays unpriced.
    assert_eq!(key(Some("account:x"), "mystery-1"), None);
    assert_eq!(category_for_model_name("GLM-5.3-Flash"), Some(ProviderCategory::Glm));
    assert_eq!(category_for_model_name("kimi-k3"), Some(ProviderCategory::Kimi));
    assert_eq!(category_for_model_name("sonnet"), None);
}

struct FakeFetcher {
    responses: RefCell<Vec<anyhow::Result<FetchResponse>>>,
    seen: RefCell<Vec<(String, Option<String>, Option<String>)>>,
}

impl FakeFetcher {
    fn new(responses: Vec<anyhow::Result<FetchResponse>>) -> Self {
        Self { responses: RefCell::new(responses), seen: RefCell::new(Vec::new()) }
    }
}

impl Fetcher for FakeFetcher {
    fn get(&self, url: &str, etag: Option<&str>, last_modified: Option<&str>) -> anyhow::Result<FetchResponse> {
        self.seen.borrow_mut().push((url.into(), etag.map(Into::into), last_modified.map(Into::into)));
        self.responses.borrow_mut().remove(0)
    }
}

fn ok(body: &serde_json::Value, etag: &str) -> anyhow::Result<FetchResponse> {
    Ok(FetchResponse {
        status: 200,
        etag: Some(etag.into()),
        last_modified: Some("Wed, 08 Oct 2026 00:00:00 GMT".into()),
        body: serde_json::to_vec(body).unwrap(),
    })
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("cutex-usage-catalog-{name}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn cache_refresh_is_conditional_and_keeps_the_old_copy_on_failure() {
    let dir = temp_dir("refresh");
    let url = "https://example.test/prices.json";
    let t0 = Utc.with_ymd_and_hms(2026, 10, 9, 0, 0, 0).unwrap();
    let fetcher = FakeFetcher::new(vec![
        ok(&litellm_fixture(), "\"v1\""),
        Ok(FetchResponse { status: 304, etag: None, last_modified: None, body: Vec::new() }),
        Err(anyhow::anyhow!("connection failed")),
        Ok(FetchResponse { status: 200, etag: None, last_modified: None, body: b"<html>".to_vec() }),
    ]);
    let first = refresh_catalog_at(&dir, PriceSource::Litellm, url, &fetcher, t0).unwrap();
    assert_eq!(first.models.len(), 4);
    assert_eq!(first.etag.as_deref(), Some("\"v1\""));

    let t1 = t0 + chrono::Duration::hours(25);
    let second = refresh_catalog_at(&dir, PriceSource::Litellm, url, &fetcher, t1).unwrap();
    assert_eq!(second.fetched_at, first.fetched_at, "304 keeps the download time");
    assert_eq!(second.models, first.models);
    assert_eq!(fetcher.seen.borrow()[1].1.as_deref(), Some("\"v1\""), "If-None-Match sent");
    assert!(fetcher.seen.borrow()[1].2.is_some(), "If-Modified-Since sent");

    // Offline / garbage: old models stay, the error is recorded.
    for _ in 0..2 {
        assert!(refresh_catalog_at(&dir, PriceSource::Litellm, url, &fetcher, t1).is_err());
        let cached = load_catalog_at(&catalog_file(&dir, PriceSource::Litellm)).unwrap();
        assert_eq!(cached.models, first.models);
        assert!(cached.last_error.is_some());
    }

    // Due logic: failures retry after an hour, successes after the interval.
    let mut settings = UsageSettings::default().effective();
    settings.catalog_url = Some(url.into());
    assert!(!catalog_due(&dir, &settings, t1 + chrono::Duration::minutes(30)));
    assert!(catalog_due(&dir, &settings, t1 + chrono::Duration::hours(1)));
    settings.catalog_url = Some("https://example.test/other.json".into());
    assert!(catalog_due(&dir, &settings, t1), "a new URL is due at once");
    settings.price_source = PriceSource::Manual;
    assert!(!catalog_due(&dir, &settings, t1), "manual never fetches");
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn exchange_rates_auto_fetch_is_opt_in_and_falls_back_to_config() {
    let dir = temp_dir("fx");
    let t0 = Utc.with_ymd_and_hms(2026, 10, 9, 0, 0, 0).unwrap();
    let mut settings = UsageSettings::default().effective();
    assert_eq!(effective_rates(&dir, &settings).1, "config");
    let fetcher = FakeFetcher::new(vec![
        ok(&json!({"amount": 1.0, "base": "USD", "date": "2026-10-08", "rates": {"AUD": 1.6, "CNY": 7.3}}), "x"),
        Err(anyhow::anyhow!("offline")),
    ]);
    settings.price_source = PriceSource::Manual;
    assert!(refresh_due(&dir, &settings, &FakeFetcher::new(vec![]), true, t0).is_empty());
    settings.exchange_rate_auto_fetch = true;
    assert!(refresh_due(&dir, &settings, &fetcher, false, t0).is_empty());
    let (rates, source, fx) = effective_rates(&dir, &settings);
    assert_eq!((rates.aud, rates.cny, source), (1.6, 7.3, "auto"));
    assert_eq!(fx.unwrap().date.as_deref(), Some("2026-10-08"));
    // A failed refresh keeps the last fetched rates.
    assert_eq!(refresh_due(&dir, &settings, &fetcher, true, t0).len(), 1);
    assert_eq!(effective_rates(&dir, &settings).0.aud, 1.6);
    settings.exchange_rate_auto_fetch = false;
    assert_eq!(effective_rates(&dir, &settings).0.aud, crate::config::usage_pricing::DEFAULT_AUD_PER_USD);
    std::fs::remove_dir_all(dir).ok();
}

fn sample(provider: &str, model: &str, profile: &str, input: u64, cached: u64, output: u64) -> UsageSample {
    UsageSample {
        entry_id: "e".into(),
        event_id: "ev".into(),
        observed_at: "2026-10-09T00:00:00Z".into(),
        host_id: "h".into(),
        cutex_session_id: "s".into(),
        thread_id: "t".into(),
        turn_id: "u".into(),
        runtime_generation: 1,
        profile: Some(profile.into()),
        profile_id: None,
        profile_deleted: false,
        source: None,
        attribution: UsageAttribution {
            provider: Some(provider.into()),
            model: Some(model.into()),
            request_model: None,
            reasoning_effort: None,
            service_tier: None,
        },
        token_source: UsageTokenSource::NativeLast,
        model_context_window: None,
        compaction_count: None,
        model_requests: None,
        tokens: TokenUsageBreakdown {
            total_tokens: input + output,
            input_tokens: input,
            cached_input_tokens: cached,
            cache_write_input_tokens: 0,
            output_tokens: output,
            reasoning_output_tokens: 0,
        },
        cumulative_total: TokenUsageBreakdown::default(),
    }
}

#[test]
fn estimate_prefers_manual_then_library_then_builtin_and_converts() {
    let library = std::sync::Arc::new(CatalogCache {
        models: parse_litellm(&litellm_fixture()).unwrap(),
        ..Default::default()
    });
    let manual: UsagePriceTable = serde_json::from_value(json!({
        "currency": "USD",
        "models": { "deepseek-chat": { "input": 1.0, "output": 2.0, "currency": "CNY" } }
    }))
    .unwrap();
    let mut settings = UsageSettings::default().effective();
    settings.display_currency = Currency::Aud;
    let rates = ExchangeRates { aud: 1.5, cny: 7.0 };
    let mut context = PriceContext::new(&settings, Some(manual.clone()), Some(library.clone()), rates);
    context.profile_categories = HashMap::from([("glm-cn".to_string(), ProviderCategory::Zai)]);

    // Manual (CNY) wins over the library and converts CNY → AUD.
    let (cost, origin) = context.price(&sample("deepseek", "deepseek/deepseek-chat", "p", 1_000_000, 0, 1_000_000)).unwrap();
    assert_eq!(origin, PriceOrigin::Manual);
    assert!((cost - 3.0 / 7.0 * 1.5).abs() < 1e-9);

    // Library: 1M input (0.2M cached) + 0.1M output of gpt-5.5, USD → AUD.
    let (cost, origin) = context.price(&sample("openai", "gpt-5.5", "p", 1_000_000, 200_000, 100_000)).unwrap();
    assert_eq!(origin, PriceOrigin::Litellm);
    // long context (> 272K): 0.8M*10 + 0.2M*1 + 0.1M*45 = 8 + 0.2 + 4.5 USD
    assert!((cost - 12.7 * 1.5).abs() < 1e-9, "{cost}");

    // Library by profile category (provider name unknown).
    let (_, origin) = context.price(&sample("custom", "glm-5.3", "glm-cn", 1000, 0, 1000)).unwrap();
    assert_eq!(origin, PriceOrigin::Litellm);

    // Built-in rate card when the library has no entry.
    let (_, origin) = context.price(&sample("openai", "gpt-6-astra", "p", 1000, 0, 1000)).unwrap();
    assert_eq!(origin, PriceOrigin::Builtin);
    assert!(context.price(&sample("custom", "unknown-model", "p", 1000, 0, 1000)).is_none());

    // Unknown service tier on the library path: unpriced rather than guessed.
    let mut flex = sample("zai", "glm-5.3", "p", 1000, 0, 1000);
    flex.attribution.service_tier = Some("flex".into());
    assert!(context.price(&flex).is_none());

    // Manual source: only the owner's table.
    settings.price_source = PriceSource::Manual;
    let manual_only = PriceContext::new(&settings, Some(manual), Some(library), rates);
    assert!(manual_only.price(&sample("openai", "gpt-5.5", "p", 1000, 0, 1000)).is_none());
    assert_eq!(
        manual_only.price(&sample("deepseek", "deepseek-chat", "p", 1000, 0, 1000)).unwrap().1,
        PriceOrigin::Manual
    );
}

#[test]
fn configurations_use_real_model_prices_and_service_tiers_without_merging_names() {
    use crate::management::v2::usage::pricing::derive_sample_estimates;
    let settings = UsageSettings::default().effective();
    let library = std::sync::Arc::new(CatalogCache {
        models: parse_litellm(&litellm_fixture()).unwrap(), ..Default::default()
    });
    let context = PriceContext::new(&settings, None, Some(library), ExchangeRates::default());
    for model in ["gpt-6-astra", "gpt-5.5", "gpt-5.6-terra"] {
        for tier in [None, Some("fast"), Some("ultrafast"), Some("unknown")] {
            let mut base = sample("openai", model, "p", 300_000, 20_000, 100_000);
            base.attribution.service_tier = tier.map(str::to_owned);
            let expected = context.price(&base);
            let base_estimates = derive_sample_estimates(&base.attribution, &base.tokens);
            for name in ["Astra [400K]", "Astra [285K]"] {
                let mut variant = base.clone();
                variant.attribution.model = Some(name.into());
                variant.attribution.request_model = Some(model.into());
                assert_eq!(context.price(&variant), expected, "{model} {tier:?}");
                let estimates = derive_sample_estimates(&variant.attribution, &variant.tokens);
                assert_eq!(estimates.api_equivalent_usd_nanos, base_estimates.api_equivalent_usd_nanos);
                assert_eq!(estimates.codex_credit_nanos, base_estimates.codex_credit_nanos);
                assert_eq!(variant.attribution.model.as_deref(), Some(name));
            }
        }
    }
}

#[test]
fn explicit_configuration_prices_precede_real_model_prices_only_on_exact_match() {
    let mut settings = UsageSettings::default().effective();
    settings.price_source = PriceSource::Manual;
    let manual: UsagePriceTable = serde_json::from_value(json!({"currency":"USD","models":{
        "gpt-6-astra":{"input":10,"output":50},
        "Astra":{"input":999,"output":999},
        "Astra [400K]":{"input":1,"output":2},
        "openai/Astra [400K]":{"input":3,"output":4},
        "custom-quote":{"input":5,"output":6,"aliases":["Astra [285K]"]}
    }})).unwrap();
    let mut variant = sample("openai", "Astra [400K]", "p", 1_000_000, 0, 1_000_000);
    variant.attribution.request_model = Some("gpt-6-astra".into());
    variant.attribution.service_tier = Some("fast".into());
    let make_context = |manual| PriceContext::new(&settings, Some(manual), None, ExchangeRates::default());
    let mut prices = manual.clone();
    assert_eq!(make_context(prices.clone()).price(&variant), Some((7.0, PriceOrigin::Manual)));
    prices.models.remove("openai/Astra [400K]");
    assert_eq!(make_context(prices.clone()).price(&variant), Some((3.0, PriceOrigin::Manual)));
    variant.attribution.model = Some("Astra [285K]".into());
    assert_eq!(make_context(prices.clone()).price(&variant), Some((11.0, PriceOrigin::Manual)));
    variant.attribution.model = Some("Astra [100K]".into());
    // No explicit configuration price: use the actual model, not "Astra".
    assert_eq!(make_context(prices).price(&variant), Some((60.0, PriceOrigin::Manual)));
}

#[test]
fn model_map_prices_as_the_target_through_manual_library_and_builtin() {
    use crate::management::v2::usage::model::UsageLedger;
    use crate::management::v2::usage::model::UsageLedgerEntry;
    use crate::management::v2::usage::query::aggregate_usage;
    use crate::management::v2::usage::query::UsageGroupKey;
    use crate::management::v2::usage::query::UsageLabels;
    use crate::management::v2::usage::query::UsageQuery;
    let library = std::sync::Arc::new(CatalogCache {
        models: parse_litellm(&litellm_fixture()).unwrap(),
        ..Default::default()
    });
    let manual: UsagePriceTable = serde_json::from_value(json!({
        "currency": "USD",
        "models": { "house-model": { "input": 1.0, "output": 2.0 } },
        "modelMap": {
            "gemini-3.8-flash-tiere": "gpt-5.5",
            "Sonnet": "claude-sonnet-4-5",
            "relay/odd-name": "house-model",
            "gpt-6-astra-alias": "gpt-6-astra",
            "ghost": "no-such-model"
        }
    }))
    .unwrap();
    manual.validate().unwrap();
    let settings = UsageSettings::default().effective();
    let rates = ExchangeRates::default();
    let context = PriceContext::new(&settings, Some(manual.clone()), Some(library.clone()), rates);
    let library_price = |model: &str| context.price(&sample("openai", model, "p", 100_000, 0, 10_000));

    // Library target: same cost as the target itself; date suffix / [..] / case normalized.
    let target = library_price("gpt-5.5").unwrap();
    for model in ["gemini-3.8-flash-tiere", "Gemini-3.8-Flash-Tiere-20261001", "vendor/gemini-3.8-flash-tiere[1m]"] {
        let (priced, to) = context.price_mapped(&sample("google", model, "p", 100_000, 0, 10_000));
        assert_eq!((priced, to), (Some(target), Some("gpt-5.5")), "{model}");
    }
    // Bare Claude Code alias (old claude-r5 rows) priced as the resolved id.
    let (priced, to) = context.price_mapped(&sample("anthropic", "sonnet", "p", 1_000_000, 0, 0));
    assert_eq!(to, Some("claude-sonnet-4-5"));
    assert_eq!(priced, Some((3.0, PriceOrigin::Litellm)));
    let (_, to) = context.price_mapped(&sample("anthropic", "opus", "p", 1, 0, 1));
    assert_eq!(to, None);
    // Provider-qualified key, manual target.
    let (priced, to) = context.price_mapped(&sample("relay", "odd-name", "p", 1_000_000, 0, 1_000_000));
    assert_eq!((priced, to), (Some((3.0, PriceOrigin::Manual)), Some("house-model")));
    assert_eq!(context.price_mapped(&sample("other", "odd-name", "p", 1, 0, 1)).1, None);
    // Built-in rate card target.
    let (priced, _) = context.price_mapped(&sample("openai", "gpt-6-astra-alias", "p", 1000, 0, 1000));
    assert_eq!(priced.map(|(_, origin)| origin), Some(PriceOrigin::Builtin));
    // A configuration id (requestModel) is matched too.
    let mut configured = sample("google", "Flash [1M]", "p", 100_000, 0, 10_000);
    configured.attribution.request_model = Some("gemini-3.8-flash-tiere".into());
    assert_eq!(context.price_mapped(&configured), (Some(target), Some("gpt-5.5")));
    // Unknown target: unpriced, no fallback to the original name.
    assert_eq!(context.price_mapped(&sample("x", "ghost", "p", 1, 0, 1)), (None, Some("no-such-model")));

    // Map without manual prices still applies (manual-only source → no library).
    let map_only = UsagePriceTable { models: Default::default(), ..manual.clone() };
    let context_map_only = PriceContext::new(&settings, Some(map_only), Some(library), rates);
    assert_eq!(
        context_map_only.price_mapped(&sample("google", "gemini-3.8-flash-tiere", "p", 100_000, 0, 10_000)).1,
        Some("gpt-5.5")
    );

    // Report: original model name kept, pricedAs on rows / totals / unpriced hints.
    let ledger = UsageLedger {
        entries: ["gemini-3.8-flash-tiere", "gpt-5.5", "ghost"]
            .into_iter()
            .map(|model| UsageLedgerEntry::Usage(Box::new(sample("google", model, "p", 1000, 0, 100))))
            .collect(),
        warnings: Vec::new(),
    };
    let query = UsageQuery {
        from: chrono::NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
        to: chrono::NaiveDate::from_ymd_opt(2026, 10, 31).unwrap(),
        group_by: vec![UsageGroupKey::Model],
    };
    let result = aggregate_usage(&ledger, &query, &UsageLabels::default(), Some(&context), &|at| at.date_naive());
    let value = serde_json::to_value(&result).unwrap();
    let row = |model: &str| value["rows"].as_array().unwrap().iter().find(|row| row["key"]["model"] == model).unwrap().clone();
    assert_eq!(row("gemini-3.8-flash-tiere")["pricedAs"], json!(["gpt-5.5"]));
    assert_eq!(row("gemini-3.8-flash-tiere")["estimatedCost"], row("gpt-5.5")["estimatedCost"]);
    assert!(row("gpt-5.5").get("pricedAs").is_none());
    assert_eq!(row("ghost")["pricedAs"], json!(["no-such-model"]));
    assert_eq!(value["totals"]["pricedAs"], json!(["gpt-5.5", "no-such-model"]));
    assert_eq!(
        value["unpricedModels"],
        json!([{ "provider": "google", "model": "ghost", "pricedAs": "no-such-model", "requests": 1, "tokens": 1100 }])
    );
}

/// Live smoke test (network): `cargo test --lib live_library_fetch -- --ignored`.
#[test]
#[ignore]
fn live_library_fetch() {
    let dir = temp_dir("live");
    let now = Utc::now();
    for source in [PriceSource::Litellm, PriceSource::ModelsDev] {
        let url = source.default_url().unwrap();
        let cache = refresh_catalog_at(&dir, source, url, &HttpFetcher, now).unwrap();
        let again = refresh_catalog_at(&dir, source, url, &HttpFetcher, now).unwrap();
        println!(
            "{}: {} models, etag {:?}, second fetchedAt same: {}",
            source.key(),
            cache.models.len(),
            cache.etag,
            again.fetched_at == cache.fetched_at
        );
        for (provider, model) in [("openai", "gpt-5.5"), ("deepseek", "deepseek-chat"), ("glm", "glm-4.6"), ("kimi", "kimi-k2-0905-preview"), ("anthropic", "claude-sonnet-4-5-20250929")] {
            println!("  {provider}/{model} -> {:?}", cache.lookup(Some(provider), None, model).map(|(k, p)| (k.to_string(), p.input, p.output)));
        }
    }
    let fx = refresh_fx_at(&dir, FX_URL, &HttpFetcher, now).unwrap();
    println!("fx: {:?} {:?}", fx.date, fx.rates);
    std::fs::remove_dir_all(dir).ok();
}
