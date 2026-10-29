use std::collections::HashMap;

use anyhow::Context;
use chrono::DateTime;
use chrono::FixedOffset;
use serde::Deserialize;
use serde::Serialize;

use crate::management::v2::model::EventEnvelope;
use crate::management::v2::model::MAX_SAFE_SEQUENCE;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsageBreakdown {
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    #[serde(default)]
    pub cache_write_input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub reasoning_output_tokens: u64,
}

impl TokenUsageBreakdown {
    pub(super) fn validate(&self) -> anyhow::Result<()> {
        for (name, value) in [
            ("totalTokens", self.total_tokens),
            ("inputTokens", self.input_tokens),
            ("cachedInputTokens", self.cached_input_tokens),
            ("cacheWriteInputTokens", self.cache_write_input_tokens),
            ("outputTokens", self.output_tokens),
            ("reasoningOutputTokens", self.reasoning_output_tokens),
        ] {
            if value > MAX_SAFE_SEQUENCE {
                anyhow::bail!("usage {name} exceeds the JSON-safe range");
            }
        }
        Ok(())
    }

    pub(super) fn detailed_is_zero(&self) -> bool {
        self.input_tokens == 0
            && self.cached_input_tokens == 0
            && self.cache_write_input_tokens == 0
            && self.output_tokens == 0
            && self.reasoning_output_tokens == 0
    }

    pub(super) fn has_regression_from(&self, previous: &Self) -> bool {
        self.total_tokens < previous.total_tokens
            || self.input_tokens < previous.input_tokens
            || self.cached_input_tokens < previous.cached_input_tokens
            || self.cache_write_input_tokens < previous.cache_write_input_tokens
            || self.output_tokens < previous.output_tokens
            || self.reasoning_output_tokens < previous.reasoning_output_tokens
    }

    pub(super) fn checked_difference(&self, previous: &Self) -> anyhow::Result<Self> {
        Ok(Self {
            total_tokens: self
                .total_tokens
                .checked_sub(previous.total_tokens)
                .context("usage totalTokens regressed")?,
            input_tokens: self
                .input_tokens
                .checked_sub(previous.input_tokens)
                .context("usage inputTokens regressed")?,
            cached_input_tokens: self
                .cached_input_tokens
                .checked_sub(previous.cached_input_tokens)
                .context("usage cachedInputTokens regressed")?,
            cache_write_input_tokens: self
                .cache_write_input_tokens
                .checked_sub(previous.cache_write_input_tokens)
                .context("usage cacheWriteInputTokens regressed")?,
            output_tokens: self
                .output_tokens
                .checked_sub(previous.output_tokens)
                .context("usage outputTokens regressed")?,
            reasoning_output_tokens: self
                .reasoning_output_tokens
                .checked_sub(previous.reasoning_output_tokens)
                .context("usage reasoningOutputTokens regressed")?,
        })
    }

    pub fn checked_add_assign(&mut self, delta: &Self) -> anyhow::Result<()> {
        self.total_tokens = checked_usage_sum(self.total_tokens, delta.total_tokens)?;
        self.input_tokens = checked_usage_sum(self.input_tokens, delta.input_tokens)?;
        self.cached_input_tokens =
            checked_usage_sum(self.cached_input_tokens, delta.cached_input_tokens)?;
        self.cache_write_input_tokens = checked_usage_sum(
            self.cache_write_input_tokens,
            delta.cache_write_input_tokens,
        )?;
        self.output_tokens = checked_usage_sum(self.output_tokens, delta.output_tokens)?;
        self.reasoning_output_tokens =
            checked_usage_sum(self.reasoning_output_tokens, delta.reasoning_output_tokens)?;
        Ok(())
    }
}

