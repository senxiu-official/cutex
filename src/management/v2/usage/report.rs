use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::HashMap;

use chrono::DateTime;
use chrono::Datelike;
use chrono::Duration;
use chrono::NaiveDateTime;
use chrono::SecondsFormat;
use chrono::Timelike;
use chrono::Utc;
use serde::Serialize;

use super::model::parse_timestamp;
use super::model::ResetWindowKind;
use super::model::TokenUsageBreakdown;
use super::model::UsageAttribution;
use super::model::UsageLedger;
use super::model::UsageLedgerEntry;
use super::model::UsageResetBoundary;
use super::model::UsageSample;
use super::model::UsageStateSnapshot;
use super::pricing::derive_sample_estimates;
use super::pricing::rate_cards;
use super::pricing::DerivedUsageEstimate;
use super::pricing::EstimateAccumulator;
use super::pricing::UsageRateCard;
use super::pricing::API_EQUIVALENT_RATE_CARD_ID;
use super::pricing::CODEX_CREDIT_RATE_CARD_ID;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UsageReportPeriod {
    Total,
    Hour,
    Day,
    Week,
    Reset,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UsageReportGroupBy {
    Agent,
    Profile,
    Model,
}

#[derive(Debug, Clone)]
pub struct UsageReportQuery {
    pub period: UsageReportPeriod,
    pub group_by: UsageReportGroupBy,
    pub since: Option<DateTime<Utc>>,
    /// Exclusive upper time bound.
    pub until: Option<DateTime<Utc>>,
    pub reset_window: ResetWindowKind,
}

impl Default for UsageReportQuery {
    fn default() -> Self {
        Self {
            period: UsageReportPeriod::Day,
            group_by: UsageReportGroupBy::Agent,
            since: None,
            until: None,
            reset_window: ResetWindowKind::Primary,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UsageReport {
    pub generated_at: String,
    pub scope: UsageReportScope,
    pub coverage: UsageReportCoverage,
    pub rate_cards: Vec<UsageRateCard>,
    pub rows: Vec<UsageReportRow>,
    pub totals: UsageReportTotals,
    pub pricing_gaps: Vec<UsagePricingGap>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UsageReportScope {
    pub period: UsageReportPeriod,
    pub group_by: UsageReportGroupBy,
    pub since: Option<String>,
    pub until: Option<String>,
    pub reset_window: ResetWindowKind,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UsageReportCoverage {
    pub reducer_revision: u64,
    pub reducer_observed_since: Option<String>,
    pub reducer_updated_at: Option<String>,
    pub first_selected_sample_at: Option<String>,
    pub last_selected_sample_at: Option<String>,
    pub selected_samples: u64,
    pub ledger_usage_samples: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UsageReportRow {
    pub period: UsagePeriodBucket,
    pub group: UsageReportGroup,
    pub usage_samples: u64,
    pub tokens: TokenUsageBreakdown,
    pub codex_credits: DerivedUsageEstimate,
    pub api_equivalent_usd: DerivedUsageEstimate,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UsageReportTotals {
    pub usage_samples: u64,
    pub tokens: TokenUsageBreakdown,
    pub codex_credits: DerivedUsageEstimate,
    pub api_equivalent_usd: DerivedUsageEstimate,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UsagePeriodBucket {
    pub key: String,
    pub label: String,
    pub start: Option<String>,
    pub end: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_limit_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_window: Option<ResetWindowKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_observed: Option<bool>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UsageReportGroup {
    pub key: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cutex_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UsagePricingGap {
    pub attribution: UsageAttribution,
    pub usage_samples: u64,
    pub tokens: TokenUsageBreakdown,
    pub codex_credit_unpriced_samples: u64,
    pub api_equivalent_unpriced_samples: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum PeriodIdentity {
    Total,
    Timed(i64),
    Reset {
        start: i64,
        end: i64,
        profile: Option<String>,
        limit_id: Option<String>,
        window: String,
    },
    ResetUnobserved {
        profile: Option<String>,
        window: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum GroupIdentity {
    Agent(String),
    Profile(Option<String>),
    Model {
        provider: Option<String>,
        model: Option<String>,
        reasoning_effort: Option<String>,
        service_tier: Option<String>,
    },
}

#[derive(Debug)]
struct ResetInterval {
    entry_id: String,
    profile: Option<String>,
    limit_id: Option<String>,
    window: ResetWindowKind,
    start: i64,
    end: i64,
    observed_at: DateTime<Utc>,
}

#[derive(Debug)]
struct RowAccumulator {
    period: UsagePeriodBucket,
    group: UsageReportGroup,
    usage_samples: u64,
    tokens: TokenUsageBreakdown,
    codex_credits: EstimateAccumulator,
    api_equivalent_usd: EstimateAccumulator,
}

#[derive(Debug, Default)]
struct TotalsAccumulator {
    usage_samples: u64,
    tokens: TokenUsageBreakdown,
    codex_credits: EstimateAccumulator,
    api_equivalent_usd: EstimateAccumulator,
}

#[derive(Debug)]
struct GapAccumulator {
    attribution: UsageAttribution,
    usage_samples: u64,
    tokens: TokenUsageBreakdown,
    codex_credit_unpriced_samples: u64,
    api_equivalent_unpriced_samples: u64,
}

pub fn build_usage_report(
    ledger: &UsageLedger,
    state: &UsageStateSnapshot,
    query: &UsageReportQuery,
    agent_labels: &HashMap<String, String>,
    generated_at: DateTime<Utc>,
) -> anyhow::Result<UsageReport> {
    validate_query(query)?;
    let reset_intervals = reset_intervals(ledger, query.reset_window)?;
    let mut rows = BTreeMap::<(PeriodIdentity, GroupIdentity), RowAccumulator>::new();
    let mut totals = TotalsAccumulator::default();
    let mut gaps = BTreeMap::<String, GapAccumulator>::new();
    let mut coverage = UsageReportCoverage {
        reducer_revision: state.revision,
        reducer_observed_since: state.observed_since.clone(),
        reducer_updated_at: state.updated_at.clone(),
        ledger_usage_samples: count_usage_samples(ledger)?,
        ..UsageReportCoverage::default()
    };
    let mut unmatched_reset_samples = 0_u64;

    for entry in &ledger.entries {
        let UsageLedgerEntry::Usage(sample) = entry else {
            continue;
        };
        let observed_at = parse_timestamp(&sample.observed_at)?.with_timezone(&Utc);
        if query.since.is_some_and(|since| observed_at < since)
            || query.until.is_some_and(|until| observed_at >= until)
        {
            continue;
        }
        let (period_identity, period, reset_matched) = period_bucket(
            query.period,
            query.reset_window,
            sample,
            observed_at,
            &reset_intervals,
        )?;
        if query.period == UsageReportPeriod::Reset && !reset_matched {
            unmatched_reset_samples = checked_count(unmatched_reset_samples)?;
        }
        let (group_identity, group) = group_bucket(query.group_by, sample, agent_labels);
        let estimates = derive_sample_estimates(&sample.attribution, &sample.tokens);
        let row = rows
            .entry((period_identity, group_identity))
            .or_insert_with(|| RowAccumulator {
                period,
                group,
                usage_samples: 0,
                tokens: TokenUsageBreakdown::default(),
                codex_credits: EstimateAccumulator::default(),
                api_equivalent_usd: EstimateAccumulator::default(),
            });
        add_sample(
            &mut row.usage_samples,
            &mut row.tokens,
            &mut row.codex_credits,
            &mut row.api_equivalent_usd,
            sample,
            estimates.codex_credit_nanos,
            estimates.api_equivalent_usd_nanos,
        )?;
        add_sample(
            &mut totals.usage_samples,
            &mut totals.tokens,
            &mut totals.codex_credits,
            &mut totals.api_equivalent_usd,
            sample,
            estimates.codex_credit_nanos,
            estimates.api_equivalent_usd_nanos,
        )?;
        update_coverage(&mut coverage, &sample.observed_at)?;
        // A missing Codex-credit rate is not an API-pricing gap: non-OpenAI
        // providers never use that rate card, and newer OpenAI models can have
        // API rates before Cutex has a subscription-credit-rate model.
        if estimates.api_equivalent_usd_nanos.is_none() {
            add_pricing_gap(
                &mut gaps,
                sample,
                estimates.codex_credit_nanos.is_none(),
                estimates.api_equivalent_usd_nanos.is_none(),
            )?;
        }
    }

    let mut warnings = ledger.warnings.clone();
    if unmatched_reset_samples > 0 {
        warnings.push(format!(
            "{unmatched_reset_samples} usage samples did not match an observed {} reset window",
            reset_window_label(query.reset_window)
        ));
    }
    Ok(UsageReport {
        generated_at: canonical_timestamp(generated_at),
        scope: UsageReportScope {
            period: query.period,
            group_by: query.group_by,
            since: query.since.map(canonical_timestamp),
            until: query.until.map(canonical_timestamp),
            reset_window: query.reset_window,
        },
        coverage,
        rate_cards: rate_cards(),
        rows: rows.into_values().map(RowAccumulator::finish).collect(),
        totals: totals.finish(),
        pricing_gaps: gaps.into_values().map(GapAccumulator::finish).collect(),
        warnings,
    })
}

fn validate_query(query: &UsageReportQuery) -> anyhow::Result<()> {
    if query
        .since
        .zip(query.until)
        .is_some_and(|(since, until)| since >= until)
    {
        anyhow::bail!("usage report --since must be earlier than --until");
    }
    Ok(())
}

fn count_usage_samples(ledger: &UsageLedger) -> anyhow::Result<u64> {
    ledger.entries.iter().try_fold(0_u64, |count, entry| {
        if matches!(entry, UsageLedgerEntry::Usage(_)) {
            checked_count(count)
        } else {
            Ok(count)
        }
    })
}

fn add_sample(
    usage_samples: &mut u64,
    tokens: &mut TokenUsageBreakdown,
    codex_credits: &mut EstimateAccumulator,
    api_equivalent_usd: &mut EstimateAccumulator,
    sample: &UsageSample,
    codex_credit_nanos: Option<u128>,
    api_equivalent_usd_nanos: Option<u128>,
) -> anyhow::Result<()> {
    *usage_samples = checked_count(*usage_samples)?;
    tokens.checked_add_assign(&sample.tokens)?;
    codex_credits.add(codex_credit_nanos)?;
    api_equivalent_usd.add(api_equivalent_usd_nanos)?;
    Ok(())
}

fn checked_count(current: u64) -> anyhow::Result<u64> {
    current
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("usage sample count overflow"))
}

fn update_coverage(coverage: &mut UsageReportCoverage, timestamp: &str) -> anyhow::Result<()> {
    let parsed = parse_timestamp(timestamp)?;
    if coverage
        .first_selected_sample_at
        .as_deref()
        .map(parse_timestamp)
        .transpose()?
        .is_none_or(|current| parsed < current)
    {
        coverage.first_selected_sample_at = Some(timestamp.to_string());
    }
    if coverage
        .last_selected_sample_at
        .as_deref()
        .map(parse_timestamp)
        .transpose()?
        .is_none_or(|current| parsed > current)
    {
        coverage.last_selected_sample_at = Some(timestamp.to_string());
    }
    coverage.selected_samples = checked_count(coverage.selected_samples)?;
    Ok(())
}

fn period_bucket(
    period: UsageReportPeriod,
    reset_window: ResetWindowKind,
    sample: &UsageSample,
    observed_at: DateTime<Utc>,
    reset_intervals: &[ResetInterval],
) -> anyhow::Result<(PeriodIdentity, UsagePeriodBucket, bool)> {
    match period {
        UsageReportPeriod::Total => Ok((
            PeriodIdentity::Total,
            UsagePeriodBucket {
                key: "all".to_string(),
                label: "all".to_string(),
                start: None,
                end: None,
                reset_profile: None,
                reset_limit_id: None,
                reset_window: None,
                reset_observed: None,
            },
            true,
        )),
        UsageReportPeriod::Hour => {
            let start = utc_datetime(
                observed_at
                    .date_naive()
                    .and_hms_opt(observed_at.hour(), 0, 0),
                "hour",
            )?;
            timed_bucket(
                start,
                start + Duration::hours(1),
                start.format("%Y-%m-%d %H:00 UTC").to_string(),
            )
        }
        UsageReportPeriod::Day => {
            let start = utc_datetime(observed_at.date_naive().and_hms_opt(0, 0, 0), "day")?;
            timed_bucket(
                start,
                start + Duration::days(1),
                start.format("%Y-%m-%d").to_string(),
            )
        }
        UsageReportPeriod::Week => {
            let date = observed_at.date_naive()
                - Duration::days(i64::from(observed_at.weekday().num_days_from_monday()));
            let start = utc_datetime(date.and_hms_opt(0, 0, 0), "ISO week")?;
            timed_bucket(
                start,
                start + Duration::days(7),
                format!(
                    "{}-W{:02}",
                    observed_at.iso_week().year(),
                    observed_at.iso_week().week()
                ),
            )
        }
        UsageReportPeriod::Reset => reset_bucket(
            reset_window,
            sample.profile.as_ref(),
            observed_at,
            reset_intervals,
        ),
    }
}

fn timed_bucket(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    label: String,
) -> anyhow::Result<(PeriodIdentity, UsagePeriodBucket, bool)> {
    Ok((
        PeriodIdentity::Timed(start.timestamp()),
        UsagePeriodBucket {
            key: canonical_timestamp(start),
            label,
            start: Some(canonical_timestamp(start)),
            end: Some(canonical_timestamp(end)),
            reset_profile: None,
            reset_limit_id: None,
            reset_window: None,
            reset_observed: None,
        },
        true,
    ))
}

fn reset_bucket(
    reset_window: ResetWindowKind,
    profile: Option<&String>,
    observed_at: DateTime<Utc>,
    reset_intervals: &[ResetInterval],
) -> anyhow::Result<(PeriodIdentity, UsagePeriodBucket, bool)> {
    let interval = best_reset_interval(reset_intervals, profile, observed_at);
    let Some(interval) = interval else {
        let profile = profile.cloned();
        let window = reset_window_label(reset_window).to_string();
        return Ok((
            PeriodIdentity::ResetUnobserved {
                profile: profile.clone(),
                window: window.clone(),
            },
            UsagePeriodBucket {
                key: format!(
                    "reset:{window}:unobserved:{}",
                    profile.as_deref().unwrap_or("unknown-profile")
                ),
                label: format!("unobserved {window} reset"),
                start: None,
                end: None,
                reset_profile: profile,
                reset_limit_id: None,
                reset_window: Some(reset_window),
                reset_observed: Some(false),
            },
            false,
        ));
    };
    let start = DateTime::from_timestamp(interval.start, 0)
        .ok_or_else(|| anyhow::anyhow!("reset window start is outside the timestamp range"))?;
    let end = DateTime::from_timestamp(interval.end, 0)
        .ok_or_else(|| anyhow::anyhow!("reset window end is outside the timestamp range"))?;
    let window = reset_window_label(interval.window).to_string();
    let profile = interval.profile.clone();
    let limit_id = interval.limit_id.clone();
    Ok((
        PeriodIdentity::Reset {
            start: interval.start,
            end: interval.end,
            profile: profile.clone(),
            limit_id: limit_id.clone(),
            window: window.clone(),
        },
        UsagePeriodBucket {
            key: format!(
                "reset:{window}:{}:{}:{}:{}",
                interval.start,
                interval.end,
                profile.as_deref().unwrap_or("unknown-profile"),
                limit_id.as_deref().unwrap_or("unknown-limit")
            ),
            label: reset_period_label(start, end, &window),
            start: Some(canonical_timestamp(start)),
            end: Some(canonical_timestamp(end)),
            reset_profile: profile,
            reset_limit_id: limit_id,
            reset_window: Some(interval.window),
            reset_observed: Some(true),
        },
        true,
    ))
}

fn utc_datetime(value: Option<NaiveDateTime>, label: &str) -> anyhow::Result<DateTime<Utc>> {
    value
        .map(|value| DateTime::from_naive_utc_and_offset(value, Utc))
        .ok_or_else(|| anyhow::anyhow!("failed to construct usage {label} boundary"))
}

fn reset_intervals(
    ledger: &UsageLedger,
    window: ResetWindowKind,
) -> anyhow::Result<Vec<ResetInterval>> {
    ledger
        .entries
        .iter()
        .filter_map(|entry| match entry {
            UsageLedgerEntry::ResetBoundary(boundary) if boundary.window_kind == window => {
                Some(reset_interval(boundary))
            }
            _ => None,
        })
        .collect()
}

fn reset_interval(boundary: &UsageResetBoundary) -> anyhow::Result<ResetInterval> {
    let duration_seconds = boundary
        .window_duration_mins
        .checked_mul(60)
        .ok_or_else(|| anyhow::anyhow!("reset window duration overflow"))?;
    let start = boundary
        .resets_at
        .checked_sub(duration_seconds)
        .ok_or_else(|| anyhow::anyhow!("reset window start overflow"))?;
    Ok(ResetInterval {
        entry_id: boundary.entry_id.clone(),
        profile: boundary.profile.clone(),
        limit_id: boundary.limit_id.clone(),
        window: boundary.window_kind,
        start,
        end: boundary.resets_at,
        observed_at: parse_timestamp(&boundary.observed_at)?.with_timezone(&Utc),
    })
}

fn best_reset_interval<'a>(
    intervals: &'a [ResetInterval],
    profile: Option<&String>,
    timestamp: DateTime<Utc>,
) -> Option<&'a ResetInterval> {
    intervals
        .iter()
        .filter(|interval| {
            interval.profile.as_ref() == profile
                && timestamp.timestamp() >= interval.start
                && timestamp.timestamp() < interval.end
        })
        .min_by(|left, right| compare_reset_candidates(left, right, timestamp))
}

fn compare_reset_candidates(
    left: &ResetInterval,
    right: &ResetInterval,
    timestamp: DateTime<Utc>,
) -> Ordering {
    let left_before = left.observed_at <= timestamp;
    let right_before = right.observed_at <= timestamp;
    match (left_before, right_before) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (true, true) => right
            .observed_at
            .cmp(&left.observed_at)
            .then_with(|| left.entry_id.cmp(&right.entry_id)),
        (false, false) => left
            .observed_at
            .cmp(&right.observed_at)
            .then_with(|| left.entry_id.cmp(&right.entry_id)),
    }
}

fn group_bucket(
    group_by: UsageReportGroupBy,
    sample: &UsageSample,
    agent_labels: &HashMap<String, String>,
) -> (GroupIdentity, UsageReportGroup) {
    match group_by {
        UsageReportGroupBy::Agent => {
            let id = sample.cutex_session_id.clone();
            let label = agent_labels.get(&id).cloned().unwrap_or_else(|| id.clone());
            (
                GroupIdentity::Agent(id.clone()),
                UsageReportGroup {
                    key: id.clone(),
                    label,
                    cutex_session_id: Some(id),
                    profile: None,
                    provider: None,
                    model: None,
                    reasoning_effort: None,
                    service_tier: None,
                },
            )
        }
        UsageReportGroupBy::Profile => {
            let profile = sample.profile.clone();
            let label = profile
                .clone()
                .unwrap_or_else(|| "(unknown profile)".to_string());
            (
                GroupIdentity::Profile(profile.clone()),
                UsageReportGroup {
                    key: profile
                        .clone()
                        .unwrap_or_else(|| "unknown-profile".to_string()),
                    label,
                    cutex_session_id: None,
                    profile,
                    provider: None,
                    model: None,
                    reasoning_effort: None,
                    service_tier: None,
                },
            )
        }
        UsageReportGroupBy::Model => {
            let attribution = sample.attribution.clone();
            let identity = GroupIdentity::Model {
                provider: attribution.provider.clone(),
                model: attribution.model.clone(),
                reasoning_effort: attribution.reasoning_effort.clone(),
                service_tier: attribution.service_tier.clone(),
            };
            let provider_model = match (&attribution.provider, &attribution.model) {
                (Some(provider), Some(model)) => format!("{provider}/{model}"),
                (None, Some(model)) => model.clone(),
                (Some(provider), None) => format!("{provider}/(unknown model)"),
                (None, None) => "(unknown model)".to_string(),
            };
            let mut qualifiers = Vec::new();
            if let Some(effort) = &attribution.reasoning_effort {
                qualifiers.push(format!("effort={effort}"));
            }
            if let Some(tier) = &attribution.service_tier {
                qualifiers.push(format!("tier={tier}"));
            }
            let label = if qualifiers.is_empty() {
                provider_model.clone()
            } else {
                format!("{provider_model} [{}]", qualifiers.join(", "))
            };
            (
                identity,
                UsageReportGroup {
                    key: label.clone(),
                    label,
                    cutex_session_id: None,
                    profile: None,
                    provider: attribution.provider,
                    model: attribution.model,
                    reasoning_effort: attribution.reasoning_effort,
                    service_tier: attribution.service_tier,
                },
            )
        }
    }
}

fn add_pricing_gap(
    gaps: &mut BTreeMap<String, GapAccumulator>,
    sample: &UsageSample,
    codex_unpriced: bool,
    api_unpriced: bool,
) -> anyhow::Result<()> {
    let key = serde_json::to_string(&sample.attribution)?;
    let gap = gaps.entry(key).or_insert_with(|| GapAccumulator {
        attribution: sample.attribution.clone(),
        usage_samples: 0,
        tokens: TokenUsageBreakdown::default(),
        codex_credit_unpriced_samples: 0,
        api_equivalent_unpriced_samples: 0,
    });
    gap.usage_samples = checked_count(gap.usage_samples)?;
    gap.tokens.checked_add_assign(&sample.tokens)?;
    if codex_unpriced {
        gap.codex_credit_unpriced_samples = checked_count(gap.codex_credit_unpriced_samples)?;
    }
    if api_unpriced {
        gap.api_equivalent_unpriced_samples = checked_count(gap.api_equivalent_unpriced_samples)?;
    }
    Ok(())
}

fn canonical_timestamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn reset_window_label(window: ResetWindowKind) -> &'static str {
    match window {
        ResetWindowKind::Primary => "primary",
        ResetWindowKind::Secondary => "secondary",
    }
}

fn reset_period_label(start: DateTime<Utc>, end: DateTime<Utc>, window: &str) -> String {
    if start.time() == chrono::NaiveTime::MIN
        && end.time() == chrono::NaiveTime::MIN
        && (end - start).num_days() >= 1
    {
        format!(
            "{}..{} {window}",
            start.format("%Y-%m-%d"),
            end.format("%Y-%m-%d")
        )
    } else {
        format!(
            "{}..{} {window}",
            start.format("%Y-%m-%d %H:%M"),
            end.format("%Y-%m-%d %H:%M")
        )
    }
}

impl RowAccumulator {
    fn finish(self) -> UsageReportRow {
        UsageReportRow {
            period: self.period,
            group: self.group,
            usage_samples: self.usage_samples,
            tokens: self.tokens,
            codex_credits: self.codex_credits.finish(CODEX_CREDIT_RATE_CARD_ID),
            api_equivalent_usd: self.api_equivalent_usd.finish(API_EQUIVALENT_RATE_CARD_ID),
        }
    }
}

impl TotalsAccumulator {
    fn finish(self) -> UsageReportTotals {
        UsageReportTotals {
            usage_samples: self.usage_samples,
            tokens: self.tokens,
            codex_credits: self.codex_credits.finish(CODEX_CREDIT_RATE_CARD_ID),
            api_equivalent_usd: self.api_equivalent_usd.finish(API_EQUIVALENT_RATE_CARD_ID),
        }
    }
}

impl GapAccumulator {
    fn finish(self) -> UsagePricingGap {
        UsagePricingGap {
            attribution: self.attribution,
            usage_samples: self.usage_samples,
            tokens: self.tokens,
            codex_credit_unpriced_samples: self.codex_credit_unpriced_samples,
            api_equivalent_unpriced_samples: self.api_equivalent_unpriced_samples,
        }
    }
}
