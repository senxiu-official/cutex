//! Ask records (asks design §5.1, §5.2). All records are durable in the ask
//! store and survive restart. Ask bodies, attachments and options are
//! immutable once created; a different request is a new ask (a follow-up).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Who asked: an agent (authenticated durable session) or the owner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum Requester {
    Peer { peer_id: String, cutex_session_id: String, name: Option<String>, label: String },
    Agent {
        cutex_session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Owner,
}

impl Requester {
    pub fn session(&self) -> Option<&str> {
        match self {
            Self::Agent { cutex_session_id, .. } => Some(cutex_session_id),
            Self::Owner | Self::Peer { .. } => None,
        }
    }

    pub fn is_owner(&self) -> bool {
        matches!(self, Self::Owner)
    }

    /// Display name for texts addressed to the target.
    pub fn label(&self) -> String {
        match self {
            Self::Agent { name: Some(name), .. } => name.clone(),
            Self::Agent { cutex_session_id, .. } => cutex_session_id.clone(),
            Self::Owner => "the owner".into(),
            Self::Peer { name, cutex_session_id, label, .. } => format!("{}@{}", crate::peers::wire::sanitize_name(name.as_deref().unwrap_or(cutex_session_id)), crate::peers::wire::sanitize_name(label)),
        }
    }
}

/// `ifBusy` (§5.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum IfBusy {
    #[default]
    Queue,
    Fail,
}

/// Where questions raised in the ask's turns go first (§5.9).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum QuestionRoute {
    #[default]
    Requester,
    Owner,
}

/// How an unclaimed answer reaches the requester (§5.7).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Notify {
    /// Starts (or joins after) a requester turn as an ANSWER event.
    #[default]
    Turn,
    /// Kept for the next `agent_wait` / `agent_list`.
    Passive,
}

/// Handoff context (§8.6).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum ContextOption {
    None,
    #[default]
    Brief,
    /// The requester's last N turns rendered into an artifact.
    Recent { turns: u32 },
    /// Session fork (same provider only); falls back to `recent` when the
    /// source has unfinished obligations (§5.11).
    Fork,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AskOptions {
    #[serde(default)]
    pub if_busy: IfBusy,
    #[serde(default)]
    pub review: bool,
    #[serde(default)]
    pub questions: QuestionRoute,
    #[serde(default)]
    pub notify: Notify,
    #[serde(default)]
    pub context: ContextOption,
}

/// The immutable request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AskBody {
    /// Up to the inline brief cap; the full text is in `textArtifact` when
    /// longer.
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_artifact: Option<String>,
    pub text_bytes: usize,
    pub text_sha256: String,
    /// Artifact ids (content-addressed, §8.6).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<String>,
}

/// Execution state of the ask, driven by its current attempt (§5.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExecutionState {
    PendingReview,
    Queued,
    Admitted,
    Running,
    Answered,
    Failed,
    Interrupted,
    Cancelled,
    Rejected,
}

