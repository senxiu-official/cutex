use serde::Serialize;

use super::model::TokenUsageBreakdown;
use super::model::UsageAttribution;

const NANOS_PER_UNIT: u128 = 1_000_000_000;
const TOKENS_PER_MILLION: u128 = 1_000_000;

pub const CODEX_CREDIT_RATE_CARD_ID: &str = "openai-codex-credits-2026-08-13";
pub const API_EQUIVALENT_RATE_CARD_ID: &str = "provider-api-2026-10-03";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UsageRateCard {
    pub id: String,
    pub unit: String,
    pub description: String,
    pub source_urls: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DerivedUsageEstimate {
    pub rate_card_id: String,
    /// Decimal string so JSON consumers do not lose precision to binary floats.
    pub amount: String,
    pub priced_samples: u64,
    pub unpriced_samples: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct SampleEstimates {
    pub codex_credit_nanos: Option<u128>,
    pub api_equivalent_usd_nanos: Option<u128>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct EstimateAccumulator {
    amount_nanos: u128,
    priced_samples: u64,
    unpriced_samples: u64,
}

impl EstimateAccumulator {
    pub fn add(&mut self, amount_nanos: Option<u128>) -> anyhow::Result<()> {
        match amount_nanos {
            Some(amount_nanos) => {
                self.amount_nanos = self
                    .amount_nanos
                    .checked_add(amount_nanos)
                    .ok_or_else(|| anyhow::anyhow!("derived usage estimate overflow"))?;
                self.priced_samples = self
                    .priced_samples
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("priced usage sample count overflow"))?;
            }
            None => {
                self.unpriced_samples = self
                    .unpriced_samples
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("unpriced usage sample count overflow"))?;
            }
        }
        Ok(())
    }

    pub fn finish(&self, rate_card_id: &str) -> DerivedUsageEstimate {
        DerivedUsageEstimate {
            rate_card_id: rate_card_id.to_string(),
            amount: format_nanos(self.amount_nanos),
            priced_samples: self.priced_samples,
            unpriced_samples: self.unpriced_samples,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct TokenRates {
    uncached_input: u64,
    cached_input: u64,
    cache_write_input: u64,
    output: u64,
}

#[derive(Debug, Clone, Copy)]
struct Multiplier {
    numerator: u64,
    denominator: u64,
}

impl Multiplier {
    const STANDARD: Self = Self {
        numerator: 1,
        denominator: 1,
    };
}

pub(super) fn rate_cards() -> Vec<UsageRateCard> {
    vec![
        UsageRateCard {
            id: CODEX_CREDIT_RATE_CARD_ID.to_string(),
            unit: "credit".to_string(),
            description: "Published Codex token credits; cache writes are excluded and Fast multipliers are applied when identified.".to_string(),
            source_urls: vec![
                "https://help.openai.com/en/articles/20001106-codex-rate-card".to_string(),
                "https://learn.chatgpt.com/docs/agent-configuration/speed".to_string(),
            ],
        },
        UsageRateCard {
            id: API_EQUIVALENT_RATE_CARD_ID.to_string(),
            unit: "USD".to_string(),
            description: "Public API-equivalent estimate from current provider rate cards; unknown models and tiers remain unpriced.".to_string(),
            source_urls: vec![
                "https://platform.openai.com/docs/pricing".to_string(),
                "https://learn.chatgpt.com/docs/agent-configuration/speed".to_string(),
                "https://docs.z.ai/guides/overview/pricing".to_string(),
                "https://platform.moonshot.ai/docs/pricing/chat".to_string(),
            ],
        },
    ]
}

pub(super) fn derive_sample_estimates(
    attribution: &UsageAttribution,
    tokens: &TokenUsageBreakdown,
) -> SampleEstimates {
    if api_provider(attribution.provider.as_deref()).is_none() {
        return SampleEstimates::default();
    }
    let Some(model) = attribution.pricing_model().map(normalize_model) else {
        return SampleEstimates::default();
    };
    let codex_credit_nanos = if is_openai_provider(attribution.provider.as_deref()) {
        codex_credit_rates(&model)
            .and_then(|rates| {
                codex_multiplier(&model, attribution.service_tier.as_deref())
                    .map(|multiplier| (rates, multiplier))
            })
            .and_then(|(rates, multiplier)| derive_amount_nanos(tokens, rates, multiplier))
    } else {
        None
    };
    SampleEstimates {
        codex_credit_nanos,
        api_equivalent_usd_nanos: api_rates(
            attribution.provider.as_deref(),
            &model,
            tokens.input_tokens > 272_000,
        )
        .and_then(|rates| {
            api_multiplier(
                attribution.provider.as_deref(),
                &model,
                attribution.service_tier.as_deref(),
            )
            .map(|multiplier| (rates, multiplier))
        })
        .and_then(|(rates, multiplier)| derive_amount_nanos(tokens, rates, multiplier)),
    }
}

fn is_openai_provider(provider: Option<&str>) -> bool {
    provider.is_none_or(|provider| {
        let provider = provider.trim().to_ascii_lowercase();
        provider == "chatgpt" || provider == "codex" || provider.starts_with("openai")
    })
}

fn normalize_model(model: &str) -> String {
    model.trim().to_ascii_lowercase()
}

fn api_provider(provider: Option<&str>) -> Option<&'static str> {
    if is_openai_provider(provider) {
        Some("openai")
    } else if is_glm_provider(provider) {
        Some("glm")
    } else if is_kimi_provider(provider) {
        Some("kimi")
    } else {
        None
    }
}

fn is_glm_provider(provider: Option<&str>) -> bool {
    provider.is_some_and(|provider| {
        let provider = provider.trim().to_ascii_lowercase();
        provider == "glm" || provider == "zai" || provider == "z.ai"
    })
}

fn is_kimi_provider(provider: Option<&str>) -> bool {
    provider.is_some_and(|provider| {
        let provider = provider.trim().to_ascii_lowercase();
        provider == "kimi" || provider == "moonshot"
    })
}

fn codex_credit_rates(model: &str) -> Option<TokenRates> {
    let rates = match model {
        "gpt-5.6-sol" => TokenRates::new(125_000_000_000, 12_500_000_000, 750_000_000_000),
        "gpt-5.6-terra" => TokenRates::new(50_000_000_000, 5_000_000_000, 300_000_000_000),
        "gpt-5.6-luna" => TokenRates::new(5_000_000_000, 500_000_000, 30_000_000_000),
        "gpt-5.5" => TokenRates::new(125_000_000_000, 12_500_000_000, 750_000_000_000),
        "gpt-5.5-cyber" | "gpt-5.5-codex-cyber" => {
            TokenRates::new(312_500_000_000, 31_250_000_000, 1_875_000_000_000)
        }
        "gpt-5.4" => TokenRates::new(62_500_000_000, 6_250_000_000, 375_000_000_000),
        "gpt-5.4-mini" => TokenRates::new(18_750_000_000, 1_875_000_000, 113_000_000_000),
        "gpt-5.3-codex" => TokenRates::new(43_750_000_000, 4_375_000_000, 350_000_000_000),
        "gpt-5.2" => TokenRates::new(43_750_000_000, 4_375_000_000, 350_000_000_000),
        _ => return None,
    };
    Some(rates)
}

fn api_rates(provider: Option<&str>, model: &str, long_context: bool) -> Option<TokenRates> {
    if is_glm_provider(provider) {
        let rates = match model {
            "glm-5.3" | "glm-5.2" | "glm-5.1" => TokenRates::with_cache_write(
                1_400_000_000,
                260_000_000,
                1_400_000_000,
                4_400_000_000,
            ),
            "glm-5.3-flash" => {
                TokenRates::with_cache_write(150_000_000, 30_000_000, 150_000_000, 500_000_000)
            }
            "glm-5.3-flashx" => {
                TokenRates::with_cache_write(370_000_000, 75_000_000, 370_000_000, 1_250_000_000)
            }
            _ => return None,
        };
        return Some(rates);
    }
    if is_kimi_provider(provider) {
        let rates = match model {
            "kimi-k3" | "k3" | "k3-256k" => TokenRates::with_cache_write(
                3_000_000_000,
                300_000_000,
                3_000_000_000,
                15_000_000_000,
            ),
            _ => return None,
        };
        return Some(rates);
    }
    let rates = match model {
        "gpt-6-astra" => TokenRates::with_cache_write(
            10_000_000_000,
            1_000_000_000,
            12_500_000_000,
            50_000_000_000,
        ),
        "gpt-6.1-sol" => {
            TokenRates::with_cache_write(2_000_000_000, 100_000_000, 2_500_000_000, 10_000_000_000)
        }
        "gpt-6-sol" => {
            TokenRates::with_cache_write(2_000_000_000, 200_000_000, 2_500_000_000, 10_000_000_000)
        }
        "gpt-6-luna" => {
            TokenRates::with_cache_write(100_000_000, 10_000_000, 125_000_000, 500_000_000)
        }
        "gpt-5.6-sol" => {
            TokenRates::with_cache_write(4_000_000_000, 400_000_000, 5_000_000_000, 20_000_000_000)
        }
        "gpt-5.6-terra" => {
            TokenRates::with_cache_write(2_000_000_000, 200_000_000, 2_500_000_000, 12_000_000_000)
        }
        "gpt-5.6-luna" => {
            TokenRates::with_cache_write(200_000_000, 20_000_000, 250_000_000, 1_200_000_000)
        }
        "gpt-5.5" => TokenRates::with_cache_write(5_000_000_000, 500_000_000, 0, 30_000_000_000),
        _ => return None,
    };
    Some(if long_context {
        TokenRates::with_cache_write(
            rates.uncached_input.checked_mul(2)?,
            rates.cached_input.checked_mul(2)?,
            rates.cache_write_input.checked_mul(2)?,
            rates.output.checked_mul(3)? / 2,
        )
    } else {
        rates
    })
}

impl TokenRates {
    const fn new(uncached_input: u64, cached_input: u64, output: u64) -> Self {
        Self::with_cache_write(uncached_input, cached_input, 0, output)
    }

    const fn with_cache_write(
        uncached_input: u64,
        cached_input: u64,
        cache_write_input: u64,
        output: u64,
    ) -> Self {
        Self {
            uncached_input,
            cached_input,
            cache_write_input,
            output,
        }
    }
}

fn codex_multiplier(model: &str, service_tier: Option<&str>) -> Option<Multiplier> {
    match normalize_tier(service_tier).as_deref() {
        None | Some("default") => Some(Multiplier::STANDARD),
        Some("fast") => match model {
            "gpt-6-astra"
            | "gpt-6.1-sol"
            | "gpt-6-luna"
            | "gpt-5.6-sol"
            | "gpt-5.6-terra"
            | "gpt-5.6-luna"
            | "gpt-5.5"
            | "gpt-5.5-cyber"
            | "gpt-5.5-codex-cyber" => Some(Multiplier {
                numerator: 5,
                denominator: 2,
            }),
            "gpt-5.4" => Some(Multiplier {
                numerator: 2,
                denominator: 1,
            }),
            _ => None,
        },
        Some("ultrafast") if matches!(model, "gpt-6-astra") => Some(Multiplier {
            numerator: 8,
            denominator: 1,
        }),
        Some(_) => None,
    }
}

fn api_multiplier(
    provider: Option<&str>,
    model: &str,
    service_tier: Option<&str>,
) -> Option<Multiplier> {
    match normalize_tier(service_tier).as_deref() {
        None | Some("default") => Some(Multiplier::STANDARD),
        _ if !is_openai_provider(provider) => None,
        Some("fast" | "priority") => match model {
            "gpt-5.5" => Some(Multiplier {
                numerator: 5,
                denominator: 2,
            }),
            _ => Some(Multiplier {
                numerator: 2,
                denominator: 1,
            }),
        },
        Some("ultrafast") if model == "gpt-6-astra" => Some(Multiplier {
            numerator: 6,
            denominator: 1,
        }),
        Some(_) => None,
    }
}

fn normalize_tier(service_tier: Option<&str>) -> Option<String> {
    service_tier
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase)
}

fn derive_amount_nanos(
    tokens: &TokenUsageBreakdown,
    rates: TokenRates,
    multiplier: Multiplier,
) -> Option<u128> {
    let accounted_input = tokens
        .cached_input_tokens
        .checked_add(tokens.cache_write_input_tokens)?;
    let uncached_input = tokens.input_tokens.checked_sub(accounted_input)?;
    let weighted = u128::from(uncached_input)
        .checked_mul(u128::from(rates.uncached_input))?
        .checked_add(
            u128::from(tokens.cached_input_tokens).checked_mul(u128::from(rates.cached_input))?,
        )?
        .checked_add(
            u128::from(tokens.cache_write_input_tokens)
                .checked_mul(u128::from(rates.cache_write_input))?,
        )?
        .checked_add(u128::from(tokens.output_tokens).checked_mul(u128::from(rates.output))?)?
        .checked_mul(u128::from(multiplier.numerator))?;
    weighted.checked_div(TOKENS_PER_MILLION.checked_mul(u128::from(multiplier.denominator))?)
}

fn format_nanos(value: u128) -> String {
    let whole = value / NANOS_PER_UNIT;
    let remainder = value % NANOS_PER_UNIT;
    if remainder == 0 {
        return whole.to_string();
    }
    let mut fraction = format!("{remainder:09}");
    while fraction.ends_with('0') {
        fraction.pop();
    }
    format!("{whole}.{fraction}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terra(service_tier: Option<&str>) -> UsageAttribution {
        UsageAttribution {
            provider: Some("openai".to_string()),
            model: Some("gpt-5.6-terra".to_string()),
            request_model: None,
            reasoning_effort: Some("xhigh".to_string()),
            service_tier: service_tier.map(str::to_string),
        }
    }

    fn mixed_tokens() -> TokenUsageBreakdown {
        TokenUsageBreakdown {
            total_tokens: 1_100_000,
            input_tokens: 1_000_000,
            cached_input_tokens: 200_000,
            cache_write_input_tokens: 100_000,
            output_tokens: 100_000,
            reasoning_output_tokens: 40_000,
        }
    }

    #[test]
    fn standard_terra_rates_keep_reasoning_as_an_output_subset() {
        let estimates = derive_sample_estimates(&terra(None), &mixed_tokens());
        assert_eq!(
            estimates.codex_credit_nanos.map(format_nanos),
            Some("66".to_string())
        );
        assert_eq!(
            estimates.api_equivalent_usd_nanos.map(format_nanos),
            Some("5.18".to_string()),
            "requests above 272K use the published long-context rate schedule"
        );

        let mut tokens = mixed_tokens();
        tokens.input_tokens = 200_000;
        tokens.cached_input_tokens = 40_000;
        tokens.cache_write_input_tokens = 20_000;
        tokens.output_tokens = 20_000;
        tokens.reasoning_output_tokens = 8_000;
        tokens.total_tokens = 220_000;
        let estimates = derive_sample_estimates(&terra(None), &tokens);
        assert_eq!(
            estimates.codex_credit_nanos.map(format_nanos),
            Some("13.2".to_string())
        );
        assert_eq!(
            estimates.api_equivalent_usd_nanos.map(format_nanos),
            Some("0.578".to_string())
        );
    }

    #[test]
    fn fast_tier_applies_published_credit_and_priority_multipliers() {
        let mut tokens = mixed_tokens();
        tokens.input_tokens = 200_000;
        tokens.cached_input_tokens = 40_000;
        tokens.cache_write_input_tokens = 20_000;
        tokens.output_tokens = 20_000;
        tokens.total_tokens = 220_000;
        let estimates = derive_sample_estimates(&terra(Some("fast")), &tokens);
        assert_eq!(
            estimates.codex_credit_nanos.map(format_nanos),
            Some("33".to_string())
        );
        assert_eq!(
            estimates.api_equivalent_usd_nanos.map(format_nanos),
            Some("1.156".to_string())
        );
    }

    #[test]
    fn gpt_6_astra_standard_and_long_context_api_rates_are_supported() {
        let attribution = UsageAttribution {
            provider: Some("openai".to_string()),
            model: Some("gpt-6-astra".to_string()),
            request_model: None,
            reasoning_effort: Some("high".to_string()),
            service_tier: None,
        };
        let mut tokens = TokenUsageBreakdown {
            total_tokens: 220_000,
            input_tokens: 200_000,
            cached_input_tokens: 40_000,
            cache_write_input_tokens: 20_000,
            output_tokens: 20_000,
            reasoning_output_tokens: 8_000,
        };
        let standard = derive_sample_estimates(&attribution, &tokens);
        assert_eq!(
            standard.api_equivalent_usd_nanos.map(format_nanos),
            Some("2.69".to_string())
        );
        assert_eq!(standard.codex_credit_nanos, None);

        tokens.total_tokens = 320_000;
        tokens.input_tokens = 300_000;
        let long_context = derive_sample_estimates(&attribution, &tokens);
        assert_eq!(
            long_context.api_equivalent_usd_nanos.map(format_nanos),
            Some("6.88".to_string())
        );
    }

    #[test]
    fn gpt_6_astra_ultrafast_uses_official_multipliers() {
        let attribution = UsageAttribution {
            provider: Some("chatgpt".to_string()),
            model: Some("gpt-6-astra".to_string()),
            request_model: None,
            reasoning_effort: None,
            service_tier: Some("ultrafast".to_string()),
        };
        let tokens = TokenUsageBreakdown {
            total_tokens: 100,
            input_tokens: 80,
            cached_input_tokens: 20,
            cache_write_input_tokens: 0,
            output_tokens: 20,
            reasoning_output_tokens: 0,
        };
        let estimates = derive_sample_estimates(&attribution, &tokens);
        assert_eq!(
            estimates.api_equivalent_usd_nanos.map(format_nanos),
            Some("0.00972".to_string())
        );
    }

    #[test]
    fn gpt_5_5_uses_current_standard_and_fast_api_rates() {
        let mut attribution = terra(None);
        attribution.model = Some("gpt-5.5".to_string());
        let mut tokens = TokenUsageBreakdown {
            total_tokens: 220_000,
            input_tokens: 200_000,
            cached_input_tokens: 40_000,
            cache_write_input_tokens: 0,
            output_tokens: 20_000,
            reasoning_output_tokens: 8_000,
        };
        let standard = derive_sample_estimates(&attribution, &tokens);
        assert_eq!(
            standard.api_equivalent_usd_nanos.map(format_nanos),
            Some("1.42".to_string())
        );

        attribution.service_tier = Some("fast".to_string());
        let fast = derive_sample_estimates(&attribution, &tokens);
        assert_eq!(
            fast.api_equivalent_usd_nanos.map(format_nanos),
            Some("3.55".to_string())
        );

        tokens.input_tokens = 300_000;
        tokens.total_tokens = 320_000;
        attribution.service_tier = None;
        let long_context = derive_sample_estimates(&attribution, &tokens);
        assert_eq!(
            long_context.api_equivalent_usd_nanos.map(format_nanos),
            Some("3.54".to_string())
        );
    }

    #[test]
    fn glm_and_kimi_public_api_rates_are_supported_without_codex_credits() {
        let tokens = TokenUsageBreakdown {
            total_tokens: 1_000_000,
            input_tokens: 950_000,
            cached_input_tokens: 900_000,
            cache_write_input_tokens: 0,
            output_tokens: 50_000,
            reasoning_output_tokens: 0,
        };
        let glm = UsageAttribution {
            provider: Some("GLM".to_string()),
            model: Some("glm-5.3-flash".to_string()),
            request_model: None,
            reasoning_effort: Some("max".to_string()),
            service_tier: Some("default".to_string()),
        };
        let glm_estimates = derive_sample_estimates(&glm, &tokens);
        assert_eq!(
            glm_estimates.api_equivalent_usd_nanos.map(format_nanos),
            Some("0.0595".to_string())
        );
        assert_eq!(glm_estimates.codex_credit_nanos, None);

        let mut unsupported_glm_tier = glm;
        unsupported_glm_tier.service_tier = Some("flex".to_string());
        assert_eq!(
            derive_sample_estimates(&unsupported_glm_tier, &tokens),
            SampleEstimates::default()
        );

        let kimi = UsageAttribution {
            provider: Some("kimi".to_string()),
            model: Some("k3-256k".to_string()),
            request_model: None,
            reasoning_effort: Some("max".to_string()),
            service_tier: Some("default".to_string()),
        };
        let kimi_estimates = derive_sample_estimates(&kimi, &tokens);
        assert_eq!(
            kimi_estimates.api_equivalent_usd_nanos.map(format_nanos),
            Some("1.17".to_string())
        );
        assert_eq!(kimi_estimates.codex_credit_nanos, None);
    }

    #[test]
    fn unknown_provider_model_or_invalid_token_partition_is_unpriced() {
        let mut attribution = terra(None);
        attribution.provider = Some("deepseek".to_string());
        assert_eq!(
            derive_sample_estimates(&attribution, &mixed_tokens()),
            SampleEstimates::default()
        );

        attribution.provider = Some("openai".to_string());
        attribution.model = Some("future-model".to_string());
        assert_eq!(
            derive_sample_estimates(&attribution, &mixed_tokens()),
            SampleEstimates::default()
        );

        attribution.model = Some("gpt-5.6-terra".to_string());
        let mut invalid = mixed_tokens();
        invalid.cached_input_tokens = invalid.input_tokens;
        invalid.cache_write_input_tokens = 1;
        assert_eq!(
            derive_sample_estimates(&attribution, &invalid),
            SampleEstimates::default()
        );
    }
}

/// Service-tier multiplier for library / built-in API prices: 1 for the
/// default tier, the published OpenAI priority / fast multipliers, `None`
/// for tiers without a known multiplier (the sample stays unpriced).
pub(super) fn api_tier_multiplier(provider: Option<&str>, model: &str, service_tier: Option<&str>) -> Option<f64> {
    api_multiplier(provider, &normalize_model(model), service_tier)
        .map(|multiplier| multiplier.numerator as f64 / multiplier.denominator as f64)
}
