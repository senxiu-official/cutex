//! Aggregation behind `GET /v2/usage`: ledger samples bucketed by local day,
//! profile, model, agent, project and/or source (cutex itself or an extra
//! usage source, `external.rs`).
//!
//! Ledger granularity: one `UsageSample` per model request (native
//! `tokenUsage.last`, de-duplicated against the thread's cumulative total),
//! keyed by its event id. A turn is the set of samples sharing
//! `(cutexSessionId, threadId, turnId)`; `turns` counts distinct turns per
//! row. A turn whose requests straddle a bucket boundary (e.g. midnight)
//! counts once in each bucket it touches, and once in `totals`.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;

use chrono::DateTime;
use chrono::NaiveDate;
use chrono::Utc;
use serde::Serialize;

use super::model::parse_timestamp;
use super::model::TokenUsageBreakdown;
use super::model::UsageLedger;
use super::model::UsageLedgerEntry;
use super::model::UsageSample;
use super::estimate::PriceContext;
use super::estimate::PriceOrigin;
use super::pricing::derive_sample_estimates;

pub const MAX_RANGE_DAYS: i64 = 366;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UsageGroupKey {
    Day,
    Profile,
    Model,
    Agent,
    Project,
    Source,
}

impl UsageGroupKey {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "day" => Self::Day,
            "profile" => Self::Profile,
            "model" => Self::Model,
            "agent" => Self::Agent,
            "project" => Self::Project,
            "source" => Self::Source,
            _ => return None,
        })
    }

    /// Parse a comma-separated, non-empty, duplicate-free list.
    pub fn parse_list(value: &str) -> Result<Vec<Self>, String> {
        let mut out = Vec::new();
        for part in value.split(',') {
            let part = part.trim();
            let key = Self::parse(part).ok_or_else(|| {
                format!(
                    "groupBy entries must be day, profile, model, agent, project or source, got {part:?}"
                )
            })?;
            if out.contains(&key) {
                return Err(format!("groupBy lists {part} twice"));
            }
            out.push(key);
        }
        if out.is_empty() {
            return Err("groupBy must name at least one dimension".to_string());
        }
        Ok(out)
    }
}

/// Labels resolved from the session / agent-management stores at query
/// time. Project is the agent's *current* project.
#[derive(Debug, Clone, Default)]
pub struct UsageLabels {
    pub agent_names: HashMap<String, String>,
    /// cutexSessionId -> (projectId, projectName)
    pub projects: HashMap<String, (String, String)>,
    /// Extra usage source id -> its current configuration.
    pub sources: HashMap<String, UsageSourceLabel>,
}

/// Current label / kind / billing of a configured extra usage source.
#[derive(Debug, Clone, Default)]
pub struct UsageSourceLabel {
    pub label: String,
    pub kind: String,
    pub billing: String,
}

/// Source key of rows cutex recorded itself.
pub const CUTEX_SOURCE: &str = "cutex";