impl ExecutionState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Answered | Self::Failed | Self::Interrupted | Self::Cancelled | Self::Rejected
        )
    }

    /// Unfinished native obligation on the target (blocks fork/revert, §5.11).
    pub fn in_runtime(self) -> bool {
        matches!(self, Self::Admitted | Self::Running)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InterruptReason {
    PeerRemoved,
    PeerDisabled,
    PeersDisabled,
    RemovedByPeer,
    PeerStripped,
    /// The requester stopped it.
    Stopped,
    OwnerStopped,
    RuntimeRestart,
    OutcomeUnknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BlockedReason {
    Login,
    Quota,
    RuntimeDown,
    Budget,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PausedBy {
    Owner,
    PlanMode,
}

/// Zero or one hold reason on `queued` / `admitted` / `running` (§5.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "reason")]
pub enum Hold {
    PeerUnreachable,
    PeerIdentityChanged,
    PeerVersionMismatch,
    PeerUnknown { raw: String },
    TargetBusy {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        position: Option<usize>,
    },
    Paused {
        by: PausedBy,
    },
    HeldByRuntime {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        native: Option<String>,
    },
    Blocked {
        by: BlockedReason,
    },
    WaitingForApproval,
    WaitingForInput,
    /// A pending profile switch is applied before the next admission (§5.11).
    ProfileSwitchPending,
    /// The target runtime lacks the exclusive admission hook (N1); asks
    /// wait rather than share a turn.
    RuntimeUnsupported {
        missing: String,
    },
    /// The handler yielded its turn with `delegation_await` and waits for
    /// bound jobs (delegation-await contract §2). Execution stays
    /// `running`; no lease is held.
    WaitingForEvent {
        jobs: Vec<String>,
        until: AwaitUntil,
        since: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_at: Option<String>,
    },
}

impl Hold {
    /// Holds that wake a waiting requester immediately (§5.8).
    pub fn wakes_waiters(&self) -> bool {
        match self {
            // A live runtime this process reconnects to: transient.
            Self::HeldByRuntime { native } => native.as_deref() != Some(HOLD_RECONNECTING),
            Self::Paused { .. } | Self::Blocked { .. } | Self::RuntimeUnsupported { .. } | Self::PeerIdentityChanged | Self::PeerVersionMismatch | Self::PeerUnknown { .. } => true,
            _ => false,
        }
    }
}

/// `heldByRuntime.native` while the management process reconnects to a
/// runtime another cutex process started (r208).
pub const HOLD_RECONNECTING: &str = "reconnecting";

/// Review dimension, only with `review: true` (§5.12).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReviewState {
    AwaitingAcceptance,
    Accepted,
    ChangesRequested,
    Abandoned,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CloseOutcome {
    /// A non-review ask whose execution ended.
    Completed,
    Accepted,
    Abandoned,
    Stopped,
    Cancelled,
    /// A follow-up replaced the answered delegation (`supersededBy`).
    Superseded,
}

/// The gate decision for the ask (§4.5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GateRecord {
    /// `caps(target) ⊄ caps(requester)` at the last evaluation.
    pub required: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub differences: Vec<Value>,
    /// Digest of the immutable snapshot the approval is bound to.
    pub snapshot_sha256: String,
    /// The versions in the snapshot (shown as the difference when stale).
    #[serde(default)]
    pub inputs: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<String>,
    /// The approval for `snapshot_sha256` was consumed at admission.
    #[serde(default)]
    pub admitted_with_approval: bool,
    /// Why the previous approval no longer applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_reason: Option<String>,
}

/// Activity of the attempt's turn only (§5.5).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Activity {
    /// Evidence from this attempt's observed tool/output, never a heartbeat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_at: Option<String>,
    #[serde(default)]
    pub tool_calls: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AttemptState {
    /// Written before native submission (message id chosen).
    Submitting,
    /// Native receipt seen, turn not observed yet.
    Admitted,
    Running,
    Answered,
    Failed,
    Interrupted,
    /// Native refused with `busy` (N1): nothing ran; the ask is queued again.
    Refused,
    /// The turn ended after `delegation_await`: a stage, not the answer
    /// (delegation-await contract §2.2).
    Yielded,
}

impl AttemptState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Answered | Self::Failed | Self::Interrupted | Self::Refused | Self::Yielded)
    }
}

/// A stop belongs to one original occurrence; a claimed dispatch is never replayed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttemptStopIntent {
    pub cutex_session_id: String,
    pub attempt: u32,
    pub runtime_generation: u64,
    pub native_message_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub requested_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_at: Option<String>,
    pub outcome: StopDispatchOutcome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopDispatchOutcome { Pending, Claimed, Submitted, Unknown, Unsupported, Stale }