pub(super) fn checked_usage_sum(current: u64, delta: u64) -> anyhow::Result<u64> {
    current
        .checked_add(delta)
        .filter(|value| *value <= MAX_SAFE_SEQUENCE)
        .context("usage aggregate exceeds the JSON-safe range")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub struct UsageAttribution {
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// Upstream model captured from this runtime's catalog, never a live profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub service_tier: Option<String>,
}

impl UsageAttribution {
    pub(super) fn pricing_model(&self) -> Option<&str> {
        self.request_model.as_deref().or(self.model.as_deref())
    }

    pub(super) fn validate(&self) -> anyhow::Result<()> {
        for (name, value) in [
            ("provider", self.provider.as_deref()),
            ("model", self.model.as_deref()),
            ("requestModel", self.request_model.as_deref()),
            ("reasoningEffort", self.reasoning_effort.as_deref()),
            ("serviceTier", self.service_tier.as_deref()),
        ] {
            validate_optional_label(&format!("usage attribution {name}"), value)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UsageTokenSource {
    NativeLast,
    CumulativeDelta,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ResetWindowKind {
    Primary,
    Secondary,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UsageLedgerEntry {
    Usage(Box<UsageSample>),
    ResetBoundary(UsageResetBoundary),
}

impl UsageLedgerEntry {
    pub fn entry_id(&self) -> &str {
        match self {
            Self::Usage(value) => &value.entry_id,
            Self::ResetBoundary(value) => &value.entry_id,
        }
    }

    pub fn observed_at(&self) -> &str {
        match self {
            Self::Usage(value) => &value.observed_at,
            Self::ResetBoundary(value) => &value.observed_at,
        }
    }

    pub(super) fn validate(&self) -> anyhow::Result<()> {
        match self {
            Self::Usage(value) => value.validate(),
            Self::ResetBoundary(value) => value.validate(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UsageSample {
    pub entry_id: String,
    pub event_id: String,
    pub observed_at: String,
    pub host_id: String,
    pub cutex_session_id: String,
    pub thread_id: String,
    pub turn_id: String,
    pub runtime_generation: u64,
    #[serde(default)]
    pub profile: Option<String>,
    /// Stable profile id (r218+); `profile` is the name at the time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    /// Query-time attribution only (`canonicalize_ledger_profiles`): the
    /// row maps to no current profile. Never stored.
    #[serde(skip)]
    pub profile_deleted: bool,
    /// Set only on rows imported from an extra usage source (`external.rs`);
    /// absent = the row came from cutex itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<UsageSampleSource>,
    #[serde(flatten)]
    pub attribution: UsageAttribution,
    pub token_source: UsageTokenSource,
    #[serde(default)]
    pub model_context_window: Option<u64>,
    #[serde(default)]
    pub compaction_count: Option<u64>,
    /// Model requests this row covers when it is not one (a Claude turn
    /// reports its SDK steps, `tokenUsage.modelRequests`); absent = 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_requests: Option<u64>,
    pub tokens: TokenUsageBreakdown,
    pub cumulative_total: TokenUsageBreakdown,
}

/// Origin of a row imported from an extra usage source. `label` /
/// `billing` are the values at import time; reports prefer the source's
/// current configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UsageSampleSource {
    pub id: String,
    /// `claude-code` | `codex`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// `subscription` | `api`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billing: Option<String>,
    /// Agent shown for the row (`Claude Code · <project>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    /// Project (working directory name) the run was in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

impl UsageSampleSource {
    fn validate(&self) -> anyhow::Result<()> {
        validate_non_empty("usage source id", &self.id)?;
        validate_non_empty("usage source kind", &self.kind)?;
        for (name, value) in [
            ("label", self.label.as_deref()),
            ("billing", self.billing.as_deref()),
            ("agentName", self.agent_name.as_deref()),
            ("project", self.project.as_deref()),
        ] {
            validate_optional_label(&format!("usage source {name}"), value)?;
        }
        Ok(())
    }
}

impl UsageSample {
    fn validate(&self) -> anyhow::Result<()> {
        validate_non_empty("usage entryId", &self.entry_id)?;
        validate_non_empty("usage eventId", &self.event_id)?;
        parse_timestamp(&self.observed_at).context("invalid usage observedAt")?;
        validate_non_empty("usage hostId", &self.host_id)?;
        validate_non_empty("usage cutexSessionId", &self.cutex_session_id)?;
        validate_non_empty("usage threadId", &self.thread_id)?;
        validate_non_empty("usage turnId", &self.turn_id)?;
        if self.runtime_generation == 0 || self.runtime_generation > MAX_SAFE_SEQUENCE {
            anyhow::bail!("usage runtimeGeneration is outside the JSON-safe range");
        }
        validate_optional_label("usage profile", self.profile.as_deref())?;
        if let Some(source) = &self.source {
            source.validate()?;
        }
        self.attribution.validate()?;
        if self
            .model_context_window
            .is_some_and(|value| value == 0 || value > MAX_SAFE_SEQUENCE)
        {
            anyhow::bail!("usage modelContextWindow is outside the JSON-safe range");
        }
        if self
            .compaction_count
            .is_some_and(|value| value > MAX_SAFE_SEQUENCE)
        {
            anyhow::bail!("usage compactionCount is outside the JSON-safe range");
        }
        self.tokens.validate()?;
        self.cumulative_total.validate()?;
        if self.tokens.detailed_is_zero() {
            anyhow::bail!("usage sample must contain detailed token activity");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UsageResetBoundary {
    pub entry_id: String,
    pub event_id: String,
    pub observed_at: String,
    pub host_id: String,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default)]
    pub limit_id: Option<String>,
    pub window_kind: ResetWindowKind,
    pub window_duration_mins: i64,
    pub resets_at: i64,
}

impl UsageResetBoundary {
    fn validate(&self) -> anyhow::Result<()> {
        validate_non_empty("reset entryId", &self.entry_id)?;
        validate_non_empty("reset eventId", &self.event_id)?;
        parse_timestamp(&self.observed_at).context("invalid reset observedAt")?;
        validate_non_empty("reset hostId", &self.host_id)?;
        validate_optional_label("reset profile", self.profile.as_deref())?;
        validate_optional_label("reset limitId", self.limit_id.as_deref())?;
        if self.window_duration_mins <= 0 {
            anyhow::bail!("reset windowDurationMins must be positive");
        }
        if self.resets_at <= 0 {
            anyhow::bail!("reset resetsAt must be positive");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageStateSnapshot {
    pub revision: u64,
    pub observed_since: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageLedger {
    pub entries: Vec<UsageLedgerEntry>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) struct EventPosition {
    pub runtime_generation: u64,
    pub stream_id: String,
    pub sequence: u64,
    pub received_at: String,
    pub event_id: String,
}

impl EventPosition {
    pub(super) fn from_envelope(envelope: &EventEnvelope) -> anyhow::Result<Self> {
        let runtime_generation = envelope
            .correlation
            .runtime_generation
            .filter(|generation| *generation > 0 && *generation <= MAX_SAFE_SEQUENCE)
            .context("usage event requires a positive JSON-safe runtime generation")?;
        Ok(Self {
            runtime_generation,
            stream_id: envelope.stream_id.clone(),
            sequence: envelope.sequence,
            received_at: envelope.received_at.clone(),
            event_id: envelope.event_id.clone(),
        })
    }

    pub(super) fn validate(&self) -> anyhow::Result<()> {
        if self.runtime_generation == 0 || self.runtime_generation > MAX_SAFE_SEQUENCE {
            anyhow::bail!("usage event position has an invalid runtimeGeneration");
        }
        if self.sequence == 0 || self.sequence > MAX_SAFE_SEQUENCE {
            anyhow::bail!("usage event position has an invalid sequence");
        }
        validate_non_empty("usage event streamId", &self.stream_id)?;
        validate_non_empty("usage event eventId", &self.event_id)?;
        parse_timestamp(&self.received_at).context("invalid usage event receivedAt")?;
        Ok(())
    }

    pub(super) fn is_newer_than(&self, current: &Self) -> anyhow::Result<bool> {
        if self.runtime_generation != current.runtime_generation {
            return Ok(self.runtime_generation > current.runtime_generation);
        }
        if self.stream_id == current.stream_id {
            return Ok(self.sequence > current.sequence);
        }
        Ok(parse_timestamp(&self.received_at)? > parse_timestamp(&current.received_at)?)
    }
}

/// Only the current/latest turn is retained, so settings changes during that
/// turn cannot rewrite the model that actually produced its usage.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UsageActiveTurn {
    pub turn_id: String,
    pub runtime_generation: u64,
    pub attribution: UsageAttribution,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UsageCounterState {
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default)]
    pub attribution: UsageAttribution,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_turn: Option<UsageActiveTurn>,
    #[serde(default)]
    pub rerouted_models: HashMap<String, String>,
    #[serde(default)]
    pub previous_total: Option<TokenUsageBreakdown>,
    #[serde(default)]
    pub counter_epoch: u64,
    #[serde(default)]
    pub position: Option<EventPosition>,
}

impl UsageCounterState {
    fn validate(&self) -> anyhow::Result<()> {
        validate_optional_label("usage counter profile", self.profile.as_deref())?;
        validate_optional_label("usage attribution source", self.attribution_source.as_deref())?;
        self.attribution.validate()?;
        if let Some(turn) = &self.active_turn {
            validate_non_empty("usage active turnId", &turn.turn_id)?;
            if turn.runtime_generation > MAX_SAFE_SEQUENCE {
                anyhow::bail!("usage active turn generation exceeds the JSON-safe range");
            }
            turn.attribution.validate()?;
        }
        for (turn_id, model) in &self.rerouted_models {
            validate_non_empty("usage reroute turnId", turn_id)?;
            validate_non_empty("usage reroute model", model)?;
        }
        if let Some(total) = &self.previous_total {
            total.validate()?;
        }
        if self.counter_epoch > MAX_SAFE_SEQUENCE {
            anyhow::bail!("usage counter epoch exceeds the JSON-safe range");
        }
        if let Some(position) = &self.position {
            position.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SessionUsageState {
    #[serde(default)]
    pub threads: HashMap<String, UsageCounterState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) struct ResetWindowState {
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default)]
    pub limit_id: Option<String>,
    pub window_kind: ResetWindowKind,
    #[serde(default)]
    pub window_duration_mins: Option<i64>,
    #[serde(default)]
    pub resets_at: Option<i64>,
    #[serde(default)]
    pub emitted_window_duration_mins: Option<i64>,
    #[serde(default)]
    pub emitted_resets_at: Option<i64>,
    #[serde(default)]
    pub last_observed_at: Option<String>,
}

impl ResetWindowState {
    fn validate(&self) -> anyhow::Result<()> {
        validate_optional_label("reset state profile", self.profile.as_deref())?;
        validate_optional_label("reset state limitId", self.limit_id.as_deref())?;
        for (name, value) in [
            ("windowDurationMins", self.window_duration_mins),
            ("resetsAt", self.resets_at),
            (
                "emittedWindowDurationMins",
                self.emitted_window_duration_mins,
            ),
            ("emittedResetsAt", self.emitted_resets_at),
        ] {
            if value.is_some_and(|value| value <= 0) {
                anyhow::bail!("reset state {name} must be positive");
            }
        }
        if let Some(value) = &self.last_observed_at {
            parse_timestamp(value).context("invalid reset state lastObservedAt")?;
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UsageStateFile {
    #[serde(default = "usage_state_version")]
    pub version: u8,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub observed_since: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub sessions: HashMap<String, SessionUsageState>,
    /// Session IDs are never reused. Keep deletion fences so delayed native
    /// events cannot recreate a baseline and charge the whole counter again.
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub deleted_sessions: std::collections::BTreeSet<String>,
    #[serde(default)]
    pub reset_windows: Vec<ResetWindowState>,
    #[serde(default)]
    pub pending_entries: Vec<UsageLedgerEntry>,
}

impl Default for UsageStateFile {
    fn default() -> Self {
        Self {
            version: usage_state_version(),
            revision: 0,
            observed_since: None,
            updated_at: None,
            sessions: HashMap::new(),
            deleted_sessions: Default::default(),
            reset_windows: Vec::new(),
            pending_entries: Vec::new(),
        }
    }
}

impl UsageStateFile {
    pub(super) fn validate(&self) -> anyhow::Result<()> {
        if self.version != usage_state_version() {
            anyhow::bail!("unsupported management v2 usage version: {}", self.version);
        }
        if self.revision > MAX_SAFE_SEQUENCE {
            anyhow::bail!("usage revision exceeds the JSON-safe range");
        }
        if let Some(value) = &self.observed_since {
            parse_timestamp(value).context("invalid usage observedSince")?;
        }
        if let Some(value) = &self.updated_at {
            parse_timestamp(value).context("invalid usage updatedAt")?;
        }
        for (cutex_session_id, session) in &self.sessions {
            validate_non_empty("usage cutexSessionId", cutex_session_id)?;
            for (thread_id, state) in &session.threads {
                validate_non_empty("usage threadId", thread_id)?;
                state.validate().with_context(|| {
                    format!("invalid usage counter for {cutex_session_id}/{thread_id}")
                })?;
            }
        }
        for state in &self.reset_windows {
            state.validate()?;
        }
        for entry in &self.pending_entries {
            entry.validate()?;
        }
        Ok(())
    }

    pub(super) fn snapshot(&self) -> UsageStateSnapshot {
        UsageStateSnapshot {
            revision: self.revision,
            observed_since: self.observed_since.clone(),
            updated_at: self.updated_at.clone(),
        }
    }

    pub(super) fn mark_changed(&mut self, received_at: &str) -> anyhow::Result<()> {
        self.revision = self
            .revision
            .checked_add(1)
            .filter(|revision| *revision <= MAX_SAFE_SEQUENCE)
            .context("usage revision exhausted")?;
        merge_earliest_timestamp(&mut self.observed_since, received_at)?;
        merge_latest_timestamp(&mut self.updated_at, received_at)?;
        Ok(())
    }
}

pub(super) fn usage_state_version() -> u8 {
    1
}

pub(super) fn normalize_label(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

pub(super) fn validate_non_empty(label: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty() {
        anyhow::bail!("{label} must not be empty");
    }
    Ok(())
}

fn validate_optional_label(label: &str, value: Option<&str>) -> anyhow::Result<()> {
    if value.is_some_and(str::is_empty) {
        anyhow::bail!("{label} must not be empty");
    }
    Ok(())
}

pub(super) fn parse_timestamp(value: &str) -> anyhow::Result<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(value).map_err(anyhow::Error::from)
}

fn merge_earliest_timestamp(current: &mut Option<String>, candidate: &str) -> anyhow::Result<bool> {
    let candidate_time = parse_timestamp(candidate)?;
    if let Some(value) = current {
        if candidate_time >= parse_timestamp(value)? {
            return Ok(false);
        }
    }
    *current = Some(candidate.to_string());
    Ok(true)
}

fn merge_latest_timestamp(current: &mut Option<String>, candidate: &str) -> anyhow::Result<bool> {
    let candidate_time = parse_timestamp(candidate)?;
    if let Some(value) = current {
        if candidate_time <= parse_timestamp(value)? {
            return Ok(false);
        }
    }
    *current = Some(candidate.to_string());
    Ok(true)
}