#[derive(Debug, Clone)]
pub struct UsageQuery {
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub group_by: Vec<UsageGroupKey>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRowKey {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub day: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<Option<String>>,
    /// With `profile`: the profile id (null when the row cannot be traced).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<Option<String>>,
    /// With `profile`: `current` | `deleted` (no current profile; `profile` is
    /// the historical name). Absent for rows without a profile.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_state: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_name: Option<Option<String>>,
    /// With groupBy `source`: `cutex` or the extra source id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// With `source`: the source's label (null for `cutex`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_label: Option<Option<String>>,
    /// With `source`: `cutex` | `claude-code` | `codex`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<String>,
    /// With `source`: `subscription` | `api` for an extra source (null for
    /// `cutex`). A subscription source's cost is an API-equivalent value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_billing: Option<Option<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageAmounts {
    pub turns: u64,
    /// Model requests (a Claude turn counts its SDK steps when reported).
    pub requests: u64,
    /// Largest input of one model request in the row: the context a step
    /// re-read (a Claude turn's average per step). Token audit 2026-10-22:
    /// the burn is context × requests; this makes a runaway visible.
    pub max_request_input: u64,
    pub input: u64,
    pub cached_input: u64,
    pub cache_write_input: u64,
    pub output: u64,
    pub reasoning_output: u64,
    pub total: u64,
    /// Cost estimate of the priced samples in `currency` (manual table,
    /// price library, built-in rate cards; see `priceSources`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_cost: Option<f64>,
    /// Tokens of samples no price applies to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unpriced_tokens: Option<u64>,
    /// Requests no price applies to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unpriced_requests: Option<u64>,
    /// Where the prices of this row came from: `manual`, `litellm`,
    /// `models.dev`, `builtin`, plus `unpriced` when some samples had none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_sources: Option<Vec<&'static str>>,
    /// Models this row's samples were priced as through the pricing model
    /// map (`modelMap`, capability `usage-model-map`); absent when none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priced_as: Option<Vec<String>>,
    /// Built-in public API-equivalent USD estimate (priced samples only).
    pub api_equivalent_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRow {
    pub key: UsageRowKey,
    /// Copy of `key.profileState` on the row itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_state: Option<&'static str>,
    #[serde(flatten)]
    pub amounts: UsageAmounts,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageQueryResult {
    pub contract_version: u32,
    pub from: String,
    pub to: String,
    pub group_by: Vec<UsageGroupKey>,
    pub totals: UsageAmounts,
    pub rows: Vec<UsageRow>,
    /// Display currency of `estimatedCost` (USD / AUD / CNY).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    /// Price source, display currency, exchange rates used, library cache
    /// state (`estimate::pricing_info`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pricing: Option<serde_json::Value>,
    /// Models in the range without any price (manual, library, built-in).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unpriced_models: Option<Vec<UnpricedModel>>,
    pub recording_since: Option<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnpricedModel {
    pub provider: Option<String>,
    pub model: Option<String>,
    /// `modelMap` target the model is mapped to (which itself has no price).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priced_as: Option<String>,
    pub requests: u64,
    pub tokens: u64,
}

#[derive(Default)]
struct Accumulator {
    turns: HashSet<(String, String, String)>,
    requests: u64,
    max_request_input: u64,
    tokens: TokenUsageBreakdown,
    cost: f64,
    unpriced_tokens: u64,
    unpriced_requests: u64,
    sources: std::collections::BTreeSet<PriceOrigin>,
    priced_as: std::collections::BTreeSet<String>,
    api_usd_nanos: u128,
}

impl Accumulator {
    fn add(&mut self, sample: &UsageSample, cost: Option<(f64, PriceOrigin)>, priced_as: Option<&str>, api_nanos: Option<u128>) {
        if let Some(to) = priced_as {
            if !self.priced_as.contains(to) {
                self.priced_as.insert(to.to_string());
            }
        }
        self.turns.insert((
            sample.cutex_session_id.clone(),
            sample.thread_id.clone(),
            sample.turn_id.clone(),
        ));
        let requests = sample.model_requests.unwrap_or(1).max(1);
        self.requests += requests;
        self.max_request_input = self.max_request_input.max(sample.tokens.input_tokens / requests);
        let t = &mut self.tokens;
        let s = &sample.tokens;
        t.input_tokens = t.input_tokens.saturating_add(s.input_tokens);
        t.cached_input_tokens = t.cached_input_tokens.saturating_add(s.cached_input_tokens);
        t.cache_write_input_tokens = t
            .cache_write_input_tokens
            .saturating_add(s.cache_write_input_tokens);
        t.output_tokens = t.output_tokens.saturating_add(s.output_tokens);
        t.reasoning_output_tokens = t
            .reasoning_output_tokens
            .saturating_add(s.reasoning_output_tokens);
        t.total_tokens = t.total_tokens.saturating_add(s.total_tokens);
        match cost {
            Some((cost, origin)) => {
                self.cost += cost;
                self.sources.insert(origin);
            }
            None => {
                self.unpriced_tokens = self.unpriced_tokens.saturating_add(s.total_tokens);
                self.unpriced_requests += 1;
            }
        }
        if let Some(nanos) = api_nanos {
            self.api_usd_nanos = self.api_usd_nanos.saturating_add(nanos);
        }
    }

    fn finish(&self, with_prices: bool) -> UsageAmounts {
        UsageAmounts {
            turns: self.turns.len() as u64,
            requests: self.requests,
            max_request_input: self.max_request_input,
            input: self.tokens.input_tokens,
            cached_input: self.tokens.cached_input_tokens,
            cache_write_input: self.tokens.cache_write_input_tokens,
            output: self.tokens.output_tokens,
            reasoning_output: self.tokens.reasoning_output_tokens,
            total: self.tokens.total_tokens,
            estimated_cost: with_prices.then_some(round_cost(self.cost)),
            unpriced_tokens: with_prices.then_some(self.unpriced_tokens),
            unpriced_requests: with_prices.then_some(self.unpriced_requests),
            price_sources: with_prices.then(|| {
                self.sources
                    .iter()
                    .map(|origin| origin.key())
                    .chain((self.unpriced_requests > 0).then_some("unpriced"))
                    .collect()
            }),
            priced_as: (!self.priced_as.is_empty()).then(|| self.priced_as.iter().cloned().collect()),
            api_equivalent_usd: round_cost(self.api_usd_nanos as f64 / 1e9),
        }
    }
}

fn round_cost(value: f64) -> f64 {
    (value * 1e6).round() / 1e6
}

/// Aggregate `ledger` for `query`. `local_date` maps a sample instant to the
/// caller's calendar day (the daemon passes `chrono::Local`).
pub fn aggregate_usage(
    ledger: &UsageLedger,
    query: &UsageQuery,
    labels: &UsageLabels,
    pricing: Option<&PriceContext>,
    local_date: &dyn Fn(DateTime<Utc>) -> NaiveDate,
) -> UsageQueryResult {
    let mut unpriced = BTreeMap::<(Option<String>, Option<String>), UnpricedModel>::new();
    let mut rows = BTreeMap::<Vec<Option<String>>, (UsageRowKey, Accumulator)>::new();
    let mut totals = Accumulator::default();
    let mut warnings = ledger.warnings.clone();
    let mut bad_timestamps = 0_u64;

    for entry in &ledger.entries {
        let UsageLedgerEntry::Usage(sample) = entry else {
            continue;
        };
        let Ok(observed) = parse_timestamp(&sample.observed_at) else {
            bad_timestamps += 1;
            continue;
        };
        let day = local_date(observed.with_timezone(&Utc));
        if day < query.from || day > query.to {
            continue;
        }
        let (identity, key) = row_key(&query.group_by, sample, day, labels);
        let (cost, priced_as) = pricing.map(|context| context.price_mapped(sample)).unwrap_or((None, None));
        if pricing.is_some() && cost.is_none() {
            let attribution = &sample.attribution;
            let model = model_key(attribution.model.as_deref());
            let entry = unpriced
                .entry((attribution.provider.clone(), model.clone()))
                .or_insert_with(|| UnpricedModel {
                    provider: attribution.provider.clone(),
                    model,
                    priced_as: priced_as.map(str::to_string),
                    ..UnpricedModel::default()
                });
            entry.requests += 1;
            entry.tokens = entry.tokens.saturating_add(sample.tokens.total_tokens);
        }
        let api_nanos =
            derive_sample_estimates(&sample.attribution, &sample.tokens).api_equivalent_usd_nanos;
        rows.entry(identity)
            .or_insert_with(|| (key, Accumulator::default()))
            .1
            .add(sample, cost, priced_as, api_nanos);
        totals.add(sample, cost, priced_as, api_nanos);
    }
    if bad_timestamps > 0 {
        warnings.push(format!(
            "{bad_timestamps} usage samples had unreadable timestamps"
        ));
    }
    let with_prices = pricing.is_some();
    UsageQueryResult {
        contract_version: 2,
        from: query.from.format("%Y-%m-%d").to_string(),
        to: query.to.format("%Y-%m-%d").to_string(),
        group_by: query.group_by.clone(),
        totals: totals.finish(with_prices),
        rows: rows
            .into_values()
            .map(|(key, acc)| UsageRow {
                profile_state: key.profile_state,
                key,
                amounts: acc.finish(with_prices),
            })
            .collect(),
        currency: pricing.map(|context| context.display.code().to_string()),
        pricing: pricing.map(|context| context.info.clone()),
        unpriced_models: pricing.map(|_| unpriced.into_values().collect()),
        recording_since: None,
        warnings,
    }
}

/// A model's report key: its id lowercased (ASCII), so one model reported in
/// two spellings (the Claude runtime's `GLM-5.3`, Codex's `glm-5.3`) is one
/// row / unpriced entry, as pricing already matches it. Stored samples keep
/// their spelling.
fn model_key(model: Option<&str>) -> Option<String> {
    model.map(str::to_ascii_lowercase)
}

fn row_key(
    group_by: &[UsageGroupKey],
    sample: &UsageSample,
    day: NaiveDate,
    labels: &UsageLabels,
) -> (Vec<Option<String>>, UsageRowKey) {
    let mut identity = Vec::with_capacity(group_by.len());
    let mut key = UsageRowKey::default();
    for dimension in group_by {
        match dimension {
            UsageGroupKey::Day => {
                let value = day.format("%Y-%m-%d").to_string();
                identity.push(Some(value.clone()));
                key.day = Some(value);
            }
            UsageGroupKey::Profile => {
                // One row per profile id (all its names merge); untraceable
                // rows per historical name.
                identity.push(match (&sample.profile_id, &sample.profile) {
                    (Some(id), _) => Some(format!("id:{id}")),
                    (None, Some(name)) => Some(format!("name:{name}")),
                    (None, None) => None,
                });
                key.profile = Some(sample.profile.clone());
                key.profile_id = Some(sample.profile_id.clone());
                key.profile_state = sample.profile.as_ref().map(|_| {
                    if sample.profile_deleted { "deleted" } else { "current" }
                });
            }
            UsageGroupKey::Model => {
                let model = model_key(sample.attribution.model.as_deref());
                identity.push(model.clone());
                key.model = Some(model);
            }
            UsageGroupKey::Agent => {
                let id = sample.cutex_session_id.clone();
                identity.push(Some(id.clone()));
                key.agent_name = Some(
                    labels
                        .agent_names
                        .get(&id)
                        .cloned()
                        .or_else(|| sample.source.as_ref().and_then(|source| source.agent_name.clone()))
                        .unwrap_or_else(|| id.clone()),
                );
                key.agent = Some(id);
            }
            UsageGroupKey::Project => {
                let external = sample.source.as_ref().and_then(|source| {
                    source
                        .project
                        .as_ref()
                        .map(|name| (format!("ext:{}:{name}", source.id), name.clone()))
                });
                let project = labels.projects.get(&sample.cutex_session_id).or(external.as_ref());
                identity.push(project.map(|(id, _)| id.clone()));
                key.project = Some(project.map(|(id, _)| id.clone()));
                key.project_name = Some(project.map(|(_, name)| name.clone()));
            }
            UsageGroupKey::Source => {
                let (id, label, kind, billing) = match &sample.source {
                    None => (CUTEX_SOURCE.to_string(), None, CUTEX_SOURCE.to_string(), None),
                    Some(source) => match labels.sources.get(&source.id) {
                        Some(current) => (
                            source.id.clone(),
                            Some(current.label.clone()),
                            current.kind.clone(),
                            Some(current.billing.clone()),
                        ),
                        None => (
                            source.id.clone(),
                            Some(source.label.clone().unwrap_or_else(|| source.id.clone())),
                            source.kind.clone(),
                            source.billing.clone(),
                        ),
                    },
                };
                identity.push(Some(format!("source:{id}")));
                key.source = Some(id);
                key.source_label = Some(label);
                key.source_kind = Some(kind);
                key.source_billing = Some(billing);
            }
        }
    }
    (identity, key)
}

/// Ledger month files (`YYYY-MM`) that can hold samples whose local day is
/// in `[from, to]`: one day of slack on each side covers any UTC offset.
pub fn month_bounds(from: NaiveDate, to: NaiveDate) -> (String, String) {
    let first = from.pred_opt().unwrap_or(from);
    let last = to.succ_opt().unwrap_or(to);
    (
        first.format("%Y-%m").to_string(),
        last.format("%Y-%m").to_string(),
    )
}