/// One execution of an ask = one native external-input occurrence = one turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttemptRecord {
    /// 1-based; the attempt id is `<askId>#<n>`.
    pub n: u32,
    /// Native message id, chosen by cutex before submission.
    pub native_message_id: String,
    /// The exact ASK event text (artifact), so recovery can query or
    /// resubmit the identical native key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_artifact: Option<String>,
    pub runtime_generation: u64,
    pub state: AttemptState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupt_reason: Option<InterruptReason>,
    /// Target profile/model snapshot at turn start (§5.11).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The turn carried other turn-triggering input (§5.3).
    #[serde(default)]
    pub shared_turn: bool,
    #[serde(default)]
    pub owner_steered: bool,
    /// Passive notes submitted with this attempt (tells).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub included_notes: Vec<String>,
    #[serde(default)]
    pub activity: Activity,
    /// The last final agent message seen in the turn (artifact).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message_artifact: Option<String>,
    /// The last `turn/diff/updated` diff (artifact).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_diff_artifact: Option<String>,
    /// The last token usage reported for the turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_usage: Option<Value>,
    /// The requester asked to stop it (`ask_close(stop)`).
    #[serde(default)]
    pub stop_requested: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_intent: Option<AttemptStopIntent>,
    pub submitted_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// The `cutex.delegation-ask.v1` view, fixed at the first submission
    /// (r208); recovery resubmits it unchanged. Older attempts have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<Value>,
    /// The last [`PROGRESS_KEEP`] `delegation_progress` notes, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub progress: Vec<ProgressNote>,
    /// The native context was compacted while this attempt ran (r208
    /// backstop: remote compaction can drop the turn's ASK item). The
    /// answer is flagged `compacted`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub compacted_mid_attempt: bool,
    /// When the last compaction of this attempt was observed (debounces the
    /// item and thread notifications of one compaction).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacted_at: Option<String>,
    /// A brief reminder should be steered into the turn (the scheduler
    /// sends it).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub compaction_reminder_pending: bool,
    /// Reminders sent for this attempt.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub compaction_reminders: u32,
    /// A continuation or reopen attempt: why it runs and which attempt it
    /// continues (delegation-await contract §2.6, §4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<Continuation>,
    /// A `yielded` attempt: the first 300 characters of the turn's final
    /// message (a stage note, never the answer; full text in
    /// `lastMessageArtifact`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage_note: Option<String>,
    /// How the event text was fitted to the receiver (render level,
    /// sizes, limit); additive, older cutex ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render: Option<RenderInfo>,
    /// A stop is pending and the scheduler first saw the target offline
    /// and not reconnectable at this time: past the grace
    /// (`asks.limits.stopOfflineGraceSecs`) the attempt ends
    /// `interrupted(stopped)`. Cleared when the target is seen online or
    /// reconnectable again; additive, older cutex ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_offline_since: Option<String>,
}

/// How an event text was fitted to its receiver (input overflow §A2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderInfo {
    /// 0 = everything inline.
    pub level: u8,
    pub text_bytes: usize,
    /// Serialized model item bytes (what the receiver policy measures).
    pub item_bytes: usize,
    /// The receiver's item limit (`None`: off / Claude, text cap only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    /// The receiver rejected this level's text as too large: the next
    /// admission renders deeper.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rejected: bool,
}

/// Why a continuation attempt runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ResumeReason {
    /// Awaited jobs finished (`resume: all` once the wait is met, `each`
    /// on every success).
    Jobs,
    /// An awaited job did not succeed (failed, cancelled, interrupted,
    /// non-zero exit): always wakes at once.
    JobFailed,
    Timeout,
    /// The requester or the owner asked the handler to continue now.
    Resumed,
    /// The requester or the owner reopened an answered delegation.
    Reopened,
}

/// Who asked for a manual resume or a reopen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ResumeBy {
    Requester,
    Owner,
}

/// Fixed on a continuation attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Continuation {
    /// Bounded wake reserved in the same transaction as this actual attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounded_wake_id: Option<String>,
    pub reason: ResumeReason,
    /// The attempt this one continues.
    pub of: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<ResumeBy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// `delegation_await.until`: when the wait is met.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AwaitUntil {
    #[default]
    All,
    Any,
}

/// `delegation_await.resume`: how successes wake the handler (a failure
/// always wakes it).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AwaitResume {
    /// Once, when the wait is met.
    #[default]
    All,
    /// On every success.
    Each,
}

/// The handler's current wait, registered in attempt `attempt` and in
/// effect once that attempt's turn ended (`yielded`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AwaitRecord {
    pub attempt: u32,
    pub jobs: Vec<String>,
    #[serde(default)]
    pub until: AwaitUntil,
    #[serde(default)]
    pub resume: AwaitResume,
    #[serde(default)]
    pub note: String,
    pub since: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_at: Option<String>,
}

