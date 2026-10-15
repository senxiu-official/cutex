//! Workflow run and step records (design §3–§4.1).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::script::PhaseMeta;

/// Owner-approved defaults (2026-10-14).
pub const DEFAULT_MAX_AGENTS: u32 = 30;
pub const DEFAULT_MAX_CONCURRENCY: u32 = 4;
pub const DEFAULT_MAX_TOKENS: u64 = 2_000_000;
/// Hard ceilings a run request is clamped to.
pub const CEILING_MAX_AGENTS: u32 = 1000;
pub const CEILING_MAX_CONCURRENCY: u32 = 16;
pub const DEFAULT_HOST_MAX_CONCURRENT_STEPS: u32 = 6;
/// Inline result / step result limits.
pub const MAX_RESULT_BYTES: usize = 1024 * 1024;
pub const MAX_STEP_RESULT_BYTES: usize = 256 * 1024;
pub const MAX_LOG_LINES: usize = 2000;
pub const MAX_LOG_BYTES: usize = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RunState {
    /// Waiting for the owner's `workflow_run` approval.
    PendingApproval,
    Running,
    /// Stop requested: the child is gone, open steps are being stopped.
    Stopping,
    Completed,
    /// The script threw / failed a limit it did not catch.
    Errored,
    Stopped,
    /// The owner denied the plan.
    Rejected,
    /// The approval expired (24 h) before a decision.
    Expired,
}

impl RunState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Errored | Self::Stopped | Self::Rejected | Self::Expired)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PendingApproval => "pendingApproval",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Completed => "completed",
            Self::Errored => "errored",
            Self::Stopped => "stopped",
            Self::Rejected => "rejected",
            Self::Expired => "expired",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    /// The owner stopped it.
    User,
    /// The author agent stopped it (`workflow_stop`).
    Model,
    /// The daemon restarted and the run could not continue by itself.
    Interrupted,
    /// A step's account is blocked (quota / login).
    Provider,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum Author {
    Agent {
        cutex_session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Owner,
}

impl Author {
    pub fn session(&self) -> Option<&str> {
        match self {
            Self::Agent { cutex_session_id, .. } => Some(cutex_session_id),
            Self::Owner => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    pub max_agents: u32,
    pub max_concurrency: u32,
    pub max_tokens: u64,
    /// Over budget: also stop running steps (default: let them finish).
    #[serde(default)]
    pub hard_stop: bool,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_agents: DEFAULT_MAX_AGENTS,
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
            max_tokens: DEFAULT_MAX_TOKENS,
            hard_stop: false,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitsRequest {
    #[serde(default, alias = "max_agents")]
    pub max_agents: Option<u32>,
    #[serde(default, alias = "max_concurrency")]
    pub max_concurrency: Option<u32>,
    #[serde(default, alias = "max_tokens")]
    pub max_tokens: Option<u64>,
    #[serde(default, alias = "hard_stop")]
    pub hard_stop: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunError {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepCounts {
    pub total: u32,
    pub queued: u32,
    pub running: u32,
    pub done: u32,
    pub failed: u32,
    pub cached: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRecord {
    pub run_id: String,
    /// Host-wide run number (`#N`).
    pub seq: u64,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub phases: Vec<PhaseMeta>,
    pub author: Author,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub cwd: String,
    pub script_sha256: String,
    #[serde(default)]
    pub args: Value,
    pub args_sha256: String,
    pub limits: Limits,
    /// Profile used by steps without `opts.profile` (none = the author's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<String>,
    /// Profiles steps may use (approved with the plan).
    #[serde(default)]
    pub step_profiles: Vec<String>,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<String>,
    pub state: RunState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RunError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_phase: Option<String>,
    #[serde(default)]
    pub counts: StepCounts,
    /// Tokens (input + output) of all steps.
    #[serde(default)]
    pub tokens: u64,
    /// The script's return value (≤ 1 MiB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// The run whose journal seeds this one's cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft_path: Option<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    /// The tool call's action id (a retried `workflow_start` finds its run).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_action_id: Option<String>,
    /// The author was told (completion notice queued).
    #[serde(default)]
    pub notified: bool,
    #[serde(default)]
    pub revision: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StepState {
    /// Waiting for a concurrency slot.
    Queued,
    /// Creating the step session / its ask.
    Starting,
    Running,
    /// Answered and (if a schema was given) valid.
    Done,
    /// Failed, stopped, interrupted, invalid output: `agent()` returned null.
    Failed,
    /// Served from the predecessor's journal (zero tokens).
    Cached,
}

impl StepState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cached)
    }
    pub fn is_live(self) -> bool {
        matches!(self, Self::Starting | Self::Running)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepRecord {
    pub run_id: String,
    /// `sha256(prompt ‖ canonical opts)[..16] + "#" + occurrence`.
    pub step_key: String,
    /// Order of the agent() call within this execution.
    pub call_index: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub prompt: String,
    /// Canonical options (without label / phase).
    #[serde(default)]
    pub opts: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask_id: Option<String>,
    /// Asks of this step, oldest first (the last one may be a repair).
    #[serde(default)]
    pub ask_ids: Vec<String>,
    pub state: StepState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RunError>,
    #[serde(default)]
    pub tokens: u64,
    #[serde(default)]
    pub repairs: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_from: Option<String>,
    /// The last progress line of the step's ask (activity).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogLine {
    pub seq: u64,
    pub at: String,
    pub level: String,
    pub text: String,
}

pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Canonical JSON (sorted keys) for digests.
pub fn canonical(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let inner: Vec<String> = keys
                .iter()
                .map(|k| format!("{}:{}", serde_json::to_string(k).unwrap_or_default(), canonical(&map[*k])))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        Value::Array(items) => format!("[{}]", items.iter().map(canonical).collect::<Vec<_>>().join(",")),
        other => other.to_string(),
    }
}

pub fn run_counts(steps: &[StepRecord]) -> StepCounts {
    let mut counts = StepCounts::default();
    for step in steps {
        counts.total += 1;
        match step.state {
            StepState::Queued => counts.queued += 1,
            StepState::Starting | StepState::Running => counts.running += 1,
            StepState::Done => counts.done += 1,
            StepState::Failed => counts.failed += 1,
            StepState::Cached => counts.cached += 1,
        }
    }
    counts
}
