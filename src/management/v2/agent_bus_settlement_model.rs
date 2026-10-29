//! Owner follow-up settlement, separate from delivery and context receipts.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const FEATURE: &str = "agent-bus-message-settlement-v1";
pub const CLOSE_PATH: &str = "/v2/agent-bus/stuck-messages/close";
pub const EVIDENCE_TTL_SECS: i64 = 180;
pub const EVENT_METHOD: &str = "cutex/agentBus/messageSettlementUpdated";

/// Observations from an authenticated connection to an already-running adapter.
/// This is assignment/terminal evidence, never proof of delivery or no effects.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Evidence {
    pub schema_version: u8,
    pub runtime: String,
    pub recipient_id: String,
    pub runtime_generation: u64,
    /// Current read-only connection occurrence; runtime_generation above is
    /// the immutable original submission occurrence. Older evidence used both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observing_generation: Option<u64>,
    pub thread_id: String,
    pub message_id: String,
    pub semantic_sha256: String,
    pub local_semantic_sha256: String,
    pub attempt_id: String,
    pub processing_state: String,
    pub actual_turn_id: String,
    pub terminal_status: String,
    pub observed_at: DateTime<Utc>,
    /// Digest of the validated native/local/history observations, without text.
    pub observation_sha256: String,
}

impl Evidence {
    pub fn observer_generation(&self) -> u64 {self.observing_generation.unwrap_or(self.runtime_generation)}
    pub fn validate_shape(&self) -> anyhow::Result<()> {
        let bounded = |value: &str| !value.trim().is_empty() && value.len() <= 512;
        let digest =
            |value: &str| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit());
        anyhow::ensure!(
            self.schema_version == 1
                && self.runtime == "claude"
                && self.runtime_generation <= 9_007_199_254_740_991
                && self.observer_generation() <= 9_007_199_254_740_991
                && [
                    &self.recipient_id,
                    &self.thread_id,
                    &self.message_id,
                    &self.actual_turn_id
                ]
                .into_iter()
                .all(|value| bounded(value))
                && [
                    &self.semantic_sha256,
                    &self.local_semantic_sha256,
                    &self.observation_sha256
                ]
                .into_iter()
                .all(|value| digest(value))
                && uuid::Uuid::parse_str(&self.attempt_id).is_ok_and(|id| !id.is_nil())
                && matches!(self.processing_state.as_str(), "held" | "output_observed")
                && matches!(
                    self.terminal_status.as_str(),
                    "completed" | "interrupted" | "failed"
                ),
            "invalid settlement evidence shape"
        );
        Ok(())
    }

    pub fn same_identity(&self, other: &Self) -> bool {
        let mut left = self.clone();
        let mut right = other.clone();
        left.observed_at = DateTime::<Utc>::UNIX_EPOCH;
        right.observed_at = DateTime::<Utc>::UNIX_EPOCH;
        left == right
    }

    pub fn fresh(&self, now: DateTime<Utc>) -> bool {
        let age = now.signed_duration_since(self.observed_at);
        age >= chrono::Duration::zero() && age <= chrono::Duration::seconds(EVIDENCE_TTL_SECS)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Closure {
    pub actor: String,
    pub closed_at: DateTime<Utc>,
    pub reason: String,
    pub request_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceView {
    pub runtime: String,
    pub runtime_generation: u64,
    pub thread_id: String,
    pub actual_turn_id: String,
    pub terminal_status: String,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SettlementView {
    pub state: String,
    pub can_close: bool,
    pub evidence_token: String,
    pub evidence: EvidenceView,
    pub closure: Option<Closure>,
}

/// Kept in the same durable message row as original identity/delivery facts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SettlementRecord {
    pub evidence_token: String,
    pub evidence: Evidence,
    pub closure: Option<Closure>,
    #[serde(default)]
    pub request: Option<CloseRequest>,
    /// Immutable close audit, committed atomically with closure and outbox.
    #[serde(default)]
    pub audit: Option<Value>,
    /// Per-session outbox. A publication crash can repeat an event; requestId
    /// identifies it, and replay must not perform another business transition.
    #[serde(default)]
    pub pending_sessions: Vec<String>,
}

impl SettlementRecord {
    pub fn view(&self, eligible: bool, now: DateTime<Utc>) -> SettlementView {
        SettlementView {
            state: if self.closure.is_some() {
                "closed"
            } else {
                "outcome_unknown"
            }
            .into(),
            can_close: self.closure.is_none() && eligible && self.evidence.fresh(now),
            evidence_token: self.evidence_token.clone(),
            evidence: EvidenceView {
                runtime: self.evidence.runtime.clone(),
                runtime_generation: self.evidence.runtime_generation,
                thread_id: self.evidence.thread_id.clone(),
                actual_turn_id: self.evidence.actual_turn_id.clone(),
                terminal_status: self.evidence.terminal_status.clone(),
                observed_at: self.evidence.observed_at,
            },
            closure: self.closure.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CloseRequest {
    pub message_id: String,
    pub expected_evidence_token: String,
    pub request_id: String,
    pub reason: String,
}

impl CloseRequest {
    pub fn validate(&self) -> Result<(), Refusal> {
        let bounded = |value: &str, max| !value.trim().is_empty() && value.len() <= max;
        if !bounded(&self.message_id, 512)
            || !bounded(&self.expected_evidence_token, 256)
            || !bounded(&self.request_id, 128)
            || !bounded(&self.reason, 1024)
        {
            return Err(Refusal::new(
                400,
                "invalid_request",
                "messageId, expectedEvidenceToken, requestId and reason are required and bounded",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CloseOutcome {
    pub message_id: String,
    pub changed: bool,
    pub settlement: SettlementView,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub code: String,
    pub message: String,
}
impl Refusal {
    pub fn new(status: u16, code: &str, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Observation {
    Terminal(Evidence),
    Ineligible,
}

pub type Collector =
    fn(&super::agent_bus_state::AgentBusMessageSnapshot) -> anyhow::Result<Observation>;
pub type RuntimeFence = fn(&Evidence) -> anyhow::Result<bool>;
static COLLECTOR: std::sync::OnceLock<Collector> = std::sync::OnceLock::new();
static RUNTIME_FENCE: std::sync::OnceLock<RuntimeFence> = std::sync::OnceLock::new();
pub fn register_runtime_fence(fence: RuntimeFence) {
    let _ = RUNTIME_FENCE.set(fence);
}
/// Called under the message write lock. This callback must inspect only the
/// existing runtime/session identity, never reenter the message repository.
pub fn runtime_matches(evidence: &Evidence) -> anyhow::Result<bool> {
    RUNTIME_FENCE
        .get()
        .ok_or_else(|| anyhow::anyhow!("settlement runtime fence unavailable"))?(evidence)
}
pub fn register_collector(collector: Collector) {
    let _ = COLLECTOR.set(collector);
}
pub fn collect(
    snapshot: &super::agent_bus_state::AgentBusMessageSnapshot,
) -> anyhow::Result<Observation> {
    COLLECTOR
        .get()
        .ok_or_else(|| anyhow::anyhow!("settlement observer unavailable"))?(snapshot)
}