/// A woken delegation waiting for its continuation to be admitted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeRecord {
    pub reason: ResumeReason,
    pub since: String,
    /// The attempt the continuation continues.
    pub from_attempt: u32,
    /// The jobs the continuation reports on (the wait set).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<ResumeBy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// How a bound job got bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JobSource {
    /// Seen as a `cutex_job.submit` result in the delegation's turn.
    Auto,
    /// Named in `delegation_await.jobs`.
    Explicit,
}

/// A job's terminal outcome (from Job Service completion v2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobTerminal {
    /// `exited`, `failed`, `cancelled`, `interrupted` or `launchUnknown`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    pub event_id: String,
    pub at: String,
    /// Exited with code 0.
    pub ok: bool,
}

/// State of a cutex-requested cancellation (`stop_jobs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JobCancelState {
    Pending,
    Sent,
    Failed,
    NotRunning,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobCancel {
    pub requested_at: String,
    pub by: ResumeBy,
    pub state: JobCancelState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
}

/// A Job Service job bound to a delegation (delegation-await contract §2.3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobBinding {
    pub job_id: String,
    pub source: JobSource,
    /// The attempt whose turn submitted (or named) it.
    pub attempt: u32,
    pub bound_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<JobTerminal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel: Option<JobCancel>,
}

impl JobBinding {
    pub fn is_terminal(&self) -> bool {
        self.terminal.is_some()
    }
}

/// How far an ask revision got (only ever goes up).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RevisionLevel {
    /// Persisted by the Agent Bus; not in the handler's runtime yet.
    Sent,
    /// The handler's runtime accepted it (native external input pending).
    Delivered,
    /// Written into one of the handler's turns (A4).
    InContext,
    /// The handler acknowledged it (`delegation_progress ack`).
    Acknowledged,
}

/// A revision of a delegation's brief sent with `agent_tell(delegation=…)`
/// (delegation-await contract §3). `v1` is the original brief.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RevisionRecord {
    /// `v2`, `v3`, …
    pub id: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    /// The Agent Bus message carrying it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default)]
    pub wake: bool,
    pub sent_at: String,
    pub level: RevisionLevel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_context_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledged_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack_text: Option<String>,
}

/// Revisions kept per delegation.
pub const MAX_REVISIONS: usize = 20;

fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// Progress notes kept per attempt.
pub const PROGRESS_KEEP: usize = 5;

/// One `delegation_progress` note.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressNote {
    pub text: String,
    pub at: String,
}

impl AttemptRecord {
    /// A new attempt, written before native submission.
    pub fn submitting(n: u32, native_message_id: String, runtime_generation: u64, submitted_at: String) -> Self {
        Self {
            n,
            native_message_id,
            event_artifact: None,
            runtime_generation,
            state: AttemptState::Submitting,
            turn_id: None,
            interrupt_reason: None,
            profile: None,
            model: None,
            shared_turn: false,
            owner_steered: false,
            included_notes: Vec::new(),
            activity: Activity::default(),
            last_message_artifact: None,
            native_diff_artifact: None,
            token_usage: None,
            stop_requested: false,
            stop_intent: None,
            submitted_at,
            started_at: None,
            ended_at: None,
            last_error: None,
            view: None,
            progress: Vec::new(),
            compacted_mid_attempt: false,
            compacted_at: None,
            compaction_reminder_pending: false,
            compaction_reminders: 0,
            continuation: None,
            stage_note: None,
            render: None,
            stop_offline_since: None,
        }
    }

    pub fn attempt_id(&self, ask_id: &str) -> String {
        format!("{ask_id}#{}", self.n)
    }
}

/// The ask: a durable task record (§5.12).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AskRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing_request: Option<super::routing_policy::RoutingRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing_approval_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing_grant_id: Option<String>,
    /// Daemon-authenticated source lease at creation, never request JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_lineage: Option<DelegationSourceLineage>,
    #[serde(default, skip_serializing_if = "super::continuation::ContinuationData::is_empty")]
    pub bounded_continuation: super::continuation::ContinuationData,
    #[serde(default)]
    pub hops: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<crate::peers::model::Remote>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_inbound: Option<crate::peers::model::Inbound>,
    pub ask_id: String,
    /// Client-known idempotency key (§6.1), unique per requester.
    pub action_id: String,
    /// Digest of the immutable request (target, body, attachments, options,
    /// follow-up); a reused `action_id` with another request is refused.
    pub request_sha256: String,
    pub requester: Requester,
    pub target_cutex_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// The first line of the brief.
    pub title: String,
    pub body: AskBody,
    pub options: AskOptions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_up_of: Option<String>,
    /// The ask whose turn the requester was running when it asked
    /// (authenticated ancestry for evidence, §7.5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_ask: Option<String>,
    /// FIFO position key across the target's queue (enqueue order).
    pub seq: u64,
    pub execution: ExecutionState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold: Option<Hold>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_since: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<GateRecord>,
    #[serde(default)]
    pub attempts: Vec<AttemptRecord>,
    /// Latest answer (id, revision).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer_id: Option<String>,
    /// The turn that answered was reverted (§5.11). The answer text stays.
    #[serde(default)]
    pub target_reverted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed: Option<CloseOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Record revision; bumped by every change (optimistic reads by clients).
    pub revision: u64,
    /// Job Service jobs bound to this delegation (delegation-await §2.3).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<JobBinding>,
    /// The handler's wait (`delegation_await`), in effect once its attempt
    /// yielded.
    #[serde(default, rename = "await", skip_serializing_if = "Option::is_none")]
    pub awaiting: Option<AwaitRecord>,
    /// Woken (or reopened): a continuation attempt waits for admission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<ResumeRecord>,
    /// Revisions of the brief (`agent_tell(delegation=…)`), `v2` first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub revisions: Vec<RevisionRecord>,
    /// The follow-up that superseded this delegation (`closed: superseded`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    /// Retryable native submit failures in a row (backoff and cap,
    /// [`super::SUBMIT_RETRY_LIMIT`]); cleared when a receipt binds a turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submit_retry: Option<SubmitRetry>,
    /// The owner was reminded once that this open delegation has been idle
    /// for `documents.idleReminderDays` (retention sweep). Additive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_reminded_at: Option<String>,
    /// Attempts that failed because the target's Claude usage limit was
    /// exhausted and were put back in the queue (held `blocked(quota)` until
    /// the limit resets) instead of failing the delegation
    /// ([`super::QUOTA_REQUEUE_LIMIT`]). Additive: an older cutex drops it.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub quota_requeues: u32,
}

/// Exact parent occurrence captured by the daemon while it creates a child.
/// parent_ask supplies the canonical parent delegation ID. Missing old facts
/// remain unknown; never reconstruct these fields from names or prose.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegationSourceLineage {
    pub parent_attempt_id: String,
    pub owner_session_id: String,
    pub runtime_generation: u64,
    pub parent_turn_id: String,
    pub source_event_id: String,
}

/// Retryable native submit failures of one ask: transport or unknown RPC
/// errors of the submit, and failed status queries while its outcome is
/// unknown. Neither the submit nor a status query is tried again before
/// `notBefore`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmitRetry {
    pub failures: u32,
    pub not_before: String,
    pub last_error: String,
}

impl AskRecord {
    pub fn current_attempt(&self) -> Option<&AttemptRecord> {
        self.attempts.last()
    }

    /// The handler yielded and waits for jobs (no lease, no running turn).
    pub fn is_waiting(&self) -> bool {
        self.is_open()
            && self.execution == ExecutionState::Running
            && self.awaiting.as_ref().is_some_and(|wait| {
                self.current_attempt().is_some_and(|attempt| attempt.n == wait.attempt && attempt.state == AttemptState::Yielded)
            })
    }

    /// Woken or reopened: a continuation attempt waits for admission.
    pub fn resume_pending(&self) -> bool {
        self.is_open()
            && self.execution == ExecutionState::Running
            && self.resume.is_some()
            && self.current_attempt().is_none_or(|attempt| attempt.state.is_terminal())
            && self.bounded_continuation.current.as_ref().is_none_or(|bounded| {
                bounded.state == super::continuation::Phase::Cancelled
                    || (bounded.state == super::continuation::Phase::Ready
                        && bounded.wake.as_ref().is_some_and(|wake| matches!(wake.submission, super::continuation::Submission::Unclaimed)))
            })
    }

    pub fn job(&self, job_id: &str) -> Option<&JobBinding> {
        self.jobs.iter().find(|job| job.job_id == job_id)
    }

    /// Bound jobs without a terminal outcome.
    pub fn running_jobs(&self) -> Vec<String> {
        self.jobs.iter().filter(|job| !job.is_terminal()).map(|job| job.job_id.clone()).collect()
    }

    pub fn current_attempt_mut(&mut self) -> Option<&mut AttemptRecord> {
        self.attempts.last_mut()
    }

    pub fn is_open(&self) -> bool {
        self.closed.is_none()
    }

    pub fn requester_session(&self) -> Option<&str> {
        self.requester.session()
    }

    /// The session that controls this ask now (design 044 §4.2): the
    /// handover controller recorded by `director_address`, else the original
    /// requester. `requester`, `action_id` and `request_sha256` never change;
    /// an unknown / unreconciled handover is an error (fail closed).
    pub fn controller(&self) -> Result<Option<String>, super::store::AskError> {
        Ok(match route::controller(self)? {
            Some(controller) => Some(controller),
            None => self.requester.session().map(str::to_string),
        })
    }

    /// `caller` may control the ask (close, stop, reopen, resume, revise,
    /// wait, answer its questions as the requester side) now.
    pub fn controlled_by(&self, caller: &str) -> Result<bool, super::store::AskError> {
        Ok(self.controller()?.as_deref() == Some(caller))
    }

    /// As [`Self::controlled_by`], with the r253 error when not (the ask
    /// looks absent to anyone else, so ids do not leak).
    pub fn require_controller(&self, caller: &str, missing: &'static str, message: &'static str) -> Result<(), super::store::AskError> {
        if self.controlled_by(caller)? {
            Ok(())
        } else {
            Err(super::store::AskError::new(missing, message))
        }
    }

    /// `caller` may read the ask as a requester-side participant: the
    /// original requester (history) or someone on its controller chain.
    /// Reading never grants control and never settles another recipient's
    /// deliveries.
    pub fn readable_by(&self, caller: &str) -> bool {
        self.requester.session() == Some(caller)
            || matches!(self.controller(), Ok(Some(ref controller)) if controller == caller)
            || route::can_read(self, caller)
    }
}

/// Where the asks code reads design 044 routing: `director_address`, the
/// one owner of handover state. Unit tests of this crate may pin a handover
/// per ask on their own thread ([`route::pin`]) instead of writing the
/// sidecar and the Agent Management authority.
pub mod route {
    use super::{AskRecord, DeliveryKind, DeliveryRecord, DeliveryState};
    use crate::asks::store::{AskError, AskState};

    #[cfg(test)]
    #[derive(Clone, Debug)]
    pub struct Pin {
        /// `Err(code)`: an unknown / unreconciled handover.
        pub controller: Result<Option<String>, &'static str>,
        /// Earlier controllers (may read, never control).
        pub history: Vec<String>,
        /// A question route other than the controller (`questions: director`).
        pub questions: Option<String>,
    }

    #[cfg(test)]
    thread_local! {
        static PINS: std::cell::RefCell<std::collections::BTreeMap<String, Pin>> = Default::default();
    }

    /// Test only: `ask_id` is handed over as `pin` says on this thread.
    #[cfg(test)]
    pub fn pin(ask_id: &str, pin: Pin) {
        PINS.with(|pins| pins.borrow_mut().insert(ask_id.to_string(), pin));
    }

    #[cfg(test)]
    pub(crate) fn pinned(ask_id: &str) -> Option<Pin> {
        PINS.with(|pins| pins.borrow().get(ask_id).cloned())
    }

    /// The handover controller (`None`: no handover).
    pub fn controller(ask: &AskRecord) -> Result<Option<String>, AskError> {
        #[cfg(test)]
        if let Some(pin) = pinned(&ask.ask_id) {
            return pin.controller.map_err(|code| AskError::new(code, "handover unknown (test)"));
        }
        crate::director_address::controller(ask)
    }

    pub fn can_read(ask: &AskRecord, caller: &str) -> bool {
        #[cfg(test)]
        if let Some(pin) = pinned(&ask.ask_id) {
            return pin.history.iter().any(|id| id == caller) || matches!(pin.controller, Ok(Some(ref id)) if id == caller);
        }
        crate::director_address::can_read(ask, caller)
    }

    /// Who the ask's questions go to now.
    pub fn question_recipient(ask: &AskRecord) -> Result<Option<String>, AskError> {
        #[cfg(test)]
        if let Some(pin) = pinned(&ask.ask_id) {
            if let Some(route) = pin.questions {
                return Ok(Some(route));
            }
            return Ok(controller(ask)?.or_else(|| ask.requester.session().map(str::to_string)));
        }
        crate::director_address::question_recipient(ask)
    }

    /// The recipient of a pending item (`None`: hold it).
    pub fn pending_recipient(state: &AskState, delivery: &DeliveryRecord) -> Option<String> {
        debug_assert_eq!(delivery.state, DeliveryState::Pending);
        let ask = state.asks.get(&delivery.ask_id)?;
        if delivery.kind == DeliveryKind::Question {
            return question_recipient(ask).ok().flatten();
        }
        #[cfg(test)]
        if pinned(&ask.ask_id).is_some() {
            return controller(ask).ok().map(|id| id.unwrap_or_else(|| delivery.requester_cutex_session_id.clone()));
        }
        crate::director_address::delivery_recipient(state, delivery)
    }
}

/// Answer status (§5.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AnswerStatus {
    Completed,
    Failed,
    Interrupted,
}

/// Answer metadata (§5.6).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnswerMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_usage: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default)]
    pub owner_steered: bool,
    #[serde(default)]
    pub shared_turn: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub included_notes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// From `turn/diff/updated`; labelled native, not an audit (§5.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_diff: Option<String>,
    /// The handler's context was compacted mid-attempt (r208): the answer
    /// may have lost track of the brief.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub compacted: bool,
}

/// An answer revision. A new attempt's answer is a new revision of the ask's
/// answer; the content lives in the artifact store, with an inline preview.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnswerRecord {
    pub answer_id: String,
    pub ask_id: String,
    pub attempt: u32,
    pub revision: u32,
    pub status: AnswerStatus,
    /// The final agent message; `None` when the turn had none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
    pub bytes: usize,
    pub metadata: AnswerMetadata,
    pub created_at: String,
    /// The first 300 characters of the answer (r208; older answers: none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum QuestionState {
    Open,
    Resolved,
    /// The attempt or runtime generation it belonged to is gone.
    Stale,
}

/// The native pending request a question is bound to (§5.9).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeQuestionRef {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    /// JSON-RPC request id of the server request.
    pub request_id: Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionResolution {
    /// `owner` or the requester's session id.
    pub by: String,
    pub answers: Value,
    pub at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionRecord {
    pub question_id: String,
    pub ask_id: String,
    pub attempt: u32,
    pub runtime_generation: u64,
    pub native: NativeQuestionRef,
    /// The native questions (`ToolRequestUserInputParams.questions`).
    pub questions: Value,
    pub route: QuestionRoute,
    /// Set when the question fell through to the owner (timeout, offline).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routed_to_owner_at: Option<String>,
    pub state: QuestionState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<QuestionResolution>,
    pub created_at: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DeliveryKind {
    Answer,
    Question,
    /// A wake-worthy state change (paused, blocked, rejected, interrupted).
    State,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DeliveryState {
    Pending,
    ClaimedByWait,
    QueuedAsEvent,
    Acknowledged,
}

/// One continuation per answer/question per requester (§5.7).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryRecord {
    pub delivery_id: String,
    pub kind: DeliveryKind,
    pub ask_id: String,
    /// Answer id, question id, or the state name.
    pub ref_id: String,
    pub requester_cutex_session_id: String,
    pub state: DeliveryState,
    /// `action_id` of the wait that claimed it (a retried wait gets it again).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_by: Option<String>,
    /// The coalesced ANSWER/QUESTION event carrying it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    #[serde(default)]
    pub passive: bool,
    /// Why an item was acknowledged without an event or a wait (r208):
    /// `read` (`delegation_read` of that revision) or `closed`
    /// (`delegation_close` / `delegation_stop`). The state stays
    /// `acknowledged` so older releases read the record unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settled_by: Option<String>,
    /// The session this item actually went to (a wait claimed it or an event
    /// carried it), when that is not `requester_cutex_session_id` (design 044
    /// handover). A pending item has none: its recipient is resolved when it
    /// is sent (`delivery::recipient`). Older releases ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routed_to: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Content-addressed blob metadata (§8.6). Readable by the listed sessions
/// and the owner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactRecord {
    /// `art-<sha256>`.
    pub artifact_id: String,
    pub bytes: usize,
    pub media_type: String,
    pub readers: Vec<String>,
    pub asks: Vec<String>,
    pub created_at: String,
}

/// What occupies a session's admission (§5.3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum LeaseHolder {
    Ask { ask_id: String, attempt: u32 },
    /// A coalesced ANSWER/QUESTION event to this session as requester.
    Event { event_id: String },
    /// Owner queued input flushed by cutex.
    Owner,
}

/// The admission lease: at most one turn-starting admission per session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionLease {
    pub cutex_session_id: String,
    pub holder: LeaseHolder,
    pub native_message_id: String,
    pub runtime_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub since: String,
}

/// A coalesced ANSWER/QUESTION event waiting for (or carried by) a requester
/// turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventOutboxRecord {
    pub event_id: String,
    pub cutex_session_id: String,
    pub delivery_ids: Vec<String>,
    pub native_message_id: String,
    /// The exact event text (artifact) for reconciliation by message id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_artifact: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub acknowledged: bool,
    pub created_at: String,
    /// The `cutex.delegation-results.v1` view, fixed with the event text (r208).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<Value>,
    /// The requester's context was compacted in the event's turn (r210
    /// backstop: natives before r50 drop the turn's ANSWER/QUESTION item).
    /// Optional fields, so r209 reads (and drops) them unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacted_at: Option<String>,
    /// A reminder of the delivered delegations is to be steered into the turn.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub compaction_reminder_pending: bool,
}

/// Owner-set delegation pause on a session (§5.10). Created by an explicit
/// owner pause (`cutex/asks/pause`, origin `ownerPause`) or, since the owner
/// decision of 2026-10-12 ("停止 = everything waits"), by an owner turn stop
/// (`cutex/turn/stop`, origin `ownerStop`, `stopHold`). Cleared by
/// `cutex/asks/resume` or by the owner's next message to that session (a
/// stop hold only by one that starts or steers a turn); a stop hold also
/// by `cutex/userInput/queue/resume`. While it exists the
/// session admits no asks and receives no ANSWER/QUESTION events; while
/// `stop_hold` is set, no ride-along delivery (bus wakes, tells, alarms)
/// starts a turn either.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionPause {
    pub cutex_session_id: String,
    pub by: PausedBy,
    pub since: String,
    /// `ownerPause` (r208 explicit pause), `ownerStop` (turn stop, r229) or
    /// `runtimeGate` (the native input gate refused a delivery: the last
    /// turn was interrupted / failed and tool effects may be uncertain; only
    /// the owner's resume or message reopens it, r233).
    /// Older pauses have none: they came from a turn stop or a queue pause
    /// and can't be told apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// An owner stop holds every ride-along of the session (not only asks
    /// and their results) until the owner resumes or sends a message. Set on
    /// every `ownerStop` pause, and on an explicit pause the owner then stops.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stop_hold: bool,
    /// Origin `runtimeGate`: the runtime generation whose closed input gate
    /// refused a delivery. A new generation (runtime restart) ends the pause.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_generation: Option<u64>,
}
