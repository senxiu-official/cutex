//! Input ledger (cutex-desktop docs/design/054, capability `input-ledger-v1`,
//! not advertised until C1–C5 are merged): one durable `InputRecord` per input
//! an agent receives (owner message, bus note, alarm, delegation result /
//! request). The record alone decides the input's state, the surface it shows
//! on and why it waits; every change bumps the record's `rev` and the
//! session's gapless `stateRevision`.
//!
//! Frozen C1 interface (054 §6):
//!
//! ```text
//! InputLedger::transition(session, input_id, Expect { rev }, Next { state, reason,
//!     surface?, turn_id?, stage?, evidence?, .. }) -> Result<Transition, LedgerError>
//! InputLedger::get(session, &[input_id]) -> GetResult
//! InputLedger::list(session, ListQuery { since_revision?, include_settled_secs }) -> ListResult
//! InputLedger::since(session, revision, limit) -> SinceResult
//! ```
//!
//! Storage (051 rules, 054 §2.13): its own SQLite file
//! `<runtime>/management-v2/input-ledger.sqlite3` (WAL). A write is one
//! immediate transaction: read one row, upsert it, bump the session counter.
//! No whole-ledger reads or clones, no scans outside the per-session indexes.
//! The file is separate from `management-state.sqlite3` so a ledger write
//! never queues behind a long asks commit (one SQLite writer per file), and so
//! a lost / corrupt ledger can be replaced with a new epoch (§2.12) without
//! touching other stores.
//!
//! Lock rule (§2.1 invariant 6, §2.13): the ledger mutex is a LEAF lock. While
//! it is held this module only coordinates and accesses its own database file; it never
//! takes a session lane, the host authority fence, the AskStore lock, or does
//! runtime / network IO. Callers may hold any of those when they call in
//! (lane → ledger is the allowed order). Debug builds panic when a thread that
//! holds the ledger lock reserves a session lane
//! (`session_coordination`), or re-enters the ledger.
//!
//! Every new behaviour stays behind the internal switch
//! `management.inputLedger.enabled` (config.json, default off; [`enabled`]).

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::input_ledger_timing as timing;

pub const CAPABILITY: &str = "input-ledger-v1";
/// r274.2 (owner decision / Director §9.1, 2026-11-03): owner records stay
/// on nextUp until read and notes move at their native item. Advertised with
/// `input-ledger-v1`; without it the desktop applies the rule itself.
pub const OWNER_INPUT_NEXTUP_CAPABILITY: &str = "owner-input-nextup-v1";
/// Owner decision 2026-11-04: an idle busy-host send stays on the timeline.
pub const OWNER_IDLE_BUSY_TIMELINE_CAPABILITY: &str = "owner-idle-busy-timeline-v1";
/// Design 057. Advertise only after bundled runtime submission is available.
pub const OWNER_CARRIES_CAPABILITY: &str = "owner-carries-v1";
pub const DATABASE_FILE: &str = "input-ledger.sqlite3";
const FORMAT_VERSION: i64 = 1;

pub const GET_METHOD: &str = "cutex/input/get";
pub const LIST_METHOD: &str = "cutex/input/list";
/// Owner: single or bulk dismiss (§2.14, owner decision 9).
pub const DISMISS_METHOD: &str = "cutex/input/dismiss";
pub const HOLD_METHOD: &str = "cutex/input/hold";
/// Owner: release a stale lease (§2.14); served once C4 registers it.
pub const LEASE_RELEASE_METHOD: &str = "cutex/input/lease/release";
/// Owner: resend a failed / uncertain input with the same id (§2.14, C5).
pub const RETRY_METHOD: &str = "cutex/input/retry";
/// Owner: interrupt the running turn and send this owner input first
/// (`interrupt-send-contract.md`, r274).
pub const INTERRUPT_SEND_METHOD: &str = "cutex/input/interruptSend";
/// Bulk dismiss accepts at most this many items.
pub const MAX_DISMISS_ITEMS: usize = 100;
pub const STATE_CHANGED_EVENT: &str = "cutex/input/stateChanged";
pub const DELIVERY_CHANGED_EVENT: &str = "cutex/session/deliveryChanged";

/// `cutex/input/get` accepts at most this many ids.
pub const MAX_GET_IDS: usize = 100;
/// Most transitions in one [`InputLedger::transition_batch`].
pub const MAX_BATCH_ITEMS: usize = 100;
/// `cutex/input/list` default window for settled records (owner decision 4).
pub const DEFAULT_INCLUDE_SETTLED_SECS: u64 = 15 * 60;
/// Settled records are compacted after this long (owner decision 4).
pub const SETTLED_RETENTION_SECS: u64 = 7 * 24 * 60 * 60;
/// An incremental read returning more rows than this asks for a full read.
pub const MAX_SINCE_RECORDS: usize = 2000;
const PREVIEW_CHARS: usize = 80;

// ---------------------------------------------------------------------------
// Switch

/// `config.json` → `management.inputLedger.enabled` (default off). Typed so a
/// settings save keeps it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagementInternalSettings {
    #[serde(default, skip_serializing_if = "InputLedgerSettings::is_default")]
    pub input_ledger: InputLedgerSettings,
}

impl ManagementInternalSettings {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputLedgerSettings {
    #[serde(default)]
    pub enabled: bool,
}

impl InputLedgerSettings {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

static ENABLED_OVERRIDE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
static ENABLED_CACHE: Mutex<Option<(Instant, bool)>> = Mutex::new(None);
const ENABLED_CACHE_TTL: Duration = Duration::from_secs(5);

/// Whether the ledger is on for this process. Reads config.json at most once
/// per 5 s; tests and the E2E harness use [`set_enabled_override`].
pub fn enabled() -> bool {
    match ENABLED_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed) {
        1 => return false,
        2 => return true,
        _ => {}
    }
    let mut cache = ENABLED_CACHE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((at, value)) = *cache {
        if at.elapsed() < ENABLED_CACHE_TTL {
            return value;
        }
    }
    let value = crate::config::store::load_codez_config().management.input_ledger.enabled;
    *cache = Some((Instant::now(), value));
    value
}

/// `Some(on)` pins the switch for this process; `None` returns to config.
pub fn set_enabled_override(value: Option<bool>) {
    let code = match value {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    };
    ENABLED_OVERRIDE.store(code, std::sync::atomic::Ordering::Relaxed);
}

static ENABLED_PIN: Mutex<()> = Mutex::new(());

/// The override pinned for one test: the override is process-wide, so tests
/// that pin it run one at a time (parallel `cargo test` let one test's
/// `Some(true)` leak into another's switch-off assertions). Dropping the pin
/// returns to config.
pub struct EnabledPin {
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl EnabledPin {
    pub fn set(&self, value: Option<bool>) {
        set_enabled_override(value);
    }
}

impl Drop for EnabledPin {
    fn drop(&mut self) {
        set_enabled_override(None);
    }
}

pub fn pin_enabled(value: Option<bool>) -> EnabledPin {
    let lock = ENABLED_PIN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    set_enabled_override(value);
    EnabledPin { _lock: lock }
}

// ---------------------------------------------------------------------------
// Vocabulary (append only; exported as schema/export/input-states.json)

pub const INPUT_KINDS: &[&str] = &[
    "owner", "agentMessage", "peerMessage", "alarm", "systemNotice",
    "delegationRequest", "delegationResult", "delegationQuestion", "delegationStatus",
];
pub const SENDER_KINDS: &[&str] = &[
    "owner", "agent", "peer", "alarm", "jobService", "management", "claudeTask", "approval", "cutex",
];
pub const INPUT_STATES: &[&str] = &[
    "received", "held", "waiting", "sending", "accepted", "inContext",
    "uncertain", "failed", "rejected", "discarded", "superseded",
];
pub const SURFACES: &[&str] = &["timeline", "nextUp", "pendingNotes", "none"];
pub const STAGES: &[&str] = &["registered", "offered"];
pub const EVIDENCE: &[&str] = &[
    "clientIdEcho", "turnStartInput", "contextPersisted", "externalInputPersisted",
    "a4Receipt", "historyRead", "claimedByWait", "readByTool",
];
pub const ACTIONS: &[&str] = &["retry", "dismiss", "cancel", "sendNow", "edit", "releaseLease", "interruptSend", "hold", "releaseHold"];
pub const STRATEGIES: &[&str] = &["auto", "queue", "interrupt"];
pub const DELIVERY_MODES: &[&str] = &["after_turn", "soon", "passive", "interrupt"];
pub const TIMINGS: &[&str] = &["afterTurn", "nextStep", "passive"];

pub const HELD_REASONS: &[&str] = &["hostBusy"];
/// `sending` usually carries no reason; `interruptSend`: the owner's
/// `cutex/input/interruptSend` interrupts the running turn to send it first.
pub const SENDING_REASONS: &[&str] = &["interruptSend"];
pub const CARRIED_REASONS: &[&str] = &["carriedByOwner"];
pub const WAITING_REASONS: &[&str] = &[
    "explicitQueue", "afterTurn", "nextStep", "passive", "turnRunning", "turnFinishing",
    "adapterQueued", "ownerQueueAhead", "rideAlongAhead", "delegationLease", "eventLease",
    "paused", "stopHold", "ownerPause", "agentOffline", "runtimeDisconnected", "hostBusy",
    "resultCoalescing", "autoRetry", "reviewPending", "targetBusy", "staleHeld", "nativeHeld",
    "nativeRetry", "returned", "ownerRetry",
];
pub const UNCERTAIN_REASONS: &[&str] = &[
    "turnInterrupted", "turnFailed", "runtimeLost", "receiptTimeout", "outcomeUnknown",
    "leaseReleased", "nativeConflict",
];
pub const FAILED_REASONS: &[&str] = &[
    "admission_timeout", "notDelivered", "runtimeRefused", "routeLost", "interrupt_timeout",
    "session_offline", "tooLarge",
];
pub const REJECTED_REASONS: &[&str] = &[
    "targetUnavailable", "quarantined", "provenance", "invalid", "askRejected", "peerRefused",
    "owner_required",
];
pub const DISCARDED_REASONS: &[&str] = &[
    "ownerRemoved", "ownerDiscarded", "settlementClosed", "askCancelled", "senderRevoked", "archived",
    // r274.2: a passive status item its delegation outgrew, never offered.
    "superseded",
    // r274.2 §9.6: the turn that carried it was removed by a confirmed
    // `cutex/session/history/revert` ([`HISTORY_REVERTED`]).
    "historyReverted",
];
/// `discarded{historyReverted}` (r274.2 §9.6): a record bound to a turn a
/// confirmed history revert removed. The only reason the
/// `inContext | accepted | sending | failed → discarded` edges take; the
/// record keeps its `evidence`, `turnId` and `timelineItemId` as audit and
/// carries `details.historyRevert = {actionId, turnId}`.
pub const HISTORY_REVERTED: &str = "historyReverted";
pub const SUPERSEDED_REASONS: &[&str] = &["mergedInsert", "revised"];
/// `stalled.reason`: why an eligible input has not moved (§2.2, §2.12).
pub const STALLED_REASONS: &[&str] = &[
    "runtimeDisconnected", "noProgress", "leaseHeld", "hostBusy", "agentOffline",
];

pub const SESSION_DELIVERY_STATES: &[&str] =
    &["offline", "starting", "disconnected", "idle", "turn", "finishing", "held"];
pub const SESSION_DELIVERY_HELD_REASONS: &[&str] =
    &["hostBusy", "stopHold", "paused", "delegationLease", "eventLease"];
pub const SESSION_TURN_KINDS: &[&str] = &["owner", "ask", "delivery", "unknown"];
pub const LEASE_HOLDERS: &[&str] = &["ask", "event", "owner"];
pub const LEASE_RECONCILE_STATES: &[&str] = &["waitingTurn", "checking", "stale"];

macro_rules! wire_enum {
    ($name:ident { $($variant:ident = $wire:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name { $(#[serde(rename = $wire)] $variant),+ }
        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];
            pub fn as_str(self) -> &'static str { match self { $($name::$variant => $wire),+ } }
            pub fn parse(value: &str) -> Option<Self> { match value { $($wire => Some($name::$variant),)+ _ => None } }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.as_str()) }
        }
    };
}

wire_enum!(InputKind {
    Owner = "owner", AgentMessage = "agentMessage", PeerMessage = "peerMessage", Alarm = "alarm",
    SystemNotice = "systemNotice", DelegationRequest = "delegationRequest",
    DelegationResult = "delegationResult", DelegationQuestion = "delegationQuestion",
    DelegationStatus = "delegationStatus",
});
wire_enum!(SenderKind {
    Owner = "owner", Agent = "agent", Peer = "peer", Alarm = "alarm", JobService = "jobService",
    Management = "management", ClaudeTask = "claudeTask", Approval = "approval", Cutex = "cutex",
});
wire_enum!(InputState {
    Received = "received", Held = "held", Waiting = "waiting", Sending = "sending",
    Accepted = "accepted", InContext = "inContext", Uncertain = "uncertain", Failed = "failed",
    Rejected = "rejected", Discarded = "discarded", Superseded = "superseded",
});
wire_enum!(Surface { Timeline = "timeline", NextUp = "nextUp", PendingNotes = "pendingNotes", None = "none" });
wire_enum!(Stage { Registered = "registered", Offered = "offered" });
wire_enum!(Evidence {
    ClientIdEcho = "clientIdEcho", TurnStartInput = "turnStartInput",
    ContextPersisted = "contextPersisted", ExternalInputPersisted = "externalInputPersisted",
    A4Receipt = "a4Receipt", HistoryRead = "historyRead", ClaimedByWait = "claimedByWait",
    ReadByTool = "readByTool",
});
wire_enum!(Action {
    Retry = "retry", Dismiss = "dismiss", Cancel = "cancel", SendNow = "sendNow", Edit = "edit",
    ReleaseLease = "releaseLease", InterruptSend = "interruptSend",
    Hold = "hold", ReleaseHold = "releaseHold",
});
wire_enum!(Strategy { Auto = "auto", Queue = "queue", Interrupt = "interrupt" });
wire_enum!(DeliveryMode { AfterTurn = "after_turn", Soon = "soon", Passive = "passive", Interrupt = "interrupt" });
wire_enum!(Timing { AfterTurn = "afterTurn", NextStep = "nextStep", Passive = "passive" });

impl InputState {
    /// Terminal: `inContext`, `failed`, `rejected`, `discarded`, `superseded`
    /// (`failed` still accepts the owner's retry).
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::InContext | Self::Failed | Self::Rejected | Self::Discarded | Self::Superseded)
    }

    /// Settled (了结): terminal or `uncertain`. cutex does nothing more on its
    /// own; deadlines are cleared.
    pub fn is_settled(self) -> bool {
        self.is_terminal() || self == Self::Uncertain
    }

    /// Reason codes this state may carry. `None`: the state carries no reason.
    pub fn reasons(self) -> Option<&'static [&'static str]> {
        match self {
            Self::Held => Some(HELD_REASONS),
            Self::Waiting => Some(WAITING_REASONS),
            Self::Uncertain => Some(UNCERTAIN_REASONS),
            Self::Failed => Some(FAILED_REASONS),
            Self::Rejected => Some(REJECTED_REASONS),
            Self::Discarded => Some(DISCARDED_REASONS),
            Self::Superseded => Some(SUPERSEDED_REASONS),
            // Optional: see [`SENDING_REASONS`].
            Self::Sending => Some(SENDING_REASONS),
            Self::Accepted | Self::InContext => Some(CARRIED_REASONS),
            Self::Received => None,
        }
    }
}

impl Stage {
    fn order(self) -> u8 {
        match self {
            Self::Registered => 1,
            Self::Offered => 2,
        }
    }
}

// ---------------------------------------------------------------------------
// Record

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputSender {
    pub kind: SenderKind,
    pub id: String,
    pub name: String,
    pub host: Option<String>,
    pub cutex_session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputIntent {
    /// Owner messages only.
    pub strategy: Option<Strategy>,
    pub delivery_mode: Option<DeliveryMode>,
    /// Required: never defaulted by a client (#12).
    pub timing: Timing,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputOrder {
    pub admission_key: String,
    pub position: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputRemote {
    pub host: String,
    pub peer_id: String,
    pub sender_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stalled {
    pub reason: String,
    pub since: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputError {
    pub code: String,
    pub retryable: bool,
    pub details: Value,
}

/// 054 §2.2. `details` is an addition to the design text: the per-reason
/// details object (`nativeHeld` → `details.nativeReason`, …), `{}` when none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputRecord {
    pub input_id: String,
    pub cutex_session_id: String,
    pub kind: InputKind,
    pub sender: InputSender,
    pub intent: InputIntent,
    pub preview: String,
    pub created_at: String,
    pub state: InputState,
    pub reason: Option<String>,
    pub since: String,
    pub deadline: Option<String>,
    /// Busy-wake presentation only; independent of state timeout/deadline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_schedule: Option<super::input_ledger_wake::WakeSchedule>,
    pub stage: Option<Stage>,
    pub surface: Surface,
    pub order: Option<InputOrder>,
    pub turn_id: Option<String>,
    pub timeline_item_id: Option<String>,
    pub runtime_generation: Option<u64>,
    pub attempt: u64,
    pub superseded_by: Option<String>,
    pub batch_id: Option<String>,
    /// Inputs included in this owner's native submission (057). Missing on
    /// older persisted rows; retained when the owner's delivery state changes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub carries: Vec<String>,
    pub ask_id: Option<String>,
    pub remote: Option<InputRemote>,
    pub stalled: Option<Stalled>,
    pub actions: Vec<Action>,
    pub error: Option<InputError>,
    pub evidence: Option<Evidence>,
    pub details: Value,
    pub rev: u64,
    pub state_revision: u64,
    pub updated_at: String,
}

/// What a new record is (`Next::create`). Ignored when the record exists.
#[derive(Debug, Clone, PartialEq)]
pub struct NewInput {
    pub kind: InputKind,
    pub sender: InputSender,
    pub intent: InputIntent,
    /// Full text is fine: the ledger stores at most 80 characters.
    pub preview: String,
    pub ask_id: Option<String>,
    pub remote: Option<InputRemote>,
}

/// Compare-and-set on the record's `rev`. `rev: None` applies to whatever is
/// stored; `Some(0)` requires the record not to exist yet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Expect {
    pub rev: Option<u64>,
}

impl Expect {
    pub const ANY: Expect = Expect { rev: None };
    pub const NEW: Expect = Expect { rev: Some(0) };
    pub fn rev(rev: u64) -> Self {
        Self { rev: Some(rev) }
    }
}

/// The requested next state. `None` fields keep the stored value, except the
/// ones a state change resets (`stage` outside `accepted`, `evidence` outside
/// `inContext`, `stalled`, `deadline`, `error`).
#[derive(Debug, Clone, PartialEq)]
pub struct Next {
    pub state: InputState,
    pub reason: Option<String>,
    /// `None`: computed by the §2.5 rules. An explicit surface must still move
    /// forward (`nextUp|pendingNotes → timeline`, `* → none`).
    pub surface: Option<Surface>,
    pub turn_id: Option<String>,
    pub stage: Option<Stage>,
    pub evidence: Option<Evidence>,
    /// Required when the record does not exist yet.
    pub create: Option<NewInput>,
    pub details: Option<Value>,
    pub error: Option<InputError>,
    pub order: Option<Option<InputOrder>>,
    pub timeline_item_id: Option<String>,
    pub runtime_generation: Option<u64>,
    pub superseded_by: Option<String>,
    pub batch_id: Option<String>,
    pub carries: Option<Vec<String>>,
    /// `Some(None)` clears it. A state change clears it unless given.
    pub stalled: Option<Option<Stalled>>,
    /// `Some(None)` removes the deadline; `None` uses the state's default.
    pub deadline: Option<Option<DateTime<Utc>>>,
}

impl Next {
    pub fn to(state: InputState) -> Self {
        Self {
            state, reason: None, surface: None, turn_id: None, stage: None, evidence: None,
            create: None, details: None, error: None, order: None, timeline_item_id: None,
            runtime_generation: None, superseded_by: None, batch_id: None, carries: None, stalled: None,
            deadline: None,
        }
    }
    pub fn received() -> Self { Self::to(InputState::Received) }
    pub fn held(reason: &str) -> Self { Self::to(InputState::Held).reason(reason) }
    pub fn waiting(reason: &str) -> Self { Self::to(InputState::Waiting).reason(reason) }
    pub fn sending() -> Self { Self::to(InputState::Sending) }
    pub fn accepted() -> Self { Self::to(InputState::Accepted) }
    pub fn in_context(evidence: Evidence) -> Self { Self::to(InputState::InContext).evidence(evidence) }
    pub fn uncertain(reason: &str) -> Self { Self::to(InputState::Uncertain).reason(reason) }
    pub fn failed(reason: &str) -> Self { Self::to(InputState::Failed).reason(reason) }
    pub fn rejected(reason: &str) -> Self { Self::to(InputState::Rejected).reason(reason) }
    pub fn discarded(reason: &str) -> Self { Self::to(InputState::Discarded).reason(reason) }
    pub fn superseded(reason: &str) -> Self { Self::to(InputState::Superseded).reason(reason) }

    pub fn reason(mut self, reason: &str) -> Self { self.reason = Some(reason.to_string()); self }
    pub fn surface(mut self, surface: Surface) -> Self { self.surface = Some(surface); self }
    pub fn turn(mut self, turn_id: impl Into<String>) -> Self { self.turn_id = Some(turn_id.into()); self }
    pub fn stage(mut self, stage: Stage) -> Self { self.stage = Some(stage); self }
    pub fn evidence(mut self, evidence: Evidence) -> Self { self.evidence = Some(evidence); self }
    pub fn create(mut self, input: NewInput) -> Self { self.create = Some(input); self }
    pub fn details(mut self, details: Value) -> Self { self.details = Some(details); self }
    pub fn stalled(mut self, stalled: Option<Stalled>) -> Self { self.stalled = Some(stalled); self }
    pub fn deadline(mut self, deadline: Option<DateTime<Utc>>) -> Self { self.deadline = Some(deadline); self }
    pub fn superseded_by(mut self, id: impl Into<String>) -> Self { self.superseded_by = Some(id.into()); self }
    /// The native timeline item the input is in (054 §3.2 join key).
    pub fn timeline_item(mut self, item: impl Into<String>) -> Self { self.timeline_item_id = Some(item.into()); self }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Transition {
    pub record: InputRecord,
    /// `false`: nothing observable changed; no revision was used and no
    /// `cutex/input/stateChanged` may be published.
    pub changed: bool,
    pub created: bool,
    pub ledger_epoch: String,
    pub state_revision: u64,
    /// Its event was written to the outbox in the same transaction
    /// ([`publish`] drains the outbox instead of appending it directly).
    outboxed: bool,
}

#[derive(Debug)]
pub enum LedgerError {
    NotFound,
    Conflict { current_rev: u64 },
    ForbiddenEdge { from: Option<InputState>, to: InputState },
    InvalidReason { state: InputState, reason: Option<String> },
    SurfaceRegression { from: Surface, to: Surface },
    StageRegression { from: Stage, to: Stage },
    Invalid(String),
    Storage(anyhow::Error),
}

impl std::fmt::Display for LedgerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str("input record not found"),
            Self::Conflict { current_rev } => write!(f, "input record revision conflict (current rev {current_rev})"),
            Self::ForbiddenEdge { from, to } => write!(f, "input state {} -> {to} is not allowed", from.map_or("(new)", InputState::as_str)),
            Self::InvalidReason { state, reason } => write!(f, "reason {reason:?} is not valid for state {state}"),
            Self::SurfaceRegression { from, to } => write!(f, "surface {from} -> {to} is not allowed"),
            Self::StageRegression { from, to } => write!(f, "stage {from} -> {to} would move backwards"),
            Self::Invalid(message) => f.write_str(message),
            Self::Storage(error) => write!(f, "input ledger storage: {error:#}"),
        }
    }
}

impl std::error::Error for LedgerError {}

impl From<rusqlite::Error> for LedgerError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.into())
    }
}

impl From<anyhow::Error> for LedgerError {
    fn from(error: anyhow::Error) -> Self {
        Self::Storage(error)
    }
}

impl From<serde_json::Error> for LedgerError {
    fn from(error: serde_json::Error) -> Self {
        Self::Storage(error.into())
    }
}

// ---------------------------------------------------------------------------
// Rules (pure)

/// §2.3 state machine. `from: None` is a record that does not exist yet.
/// `details` key of a `waiting{returned}` written after the runtime proved the
/// steer that carried this input never read (057 §6, invariant 8's exception).
pub const CARRIER_UNREAD: &str = "carrierUnread";

fn carrier_unread_return(kind: InputKind, to: &Next) -> bool {
    kind != InputKind::Owner
        && to.state == InputState::Waiting
        && to.reason.as_deref() == Some("returned")
        && to.details.as_ref().is_some_and(|details| details[CARRIER_UNREAD] == true)
}

pub fn edge_allowed(kind: InputKind, from: Option<InputState>, to: &Next) -> bool {
    use InputState::*;
    let owner = kind == InputKind::Owner;
    let Some(from) = from else {
        return matches!(to.state, Received | Held | Waiting | Sending | Rejected | Failed);
    };
    let reverted = to.state == Discarded && to.reason.as_deref() == Some(HISTORY_REVERTED);
    if to.state==Discarded && to.reason.as_deref()==Some("settlementClosed")
        && to.details.as_ref().is_some_and(|details|details["recipientTransferred"]==true)
        && matches!(kind,InputKind::DelegationResult|InputKind::DelegationQuestion|InputKind::DelegationStatus) {
        return matches!(from,Received|Held|Waiting|Sending|Accepted|Uncertain|Failed);
    }

    match (from, to.state) {
        // r274.2 §9.6: a confirmed history revert removed the record's turn.
        // Only this reason takes these edges; and it takes no other edge
        // into `discarded` but `uncertain`'s (a record without a turn is
        // never retracted).
        (InContext | Accepted | Sending, Discarded) => reverted,
        (Failed, Discarded) if reverted => owner,
        (Waiting, Discarded) if reverted => false,
        // Same state: reason / stage / turn / stalled / order updates.
        (Received | Held | Waiting | Sending | Accepted | Uncertain, same) if same == from => true,
        (Received | Held, Uncertain) if to.reason.as_deref() == Some("outcomeUnknown")
            && to.details.as_ref().is_some_and(|details| details["absoluteDeadlineElapsed"] == true || details["sourceRecoveryUnconfirmed"] == true) => true,
        (Received, Held | Waiting | Sending | Rejected | Failed) => true,
        (Held, Sending | Waiting | Failed) => true,
        (Waiting, Sending | Discarded | Superseded | Rejected) => true,
        // Read without a delivery turn (agent_wait, a read tool).
        (Waiting, InContext) => matches!(to.evidence, Some(Evidence::ClaimedByWait | Evidence::ReadByTool)),
        (Sending, Accepted | InContext | Uncertain) => true,
        // Runtime said Absent / held / retry, nothing entered; for the owner an
        // explicit refusal that cutex re-queues. The surface never moves back
        // (timeline stays timeline: surface_move_allowed, owner decision 8).
        (Sending, Waiting) => true,
        (Sending, Failed) => owner,
        (Accepted, InContext | Uncertain) => true,
        // `cutex/input/interruptSend`: the owner takes a steered input out of
        // the running turn and sends it as the next turn's first input.
        (Accepted, Sending) => owner && to.reason.as_deref() == Some("interruptSend"),
        // ... and, the turn interrupted, waits for a busy host authority
        // fence (contract §5: `held{hostBusy}`, sent when the fence is free).
        (Sending, Held) => owner && interrupt_send_marked(to),
        (Accepted, Failed) => owner,
        // r274.2 §9.4: an owner resend (`waiting{ownerRetry}`) refused before
        // any native IO (the runtime is still away): failed again, with its
        // actions; never left waiting with none.
        (Waiting, Failed) => owner || to.details.as_ref().is_some_and(|details| details["retryRefused"] == true),
        // The same for a resend whose first outcome stays unknown (an earlier
        // send is unresolved): back to `uncertain`, with its actions.
        (Waiting, Uncertain) => owner || to.details.as_ref().is_some_and(|details| details["retryRefused"] == true),
        (Uncertain, InContext | Discarded) => true,
        (Accepted | Uncertain, Waiting) if carrier_unread_return(kind, to) => true,
        // r274.2 §9.4: the owner discards a failed message of theirs.
        (Failed, Discarded) => owner && to.reason.as_deref() == Some("ownerDiscarded"),
        // The owner's resend / retry, same id.
        (Uncertain | Failed, Waiting) => to.reason.as_deref() == Some("ownerRetry"),
        _ => false,
    }
}

/// §2.5 surface of a new record (or of an owner retry). `held{paused|stopHold}`
/// sessions put an owner `auto` / `interrupt` in nextUp: the caller passes that
/// surface explicitly, since the ledger does not read session state.
pub fn creation_surface(kind: InputKind, intent: &InputIntent) -> Surface {
    match kind {
        InputKind::Owner => {
            if intent.strategy == Some(Strategy::Queue) { Surface::NextUp } else { Surface::Timeline }
        }
        InputKind::AgentMessage | InputKind::PeerMessage => {
            if intent.timing == Timing::Passive { Surface::PendingNotes } else { Surface::NextUp }
        }
        InputKind::Alarm | InputKind::SystemNotice | InputKind::DelegationResult
        | InputKind::DelegationQuestion | InputKind::DelegationStatus => {
            if intent.timing == Timing::Passive { Surface::PendingNotes } else { Surface::NextUp }
        }
        InputKind::DelegationRequest => Surface::NextUp,
    }
}

/// §2.5 surface after a transition, when the caller gives none. `stage`:
/// the record's stage after the transition. Owner rule 2026-11-03 (r274.2
/// §9.4): the timeline shows only what has happened or is running, so a
/// record leaves nextUp / pendingNotes only with read evidence. An owner
/// record: `inContext` (any evidence), or the Claude `accepted[offered]`
/// (delivered into the tool gap). Any other kind: `inContext` with a turn
/// (or the turn-start input), or an `accepted` write that carries its read
/// signal (`details.readSignal`: the native item, the steer echo,
/// `offered`, the turn start). `accepted` + `turnId` alone (a steer the runtime only
/// queued, an adapter `registered`) is still pending input.
fn moved_surface(kind: InputKind, current: Surface, next: &Next, turn_id: Option<&str>, stage: Option<Stage>) -> Surface {
    use InputState::*;
    match next.state {
        Discarded | Superseded | Rejected => return Surface::None,
        InContext if matches!(next.evidence, Some(Evidence::ClaimedByWait | Evidence::ReadByTool)) => {
            return Surface::None;
        }
        _ => {}
    }
    let read_in = matches!(next.state, Accepted | InContext);
    match current {
        Surface::NextUp if read_in && kind == InputKind::Owner => {
            let read = next.state == InContext || stage == Some(Stage::Offered);
            if read { Surface::Timeline } else { current }
        }
        Surface::NextUp | Surface::PendingNotes if read_in => {
            let read = match next.state {
                Accepted => turn_id.is_some() && carries_read_signal(next),
                _ => turn_id.is_some() || (current == Surface::NextUp && next.evidence == Some(Evidence::TurnStartInput)),
            };
            if read { Surface::Timeline } else { current }
        }
        _ => current,
    }
}

/// Whether an `accepted` write names how the agent was shown the record
/// (`details.readSignal`, r274.2 §9.1): its read evidence.
fn carries_read_signal(next: &Next) -> bool {
    next.details.as_ref().is_some_and(|details| details.get(READ_SIGNAL).is_some_and(Value::is_string))
}

fn surface_move_allowed(from: Surface, to: Surface) -> bool {
    from == to
        || to == Surface::None && from != Surface::None
        || to == Surface::Timeline && matches!(from, Surface::NextUp | Surface::PendingNotes)
}

/// `details` key of a Claude record the agent was shown but that is not
/// proven in context yet (r274.2 §9.1, on its `accepted` timeline write):
/// `nativeItem` (an `external_event` item), `steerEcho` (the steer's own
/// user item) or `offered` (`claude/input/stageChanged offered`). r274.2
/// §9.4: also `turnStart` on a non-owner `accepted` write whose submit
/// started the turn with it (an exclusive delegation results event).
pub const READ_SIGNAL: &str = "readSignal";

/// Kinds whose `cutex/input/retry` resends the same id to a recipient that
/// may already have it (054 §2.10): bus messages and delegation results /
/// questions / status. `details.mayArriveTwice` says so on their records.
pub fn redelivery_may_duplicate(kind: InputKind) -> bool {
    matches!(
        kind,
        InputKind::AgentMessage | InputKind::PeerMessage | InputKind::Alarm | InputKind::SystemNotice
            | InputKind::DelegationResult | InputKind::DelegationQuestion | InputKind::DelegationStatus
    )
}

/// `details.interruptSend` of a transition: the owner's interruptSend is
/// waiting for the host authority fence (`held{hostBusy}`).
pub fn interrupt_send_marked(next: &Next) -> bool {
    next.reason.as_deref() == Some("hostBusy")
        && next.details.as_ref().is_some_and(|details| details.get("interruptSend").is_some_and(Value::is_object))
}

/// Whether the agent was shown an `accepted` owner record already: the
/// Claude `accepted[offered]`, or (r274.2 §9.1) one moved to the timeline at
/// its native item (bound `timelineItemId`: a steer echo or `offered`).
/// A Codex steer is `accepted` only until its echo makes it `inContext`.
pub fn shown_to_agent(record: &InputRecord) -> bool {
    record.stage == Some(Stage::Offered) || record.timeline_item_id.is_some()
}

/// Whether `cutex/input/interruptSend` may run for a record (r274
/// `interrupt-send-contract.md`, "When it is legal"), given the session's
/// §2.6 delivery: an owner record that is `accepted` into the running turn
/// (not one the agent was shown already, `shown`: [`shown_to_agent`],
/// r274.2) (`turnId == delivery.turn.turnId`), `held` (any reason), or
/// `waiting{turnRunning | turnFinishing}`, while the delivery shows a running
/// turn: `turn` or `finishing`, or `held{hostBusy}` with a turn (§2.6 order
/// 4 reports held owner input before the turn, so a held record never sees
/// `turn`; the contract lists `held` records as legal while a turn runs).
pub fn interrupt_send_legal(
    kind: InputKind,
    state: InputState,
    reason: Option<&str>,
    turn_id: Option<&str>,
    shown: bool,
    delivery: Option<&Value>,
) -> bool {
    if kind != InputKind::Owner {
        return false;
    }
    let Some(delivery) = delivery else { return false };
    let Some(running) = delivery.pointer("/turn/turnId").and_then(Value::as_str) else { return false };
    let session_state = delivery.get("state").and_then(Value::as_str);
    let turn_runs = matches!(session_state, Some("turn" | "finishing"))
        || (session_state == Some("held") && delivery.get("reason").and_then(Value::as_str) == Some("hostBusy"));
    turn_runs
        && match state {
            // Shown to the agent already (r274.2): a resend would
            // duplicate it.
            InputState::Accepted => turn_id == Some(running) && !shown,
            InputState::Held => true,
            InputState::Waiting => matches!(reason, Some("turnRunning" | "turnFinishing")),
            _ => false,
        }
}

/// r274.2 §9.4 (owner decision 2026-11-03, no auto-redelivery): an owner
/// record left `sending` / `accepted` by a runtime that went away mid-turn
/// (`stalled{runtimeDisconnected}`). cutex never sends it again by itself;
/// the owner gets `retry` and `dismiss` on it. Either first writes
/// `uncertain{runtimeLost}` (the outcome is unknown once the owner stops
/// waiting), then `waiting{ownerRetry}` / `discarded{ownerDiscarded}`.
///
/// r274.2 §9.4 action gaps (Director 2026-11-03): also an owner `accepted`
/// record the settle engine proved will not progress by itself
/// (`stalled{noProgress}` with `details.stuck` = one of [`STUCK_CASES`]:
/// the runtime generation changed since it was accepted, the lease of its
/// turn is stale, or an orphan nothing will read or settle).
///
/// Also [`stranded_resend`].
pub fn owner_stuck(record: &InputRecord) -> bool {
    record.kind == InputKind::Owner
        && (stranded_resend(record)
            || matches!(record.state, InputState::Sending | InputState::Accepted)
                && record.stalled.as_ref().is_some_and(|stalled| {
                    stalled.reason == "runtimeDisconnected" || (stalled.reason == "noProgress" && stuck_case(record).is_some())
                }))
}

/// An owner `waiting{ownerRetry}` the stall check marked. A resend in progress
/// leaves that state within its own request; one a host before this rule
/// refused stays in it for good, with no queue item, held input or timer
/// (owner report 2026-11-07).
pub fn stranded_resend(record: &InputRecord) -> bool {
    record.kind == InputKind::Owner
        && record.state == InputState::Waiting
        && record.reason.as_deref() == Some("ownerRetry")
        && record.stalled.as_ref().is_some_and(|stalled| stalled.reason == "noProgress")
}

/// `details` key naming why an owner `accepted` record is stuck.
pub const STUCK: &str = "stuck";
/// `details.stuck` values (free-form `details`, append only).
pub const STUCK_CASES: &[&str] = &["generationChanged", "leaseStale", "orphan"];

/// The [`STUCK_CASES`] value on `record`, if any.
pub fn stuck_case(record: &InputRecord) -> Option<&'static str> {
    let case = record.details.get(STUCK).and_then(Value::as_str)?;
    STUCK_CASES.iter().copied().find(|known| *known == case)
}

/// The `uncertain` reason the owner's retry / dismiss writes first on a
/// stuck record: the runtime that had it is gone (`runtimeLost`), or cutex
/// cannot tell where it is (`outcomeUnknown`).
pub fn stuck_uncertain_reason(record: &InputRecord) -> &'static str {
    if stranded_resend(record) {
        return "outcomeUnknown";
    }
    match (record.stalled.as_ref().map(|stalled| stalled.reason.as_str()), stuck_case(record)) {
        (Some("noProgress"), Some("leaseStale" | "orphan")) => "outcomeUnknown",
        _ => "runtimeLost",
    }
}

/// The `actions` of `record` under the session's §2.6 `delivery`.
pub fn record_actions(record: &InputRecord, delivery: Option<&Value>) -> Vec<Action> {
    if owner_stuck(record) {
        return vec![Action::Retry, Action::Dismiss];
    }
    let mut actions = actions_with_delivery(record.kind, record.state, record.reason.as_deref(), record.turn_id.as_deref(), shown_to_agent(record), delivery);
    if record.state == InputState::Waiting && record.kind != InputKind::Owner && record.kind != InputKind::DelegationRequest {
        if let Some(held) = record.details.get("ownerHold").and_then(Value::as_bool) {
            actions.push(if held { Action::ReleaseHold } else { Action::Hold });
        }
    }
    actions
}

/// [`actions_for`] plus `interruptSend` when [`interrupt_send_legal`] under
/// the session's stored delivery (the one clients see at the same revision).
pub fn actions_with_delivery(
    kind: InputKind,
    state: InputState,
    reason: Option<&str>,
    turn_id: Option<&str>,
    shown: bool,
    delivery: Option<&Value>,
) -> Vec<Action> {
    let mut actions = actions_for(kind, state, reason);
    if interrupt_send_legal(kind, state, reason, turn_id, shown, delivery) {
        actions.push(Action::InterruptSend);
    }
    actions
}

/// Owner actions cutex offers for a record (§2.2 `actions`, §2.14).
pub fn actions_for(kind: InputKind, state: InputState, reason: Option<&str>) -> Vec<Action> {
    match state {
        InputState::Failed => vec![Action::Retry, Action::Dismiss],
        InputState::Uncertain => vec![Action::Retry, Action::Dismiss],
        InputState::Waiting if kind == InputKind::Owner && reason == Some("explicitQueue") => {
            vec![Action::Cancel, Action::SendNow, Action::Edit, Action::Dismiss]
        }
        InputState::Waiting | InputState::Held | InputState::Accepted => {
            vec![Action::Dismiss]
        }
        _ => Vec::new(),
    }
}

/// §2.3 default deadlines (all configurable later through settings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadlines {
    pub received: Duration,
    pub held: Duration,
    pub sending: Duration,
    /// After the carrying turn ended (set by the settle code, C5).
    pub receipt_grace: Duration,
    pub stall_after: Duration,
}

impl Default for Deadlines {
    fn default() -> Self {
        Self {
            received: Duration::from_secs(5),
            held: Duration::from_secs(60),
            sending: Duration::from_secs(30),
            receipt_grace: Duration::from_secs(60),
            stall_after: Duration::from_secs(30),
        }
    }
}

impl Deadlines {
    /// Deadline of a state entered at `now`. `accepted` has none until its turn
    /// ends; `waiting` has none (stall detection instead).
    pub fn for_state(&self, state: InputState, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let after = match state {
            InputState::Received => self.received,
            InputState::Held => self.held,
            InputState::Sending => self.sending,
            _ => return None,
        };
        chrono::Duration::from_std(after).ok().map(|after| now + after)
    }
}

/// §2.3 "after the deadline" column: what an expired record becomes. The
/// settle code (C5) runs the bounded reconcile (§2.10) before applying the
/// `accepted` outcome; `None` means nothing is due.
pub fn deadline_next(record: &InputRecord) -> Option<Next> {
    let owner = record.kind == InputKind::Owner;
    match record.state {
        InputState::Received if owner => Some(Next::failed("routeLost")),
        InputState::Received => Some(Next::waiting("autoRetry")),
        InputState::Held => Some(Next::failed("admission_timeout")),
        InputState::Sending => Some(Next::uncertain("outcomeUnknown")),
        InputState::Accepted => Some(Next::uncertain("receiptTimeout")),
        _ => None,
    }
}

fn validate_reason(state: InputState, reason: Option<&str>) -> Result<(), LedgerError> {
    let ok = match (state.reasons(), reason) {
        (Some(allowed), Some(reason)) => allowed.contains(&reason),
        (Some(_), None) => matches!(state, InputState::Sending | InputState::Accepted | InputState::InContext),
        (None, None) => true,
        (None, Some(_)) => false,
    };
    if ok { Ok(()) } else { Err(LedgerError::InvalidReason { state, reason: reason.map(str::to_string) }) }
}

fn validate_id(label: &str, value: &str) -> Result<(), LedgerError> {
    if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        return Err(LedgerError::Invalid(format!("{label} must be a non-empty id without control characters")));
    }
    Ok(())
}

fn timestamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn preview(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.chars().take(PREVIEW_CHARS).collect()
}

/// Apply `next` to `current` (or create). Pure: no storage, no clock.
fn apply(
    session: &str,
    input_id: &str,
    current: Option<&InputRecord>,
    next: &Next,
    now: DateTime<Utc>,
    deadlines: &Deadlines,
    delivery: Option<&Value>,
) -> Result<InputRecord, LedgerError> {
    let kind = match (current, &next.create) {
        (Some(record), _) => record.kind,
        (None, Some(create)) => create.kind,
        (None, None) => return Err(LedgerError::NotFound),
    };
    let from = current.map(|record| record.state);
    if !edge_allowed(kind, from, next) {
        return Err(LedgerError::ForbiddenEdge { from, to: next.state });
    }
    validate_reason(next.state, next.reason.as_deref())?;
    if next.reason.as_deref() == Some("carriedByOwner") && kind == InputKind::Owner {
        return Err(LedgerError::Invalid("the carrying owner is not a carried input".into()));
    }
    match (next.state, next.evidence) {
        (InputState::InContext, None) => return Err(LedgerError::Invalid("inContext requires evidence".into())),
        (InputState::InContext, Some(_)) | (_, None) => {}
        (state, Some(_)) => return Err(LedgerError::Invalid(format!("evidence is only valid for inContext, not {state}"))),
    }
    if next.stage.is_some() && next.state != InputState::Accepted {
        return Err(LedgerError::Invalid("stage is only valid for accepted".into()));
    }
    if next.state == InputState::Superseded && next.superseded_by.is_none() {
        return Err(LedgerError::Invalid("superseded requires supersededBy".into()));
    }
    let now_text = timestamp(now);
    let retry = matches!(from, Some(InputState::Uncertain | InputState::Failed)) && next.state == InputState::Waiting;
    let mut record = match current {
        Some(record) => record.clone(),
        None => {
            let create = next.create.as_ref().expect("checked above");
            validate_id("inputId", input_id)?;
            validate_id("cutexSessionId", session)?;
            InputRecord {
                input_id: input_id.to_string(),
                cutex_session_id: session.to_string(),
                kind,
                sender: create.sender.clone(),
                intent: create.intent.clone(),
                preview: preview(&create.preview),
                created_at: now_text.clone(),
                state: next.state,
                reason: None,
                since: now_text.clone(),
                deadline: None,
                wake_schedule: None,
                stage: None,
                surface: creation_surface(kind, &create.intent),
                order: None,
                turn_id: None,
                timeline_item_id: None,
                runtime_generation: None,
                attempt: 1,
                superseded_by: None,
                batch_id: None,
                carries: Vec::new(),
                ask_id: create.ask_id.clone(),
                remote: create.remote.clone(),
                stalled: None,
                actions: Vec::new(),
                error: None,
                evidence: None,
                details: json!({}),
                rev: 0,
                state_revision: 0,
                updated_at: now_text.clone(),
            }
        }
    };
    let state_changed = current.is_none_or(|record| record.state != next.state);
    if let Some(carries) = &next.carries {
        if kind != InputKind::Owner || carries.len() > MAX_BATCH_ITEMS {
            return Err(LedgerError::Invalid("carries requires an owner and at most 100 input ids".into()));
        }
        let mut seen = std::collections::BTreeSet::new();
        for id in carries {
            validate_id("carries inputId", id)?;
            if id == input_id || !seen.insert(id) {
                return Err(LedgerError::Invalid("carries must contain distinct other input ids".into()));
            }
        }
        record.carries = carries.clone();
    }

    // Stage: accepted only, never backwards while accepted.
    let stage = if next.state == InputState::Accepted {
        match (record.stage.filter(|_| !state_changed), next.stage) {
            (Some(old), Some(new)) if new.order() < old.order() => {
                return Err(LedgerError::StageRegression { from: old, to: new });
            }
            (old, new) => new.or(old),
        }
    } else {
        None
    };

    if let Some(turn) = &next.turn_id {
        validate_id("turnId", turn)?;
        record.turn_id = Some(turn.clone());
    }
    let surface = if retry {
        // r274.2 retries wait on nextUp, except an idle timeline failure
        // before a turn started (owner decision 2026-11-04).
        let retry_surface = if kind == InputKind::Owner {
            // An idle send that failed before a turn started keeps its bubble
            // in place on explicit retry (owner decision 2026-11-04).
            if record.surface == Surface::Timeline && record.state == InputState::Failed && record.turn_id.is_none() {
                Surface::Timeline
            } else { Surface::NextUp }
        } else { creation_surface(kind, &record.intent) };
        next.surface.unwrap_or(retry_surface)
    } else if current.is_none() {
        next.surface.unwrap_or(record.surface)
    } else {
        let target = next
            .surface
            .unwrap_or_else(|| moved_surface(kind, record.surface, next, record.turn_id.as_deref(), stage));
        if !surface_move_allowed(record.surface, target) {
            return Err(LedgerError::SurfaceRegression { from: record.surface, to: target });
        }
        target
    };

    record.state = next.state;
    if next.state.is_settled() || surface == Surface::Timeline { record.wake_schedule = None; }
    record.reason = next.reason.clone();
    record.stage = stage;
    record.surface = surface;
    // r274.2 §9.6: a history revert keeps the read evidence as audit.
    if !(next.state == InputState::Discarded && next.reason.as_deref() == Some(HISTORY_REVERTED)) {
        record.evidence = next.evidence;
    }
    if state_changed {
        record.since = now_text.clone();
    }
    if retry {
        record.attempt += 1;
        record.turn_id = None;
        record.timeline_item_id = None;
    }
    // Runtime said Absent: same id goes again (§2.10), next attempt.
    let carrier_unread = matches!(from, Some(InputState::Accepted | InputState::Uncertain)) && carrier_unread_return(kind, next);
    if carrier_unread {
        record.turn_id = None;
    }
    if (from == Some(InputState::Sending) || carrier_unread) && next.state == InputState::Waiting && next.reason.as_deref() == Some("returned") {
        record.attempt += 1;
        // The next attempt is another native item (a new event id).
        record.timeline_item_id = None;
    }
    if let Some(details) = &next.details {
        record.details = details.clone();
    } else if state_changed {
        record.details = json!({});
    }
    // 054 §2.10: a bus message or delegation result is resent with the same
    // id; the recipient may see it twice. Said on the record that offers the
    // retry and on the retried one, so the client can word the button.
    if redelivery_may_duplicate(kind)
        && (matches!(next.state, InputState::Failed | InputState::Uncertain)
            || (next.state == InputState::Waiting && next.reason.as_deref() == Some("ownerRetry")))
    {
        if !record.details.is_object() {
            record.details = json!({});
        }
        record.details["mayArriveTwice"] = Value::Bool(true);
        if record.sender.kind == SenderKind::ClaudeTask {
            record.details["retryEffect"] = json!("redeliverNotice");
            record.details["taskRestartsOnRetry"] = json!(false);
            record.details["recoveryDescription"] = json!("Delivery of this Claude Task notice is unconfirmed. Retry sends the notice again; it does not restart the task. Dismiss ignores the notice.");
        }
    }
    record.error = match next.state {
        InputState::Failed | InputState::Rejected => Some(next.error.clone().unwrap_or_else(|| InputError {
            code: next.reason.clone().unwrap_or_default(),
            retryable: next.state == InputState::Failed,
            details: record.details.clone(),
        })),
        _ => None,
    };
    if let Some(order) = &next.order {
        record.order = order.clone();
    }
    if record.surface != Surface::NextUp {
        record.order = None;
    }
    if let Some(item) = &next.timeline_item_id {
        record.timeline_item_id = Some(item.clone());
    }
    if let Some(generation) = next.runtime_generation {
        record.runtime_generation = Some(generation);
    }
    if let Some(id) = &next.superseded_by {
        record.superseded_by = Some(id.clone());
    }
    if let Some(batch) = &next.batch_id {
        record.batch_id = Some(batch.clone());
    }
    record.stalled = match &next.stalled {
        Some(stalled) => stalled.clone(),
        None if state_changed => None,
        None => record.stalled.clone(),
    };
    record.deadline = if next.state.is_settled() {
        None
    } else {
        match next.deadline {
            Some(explicit) => explicit.map(timestamp),
            None if state_changed => deadlines.for_state(next.state, now).map(timestamp),
            None => record.deadline.clone(),
        }
    };
    // Only transport before admission has an absolute budget. Accepted,
    // held and queued input can normally wait for a turn; Dismiss is its exit.
    let transport = matches!(record.state, InputState::Received | InputState::Sending)
        && matches!(record.surface, Surface::NextUp | Surface::PendingNotes);
    let previous_transport = current.filter(|old|
        matches!(old.state, InputState::Received | InputState::Sending));
    let prior = previous_transport.and_then(|old| old.details["observationDeadline"].as_str())
        .and_then(|text| DateTime::parse_from_rfc3339(text).ok()).map(|at|at.with_timezone(&Utc));
    if let Some(details)=record.details.as_object_mut(){details.remove("observationDeadline");}
    if transport {
        if !record.details.is_object(){record.details=json!({});}
        // Seed only an in-scope transport observation. Never back-date held or
        // accepted rows using their old since, including during an upgrade.
        let start=previous_transport
            .and_then(|old|DateTime::parse_from_rfc3339(&old.since).ok()).map(|at|at.with_timezone(&Utc)).unwrap_or(now);
        let bound=prior.unwrap_or(start+chrono::Duration::seconds(60));
        record.details["observationDeadline"]=json!(timestamp(bound));
        let requested=record.deadline.as_deref().and_then(|text|DateTime::parse_from_rfc3339(text).ok()).map(|at|at.with_timezone(&Utc));
        record.deadline=Some(timestamp(requested.map_or(bound,|at|at.min(bound))));
    } else if !state_changed && current.is_some_and(|old|
        old.details["observationDeadline"].as_str()==old.deadline.as_deref()
            && old.deadline.is_some()) && next.deadline.is_none() {
        // Remove the previous build's injected cap when metadata next changes.
        // Preserve independently requested receipt/admission deadlines.
        record.deadline=None;
    }
    record.actions = record_actions(&record, delivery);
    Ok(record)
}

/// Whether `after` differs from `before` in anything a client sees.
fn observably_changed(before: &InputRecord, after: &InputRecord) -> bool {
    let mut probe = after.clone();
    probe.rev = before.rev;
    probe.state_revision = before.state_revision;
    probe.updated_at = before.updated_at.clone();
    probe != *before
}

// ---------------------------------------------------------------------------
// Leaf lock guard

thread_local! {
    static LEDGER_LOCK_HELD: Cell<bool> = const { Cell::new(false) };
}

/// Whether this thread holds the ledger lock.
pub fn thread_holds_ledger_lock() -> bool {
    LEDGER_LOCK_HELD.with(Cell::get)
}

/// Debug guard for every other lock (session lane, host fence, AskStore): the
/// ledger lock is a leaf, nothing may be acquired while it is held.
#[track_caller]
pub fn assert_ledger_lock_not_held(what: &str) {
    if cfg!(debug_assertions) && thread_holds_ledger_lock() {
        panic!("054 lock protocol: {what} acquired while holding the input ledger (leaf) lock");
    }
}

struct LedgerGuard<'a> {
    connection: MutexGuard<'a, Option<Connection>>,
    /// Cooperating writers queue outside SQLite's bounded busy handler.
    writer: Option<std::fs::File>,
    disposition: Option<super::input_disposal::Guard>,
}

impl Drop for LedgerGuard<'_> {
    fn drop(&mut self) {
        // Closing this operation's handle releases its cross-process lock.
        drop(self.writer.take());
        drop(self.disposition.take());
        LEDGER_LOCK_HELD.with(|held| held.set(false));
    }
}

// ---------------------------------------------------------------------------
// Store

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetResult {
    pub ledger_epoch: String,
    pub state_revision: u64,
    pub records: Vec<InputRecord>,
    pub missing: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListQuery {
    pub since_revision: Option<u64>,
    pub include_settled_secs: u64,
}

impl Default for ListQuery {
    fn default() -> Self {
        Self { since_revision: None, include_settled_secs: DEFAULT_INCLUDE_SETTLED_SECS }
    }
}

/// `cutex/input/list`. `delivery`: the last SessionDelivery (§2.6) written
/// with [`InputLedger::delivery_changed`], also on incremental reads; absent
/// until the session's first one.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListResult {
    pub ledger_epoch: String,
    pub state_revision: u64,
    pub records: Vec<InputRecord>,
    pub complete: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub resync_required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SinceResult {
    pub ledger_epoch: String,
    pub state_revision: u64,
    pub records: Vec<InputRecord>,
    /// The revision is older than the compaction floor, or more than the
    /// limit changed: read the full list instead.
    pub resync_required: bool,
    pub delivery: Option<Value>,
}

/// Result of [`InputLedger::delivery_changed`].
#[derive(Debug, Clone, PartialEq)]
pub struct DeliveryChange {
    /// The stored delivery, `stateRevision` filled in.
    pub delivery: Value,
    /// `false`: equal to the stored one (ignoring `stateRevision` and
    /// `observedAt`); no revision used, no event to publish.
    pub changed: bool,
    pub ledger_epoch: String,
    pub state_revision: u64,
    outboxed: bool,
    /// Open owner records whose `actions` changed with this delivery (the
    /// running turn decides `interruptSend`), at the same revision.
    pub records: Vec<InputRecord>,
}

/// One record whose deadline has passed (`InputLedger::due`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueInput {
    pub cutex_session_id: String,
    pub input_id: String,
    pub state: InputState,
    pub rev: u64,
    pub deadline_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionRow {
    state_revision: u64,
    epoch: String,
    compacted_through: u64,
    delivery: Option<String>,
}

pub struct InputLedger {
    path: PathBuf,
    connection: Mutex<Option<Connection>>,
    deadlines: Deadlines,
    /// Whether writes put their events into the outbox (see [`Outbox`]).
    outbox: Outbox,
    /// Where a drain appends the events (the event journal).
    sink: fn(&str, String, Value) -> anyhow::Result<()>,
    /// The in-process publisher: whoever holds it may try the cross-process
    /// publish lock file (the handle lives here, opened lazily).
    publisher: Mutex<Option<std::fs::File>>,
    /// A publisher of this process found [`Self::publisher`] busy: the
    /// holder drains again after releasing it (no lost wakeup).
    rerun: std::sync::atomic::AtomicBool,
    sweeper: OnceLock<()>,
    #[cfg(test)]
    clock: Mutex<Option<DateTime<Utc>>>,
    #[cfg(test)]
    writer_waiting: Mutex<Option<std::sync::mpsc::Sender<()>>>,
}

/// Outbox mode of a ledger. The process ledger follows the switch; a ledger
/// opened with [`InputLedger::open`] has none (its writers append directly).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outbox {
    Off,
    Switch,
    #[cfg(test)]
    Always,
}

impl std::fmt::Debug for InputLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputLedger").field("path", &self.path).finish_non_exhaustive()
    }
}

/// The process ledger at `<runtime>/management-v2/input-ledger.sqlite3`.
pub fn default_ledger() -> anyhow::Result<&'static InputLedger> {
    static LEDGER: OnceLock<InputLedger> = OnceLock::new();
    if let Some(ledger) = LEDGER.get() {
        return Ok(ledger);
    }
    let path = crate::config::paths::runtime_dir()?.join("management-v2").join(DATABASE_FILE);
    let ledger = LEDGER.get_or_init(|| InputLedger { outbox: Outbox::Switch, ..InputLedger::open(path) });
    if enabled() {
        // Backstop for rows a crashed publisher (of any process) left behind.
        ledger.ensure_sweeper();
    }
    Ok(ledger)
}

impl InputLedger {
    /// Lazy: the file is created on first use.
    pub fn open(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            connection: Mutex::new(None),
            deadlines: Deadlines::default(),
            outbox: Outbox::Off,
            sink: append_now,
            publisher: Mutex::new(None),
            rerun: std::sync::atomic::AtomicBool::new(false),
            sweeper: OnceLock::new(),
            #[cfg(test)]
            clock: Mutex::new(None),
            #[cfg(test)]
            writer_waiting: Mutex::new(None),
        }
    }

    /// Tests: a ledger standing in for one process's ledger, with an outbox
    /// regardless of the switch, draining into `sink`.
    #[cfg(test)]
    pub(crate) fn with_outbox(mut self, sink: fn(&str, String, Value) -> anyhow::Result<()>) -> Self {
        self.outbox = Outbox::Always;
        self.sink = sink;
        self
    }

    /// Tests: the process-ledger mode (outbox only while the switch is on).
    #[cfg(test)]
    pub(crate) fn with_switched_outbox(mut self, sink: fn(&str, String, Value) -> anyhow::Result<()>) -> Self {
        self.outbox = Outbox::Switch;
        self.sink = sink;
        self
    }

    /// Whether a write now puts its event into the outbox. Evaluated BEFORE
    /// the ledger lock is taken ([`enabled`] may read config.json).
    fn outbox_on(&self) -> bool {
        let on = match self.outbox {
            Outbox::Off => false,
            Outbox::Switch => enabled(),
            #[cfg(test)]
            Outbox::Always => true,
        };
        // A test capturing its events on this thread gets them directly.
        #[cfg(test)]
        let on = on && !CAPTURED_EVENTS.with(|cell| cell.borrow().is_some());
        on
    }

    pub fn with_deadlines(mut self, deadlines: Deadlines) -> Self {
        self.deadlines = deadlines;
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn deadlines(&self) -> &Deadlines {
        &self.deadlines
    }

    fn now(&self) -> DateTime<Utc> {
        #[cfg(test)]
        if let Some(at) = *self.clock.lock().unwrap() {
            return at;
        }
        Utc::now()
    }

    #[cfg(test)]
    pub(crate) fn set_clock(&self, at: DateTime<Utc>) {
        *self.clock.lock().unwrap() = Some(at);
    }

    fn lock(&self) -> Result<LedgerGuard<'_>, LedgerError> {
        self.lock_connection(false)
    }

    fn lock_write(&self) -> Result<LedgerGuard<'_>, LedgerError> {
        self.lock_connection(true)
    }

    fn lock_connection(&self, writing: bool) -> Result<LedgerGuard<'_>, LedgerError> {
        if cfg!(debug_assertions) && thread_holds_ledger_lock() {
            panic!("054 lock protocol: the input ledger lock is not reentrant");
        }
        let disposition=if writing {Some(super::input_disposal::Guard::open_notifying(&self.path,false,|| {
            #[cfg(test)]
            if let Some(waiting)=self.writer_waiting.lock().unwrap().take(){let _=waiting.send(());}
        }).map_err(LedgerError::Storage)?)} else {None};
        let connection = self.connection.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        LEDGER_LOCK_HELD.with(|held| held.set(true));
        let mut guard = LedgerGuard { connection, writer: None, disposition };
        // Opening a connection also initializes schema and the epoch marker.
        // Serialize that first use; established read connections stay WAL readers.
        if writing || guard.connection.is_none() {
            guard.writer = Some(self.writer_lock()?);
        }
        if guard.connection.is_none() {
            *guard.connection = Some(open_watched(&self.path)?);
        }
        Ok(guard)
    }

    fn writer_lock_path(&self) -> PathBuf {
        let mut name = self.path.file_name().map(|name| name.to_os_string()).unwrap_or_default();
        name.push(".write.lock");
        self.path.with_file_name(name)
    }

    fn writer_lock(&self) -> anyhow::Result<std::fs::File> {
        let path = self.writer_lock_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(path)?;
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => {}
            Err(error) if crate::platform::file_lock::lock_contended(&error) => {
                #[cfg(test)]
                if let Some(waiting) = self.writer_waiting.lock().unwrap().take() {
                    let _ = waiting.send(());
                }
                // No SQL transaction is held while waiting. This is separate
                // from the publication try-lock; appends still run outside the
                // ledger lock and never wait on a publisher.
                fs2::FileExt::lock_exclusive(&file)?;
            }
            Err(error) => return Err(error.into()),
        }
        Ok(file)
    }

    /// The only writer. One immediate transaction: read the row, check
    /// `expect`, apply the §2.3 / §2.5 rules, upsert the row and bump the
    /// session's `stateRevision` when something observable changed.
    pub fn transition(
        &self,
        session: &str,
        input_id: &str,
        expect: Expect,
        next: Next,
    ) -> Result<Transition, LedgerError> {
        let deadlines = self.deadlines;
        self.write_one(session, input_id, expect, |current, delivery, now| {
            apply(session, input_id, current, &next, now, &deadlines, delivery)
        })
    }

    /// Finish a delivery whose source has already accepted the dismissal.
    /// A queued retry projection may restore default hold metadata meanwhile;
    /// compare that narrow change inside the write transaction, without a
    /// read/retry window or holding the ledger lock across source IO.
    fn discard_settled_delivery(&self, observed: &InputRecord) -> Result<Transition, LedgerError> {
        let deadlines = self.deadlines;
        self.write_one(&observed.cutex_session_id, &observed.input_id, Expect::ANY, |current, delivery, now| {
            let current = current.ok_or(LedgerError::NotFound)?;
            if current.rev != observed.rev {
                // The durable source projection can finish this same decision
                // first. Return its record without another revision/event.
                if current.kind == observed.kind && current.attempt == observed.attempt
                    && current.state == InputState::Discarded && current.reason.as_deref() == Some("ownerDiscarded") {
                    return Ok(current.clone());
                }
                let normalize = |record: &InputRecord| {
                    let mut probe = record.clone();
                    probe.rev = observed.rev;
                    probe.state_revision = observed.state_revision;
                    probe.updated_at = observed.updated_at.clone();
                    // Actions are derived from the restored hold metadata.
                    probe.actions = observed.actions.clone();
                    if let Some(details) = probe.details.as_object_mut() {
                        details.remove(crate::asks::ledger::SOURCE_VERSION);
                        if details.get("ownerHold") == Some(&json!(false)) { details.remove("ownerHold"); }
                    }
                    probe
                };
                let source_version = |record: &InputRecord| record.details[crate::asks::ledger::SOURCE_VERSION].as_u64();
                if normalize(current) != normalize(observed) || source_version(current) < source_version(observed) {
                    return Err(LedgerError::Conflict { current_rev: current.rev });
                }
            }
            apply(&observed.cutex_session_id, &observed.input_id, Some(current),
                &Next::discarded("ownerDiscarded"), now, &deadlines, delivery)
        })
    }

    /// Update presentation only, with the same revision/outbox as every input.
    pub fn annotate_wake_schedule(&self, session: &str, input_id: &str, expect: Expect, schedule: Option<super::input_ledger_wake::WakeSchedule>) -> Result<Transition, LedgerError> {
        self.write_one(session, input_id, expect, |current, _, _| {
            let mut record = current.ok_or(LedgerError::NotFound)?.clone();
            record.wake_schedule = schedule;
            Ok(record)
        })
    }

    /// Monotonic source fence, independent of whether a source state edge is
    /// applicable. Presentation/lifecycle fields stay exactly as observed.
    pub(crate) fn note_ask_source_version(&self, session: &str, input_id: &str, expect: Expect, version: u64) -> Result<Transition, LedgerError> {
        self.write_one(session, input_id, expect, |current, _, _| {
            let mut record = current.ok_or(LedgerError::NotFound)?.clone();
            if !record.details.is_object() { record.details = serde_json::json!({}); }
            let old = record.details[crate::asks::ledger::SOURCE_VERSION].as_u64().unwrap_or(0);
            record.details[crate::asks::ledger::SOURCE_VERSION] = serde_json::json!(old.max(version));
            Ok(record)
        })
    }

    /// Bind the native timeline item a record entered the model context as
    /// (054 §3.2: clients join native items and records on `timelineItemId`).
    /// Metadata only: state, surface, `inputId`, attempt and evidence stay; a
    /// changed record takes the normal `rev` / `stateRevision` bump and its
    /// `cutex/input/stateChanged`. Only an `inContext` record on the timeline
    /// with no item yet is bound (a bound record is never rebound: the same
    /// item is `changed: false`, another is refused). `batch`: the native
    /// batch (delegation event id) the item belongs to; a record that names
    /// another batch was proven by another attempt and is refused.
    pub fn bind_timeline_item(
        &self,
        session: &str,
        input_id: &str,
        expect: Expect,
        item: &str,
        batch: Option<&str>,
    ) -> Result<Transition, LedgerError> {
        validate_id("timelineItemId", item)?;
        self.write_one(session, input_id, expect, |current, _, _| {
            let Some(current) = current else { return Err(LedgerError::NotFound) };
            let refuse = |why: &str| Err(LedgerError::Invalid(format!("timelineItemId not bound: {why}")));
            match current.timeline_item_id.as_deref() {
                Some(bound) if bound == item => return Ok(current.clone()),
                Some(_) => return refuse("the record is bound to another native item"),
                None => {}
            }
            if current.state != InputState::InContext || current.surface != Surface::Timeline {
                return refuse("only an inContext timeline record is bound");
            }
            if batch.is_some_and(|batch| current.batch_id.as_deref().is_some_and(|own| own != batch)) {
                return refuse("the record was proven by another batch");
            }
            let mut record = current.clone();
            record.timeline_item_id = Some(item.to_string());
            Ok(record)
        })
    }

    /// One record's write as one immediate transaction: read the row, check
    /// `expect`, let `next_record` compute the record, upsert it and bump the
    /// session's `stateRevision` when something observable changed.
    fn write_one(
        &self,
        session: &str,
        input_id: &str,
        expect: Expect,
        next_record: impl FnOnce(Option<&InputRecord>, Option<&Value>, DateTime<Utc>) -> Result<InputRecord, LedgerError>,
    ) -> Result<Transition, LedgerError> {
        validate_id("cutexSessionId", session)?;
        validate_id("inputId", input_id)?;
        let now = self.now();
        let outbox = self.outbox_on();
        // 054 S25 timing (switch on only): the whole write incl. outbox rows.
        let _timing = timing::span(timing::Op::Transition, outbox);
        let lock_wait = timing::span(timing::Op::LockWait, outbox);
        let mut guard = self.lock_write()?;
        drop(lock_wait);
        let decision=guard.disposition.as_ref().expect("write fence").get(session,input_id).map_err(LedgerError::Storage)?;
        let LedgerGuard {connection,disposition,..}=&mut guard;
        let connection = connection.as_mut().expect("opened under lock");
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = read_record(&tx, session, input_id)?;
        let current_rev = current.as_ref().map_or(0, |record| record.rev);
        if let Some(expected) = expect.rev.filter(|_|decision.is_none()) {
            if expected != current_rev {
                return Err(LedgerError::Conflict { current_rev });
            }
        }
        let session_row = session_row(&tx, session)?;
        let delivery = parse_delivery(session_row.as_ref())?;
        let mut record=if let Some(decision)=&decision {
            let candidate=match &current {Some(record)=>record.clone(),None=>next_record(None,delivery.as_ref(),now)?};
            super::input_disposal::suppress(candidate,decision)
        } else {next_record(current.as_ref(),delivery.as_ref(),now)?};
        let changed = current.as_ref().is_none_or(|before| observably_changed(before, &record));
        if !changed {
            let record = current.expect("unchanged implies existing");
            drop(tx);
            let (epoch, revision) = match session_row {
                Some(row) => (row.epoch, row.state_revision),
                None => return Err(LedgerError::Storage(anyhow::anyhow!("input ledger session row missing"))),
            };
            return Ok(Transition { record, changed: false, created: false, ledger_epoch: epoch, state_revision: revision, outboxed: false });
        }
        // A session's first record takes the file epoch, the one an empty
        // read already reported; only a rebuild (C5, §2.12) changes it.
        let row = match session_row {
            Some(row) => row,
            None => SessionRow { state_revision: 0, epoch: file_epoch(&tx)?, compacted_through: 0, delivery: None },
        };
        let state_revision = row.state_revision.checked_add(1)
            .filter(|revision| *revision <= super::model::MAX_SAFE_SEQUENCE)
            .ok_or_else(|| LedgerError::Storage(anyhow::anyhow!("session stateRevision exhausted")))?;
        record.rev = current_rev + 1;
        record.state_revision = state_revision;
        record.updated_at = timestamp(now);
        upsert_session_revision(&tx, session, &row, state_revision)?;
        upsert_record(&tx, session, input_id, &record, state_revision, now)?;
        if outbox {
            outbox_put(&tx, session, state_revision, 0, STATE_CHANGED_EVENT,
                &state_changed_params(session, &row.epoch, state_revision, &record), now)?;
        }
        commit_timed(tx, outbox)?;
        Ok(Transition { record, changed: true, created: current.is_none(), ledger_epoch: row.epoch, state_revision, outboxed: outbox })
    }

    /// Several transitions in one session as ONE transaction and ONE
    /// `stateRevision` (Alice 11-02 decision 4: a queue merge is one step,
    /// clients never see a half-merged queue). All or nothing: a conflict, a
    /// forbidden edge or an invalid next fails the whole batch. Every changed
    /// record carries the shared revision; unchanged ones are returned as they
    /// are (`changed: false`). Ids must be distinct, at most [`MAX_BATCH_ITEMS`].
    pub fn transition_batch(
        &self,
        session: &str,
        items: Vec<(String, Expect, Next)>,
    ) -> Result<Vec<Transition>, LedgerError> {
        self.batch(session, items, None).map(|(transitions, _)| transitions)
    }

    /// [`Self::transition_batch`] and a new SessionDelivery (§2.6) as ONE
    /// transaction and ONE `stateRevision` (`cutex/input/interruptSend`: the
    /// record's first transition and the delivery change of the interrupt
    /// are one batch revision). The records' `actions` follow the new
    /// delivery, as do those of the session's other open owner records
    /// (published at the same revision). Outbox order: the delivery, then the
    /// records. Nothing changed (records and delivery equal): no revision.
    pub fn transition_batch_with_delivery(
        &self,
        session: &str,
        items: Vec<(String, Expect, Next)>,
        delivery: &Value,
    ) -> Result<(Vec<Transition>, DeliveryChange), LedgerError> {
        if !delivery.is_object() {
            return Err(LedgerError::Invalid("delivery must be an object".into()));
        }
        self.batch(session, items, Some(delivery))
            .map(|(transitions, change)| (transitions, change.expect("a delivery batch reports its delivery")))
    }

    fn batch(
        &self,
        session: &str,
        items: Vec<(String, Expect, Next)>,
        new_delivery: Option<&Value>,
    ) -> Result<(Vec<Transition>, Option<DeliveryChange>), LedgerError> {
        validate_id("cutexSessionId", session)?;
        if items.is_empty() || items.len() > MAX_BATCH_ITEMS {
            return Err(LedgerError::Invalid(format!("a batch holds 1..={MAX_BATCH_ITEMS} transitions")));
        }
        let mut seen = std::collections::HashSet::new();
        for (input_id, _, _) in &items {
            validate_id("inputId", input_id)?;
            if !seen.insert(input_id.as_str()) {
                return Err(LedgerError::Invalid(format!("inputId {input_id} appears twice in one batch")));
            }
        }
        let now = self.now();
        let outbox = self.outbox_on();
        // 054 S25 timing (switch on only): the whole write incl. outbox rows.
        let _timing = timing::span(timing::Op::Transition, outbox);
        let lock_wait = timing::span(timing::Op::LockWait, outbox);
        let mut guard = self.lock_write()?;
        drop(lock_wait);
        let decisions:std::collections::BTreeMap<_,_>=items.iter().filter_map(|(id,_,_)|
            match guard.disposition.as_ref().expect("write fence").get(session,id) {
                Ok(Some(decision))=>Some(Ok((id.clone(),decision))),Ok(None)=>None,Err(error)=>Some(Err(LedgerError::Storage(error)))
            }).collect::<Result<_,_>>()?;
        let LedgerGuard {connection,disposition,..}=&mut guard;
        let connection = connection.as_mut().expect("opened under lock");
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let session_row = session_row(&tx, session)?;
        let stored_delivery = parse_delivery(session_row.as_ref())?;
        let delivery_changed = new_delivery.is_some_and(|new| {
            stored_delivery.as_ref().is_none_or(|stored| comparable_delivery(stored) != comparable_delivery(new))
        });
        let delivery = new_delivery.or(stored_delivery.as_ref());
        let mut applied = Vec::with_capacity(items.len());
        for (input_id, expect, next) in &items {
            let current = read_record(&tx, session, input_id)?;
            let current_rev = current.as_ref().map_or(0, |record| record.rev);
            if let Some(expected) = expect.rev.filter(|_|!decisions.contains_key(input_id)) {
                if expected != current_rev {
                    return Err(LedgerError::Conflict { current_rev });
                }
            }
            let record=if let Some(decision)=decisions.get(input_id) {
                let candidate=match &current {Some(record)=>record.clone(),None=>apply(session,input_id,None,next,now,&self.deadlines,delivery)?};
                super::input_disposal::suppress(candidate,decision)
            } else {apply(session,input_id,current.as_ref(),next,now,&self.deadlines,delivery)?};
            let changed = current.as_ref().is_none_or(|before| observably_changed(before, &record));
            applied.push((input_id.as_str(), current, record, changed));
        }
        let any_changed = delivery_changed || applied.iter().any(|(_, _, _, changed)| *changed);
        let row = match session_row {
            Some(row) => row,
            None if any_changed => {
                SessionRow { state_revision: 0, epoch: file_epoch(&tx)?, compacted_through: 0, delivery: None }
            }
            None => return Err(LedgerError::Storage(anyhow::anyhow!("input ledger session row missing"))),
        };
        if !any_changed {
            drop(tx);
            let change = new_delivery.map(|_| DeliveryChange {
                delivery: stored_delivery.clone().unwrap_or(Value::Null), changed: false, ledger_epoch: row.epoch.clone(),
                state_revision: row.state_revision, outboxed: false, records: Vec::new(),
            });
            return Ok((applied.into_iter().map(|(_, current, _, _)| Transition {
                record: current.expect("unchanged implies existing"),
                changed: false, created: false, ledger_epoch: row.epoch.clone(), state_revision: row.state_revision,
                outboxed: false,
            }).collect(), change));
        }
        let state_revision = row.state_revision.checked_add(1)
            .filter(|revision| *revision <= super::model::MAX_SAFE_SEQUENCE)
            .ok_or_else(|| LedgerError::Storage(anyhow::anyhow!("session stateRevision exhausted")))?;
        upsert_session_revision(&tx, session, &row, state_revision)?;
        // One outbox row per change, all at the shared revision; `seq` keeps
        // them in batch order (a new delivery first).
        let mut seq = 0;
        let mut change = None;
        if let Some(new) = new_delivery {
            if delivery_changed {
                let stored = store_delivery(&tx, session, &row, new, state_revision, now, outbox)?;
                seq += 1;
                change = Some((stored, true));
            } else {
                change = Some((stored_delivery.clone().unwrap_or(Value::Null), false));
            }
        }
        let mut out = Vec::with_capacity(applied.len());
        for (input_id, current, mut record, changed) in applied {
            if !changed {
                out.push(Transition {
                    record: current.expect("unchanged implies existing"),
                    changed: false, created: false, ledger_epoch: row.epoch.clone(), state_revision, outboxed: false,
                });
                continue;
            }
            record.rev = current.as_ref().map_or(0, |record| record.rev) + 1;
            record.state_revision = state_revision;
            record.updated_at = timestamp(now);
            upsert_record(&tx, session, input_id, &record, state_revision, now)?;
            if outbox {
                outbox_put(&tx, session, state_revision, seq, STATE_CHANGED_EVENT,
                    &state_changed_params(session, &row.epoch, state_revision, &record), now)?;
                seq += 1;
            }
            out.push(Transition {
                record, changed: true, created: current.is_none(), ledger_epoch: row.epoch.clone(), state_revision,
                outboxed: outbox,
            });
        }
        let mut refreshed = Vec::new();
        if let Some((delivery, true)) = &change {
            let skip: Vec<&str> = items.iter().map(|(id, _, _)| id.as_str()).collect();
            refreshed = refresh_actions(&tx, disposition.as_ref().expect("write fence"), session, &row.epoch, delivery, state_revision, now, outbox, &skip, seq, true)?;
        }
        commit_timed(tx, outbox)?;
        let change = change.map(|(delivery, changed)| DeliveryChange {
            delivery,
            changed,
            ledger_epoch: row.epoch.clone(),
            state_revision: if changed { state_revision } else { row.state_revision },
            outboxed: outbox && changed,
            records: refreshed,
        });
        Ok((out, change))
    }

    /// Records by id (≤ 100), in request order; unknown ids in `missing`.
    pub fn get(&self, session: &str, input_ids: &[String]) -> Result<GetResult, LedgerError> {
        if input_ids.len() > MAX_GET_IDS {
            return Err(LedgerError::Invalid(format!("at most {MAX_GET_IDS} inputIds")));
        }
        let mut guard = self.lock()?;
        let connection = guard.connection.as_mut().expect("opened under lock");
        let tx = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let (epoch, revision) = self.session_head(&tx, session)?;
        let mut records = Vec::new();
        let mut missing = Vec::new();
        for id in input_ids {
            match read_record(&tx, session, id)? {
                Some(record) => records.push(record),
                None => missing.push(id.clone()),
            }
        }
        drop(tx);
        Ok(GetResult { ledger_epoch: epoch, state_revision: revision, records, missing })
    }

    /// `cutex/input/list`: with `since_revision` the records changed after it
    /// (incremental); without it every non-terminal record (`uncertain`
    /// included: owner decision 9, shown until the owner resends or dismisses
    /// it) plus terminal ones from the last `include_settled_secs`, ordered by
    /// `stateRevision`.
    pub fn list(&self, session: &str, query: ListQuery) -> Result<ListResult, LedgerError> {
        if let Some(revision) = query.since_revision {
            let since = self.since(session, revision, MAX_SINCE_RECORDS)?;
            return Ok(ListResult {
                ledger_epoch: since.ledger_epoch,
                state_revision: since.state_revision,
                records: since.records,
                complete: !since.resync_required,
                resync_required: since.resync_required,
                delivery: since.delivery,
            });
        }
        let cutoff = self.now().timestamp_millis()
            .saturating_sub(i64::try_from(query.include_settled_secs.saturating_mul(1000)).unwrap_or(i64::MAX));
        let mut guard = self.lock()?;
        let connection = guard.connection.as_mut().expect("opened under lock");
        let tx = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let (epoch, revision) = self.session_head(&tx, session)?;
        let delivery = parse_delivery(session_row(&tx, session)?.as_ref())?;
        let mut records = Vec::new();
        {
            let mut open = tx.prepare_cached(
                "SELECT record_json FROM input_ledger WHERE session = ?1 AND terminal = 0
                 UNION ALL
                 SELECT record_json FROM input_ledger WHERE session = ?1 AND terminal = 1 AND updated_at_ms >= ?2",
            )?;
            for row in open.query_map(params![session, cutoff], |row| row.get::<_, String>(0))? {
                records.push(decode_record(&row?)?);
            }
        }
        drop(tx);
        records.sort_by_key(|record| record.state_revision);
        Ok(ListResult { ledger_epoch: epoch, state_revision: revision, records, complete: true, resync_required: false, delivery })
    }

    /// Records whose last change is newer than `revision`.
    pub fn since(&self, session: &str, revision: u64, limit: usize) -> Result<SinceResult, LedgerError> {
        let mut guard = self.lock()?;
        let connection = guard.connection.as_mut().expect("opened under lock");
        let tx = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let row = session_row(&tx, session)?;
        let (epoch, head, floor) = match &row {
            Some(row) => (row.epoch.clone(), row.state_revision, row.compacted_through),
            None => (self.session_head(&tx, session)?.0, 0, 0),
        };
        let delivery = parse_delivery(row.as_ref())?;
        if revision < floor || revision > head {
            drop(tx);
            return Ok(SinceResult { ledger_epoch: epoch, state_revision: head, records: Vec::new(), resync_required: true, delivery });
        }
        let mut records = Vec::new();
        {
            let mut statement = tx.prepare_cached(
                "SELECT record_json FROM input_ledger WHERE session = ?1 AND updated_revision > ?2
                 ORDER BY updated_revision LIMIT ?3",
            )?;
            let take = i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX);
            for row in statement.query_map(params![session, revision as i64, take], |row| row.get::<_, String>(0))? {
                records.push(decode_record(&row?)?);
            }
        }
        drop(tx);
        let resync_required = records.len() > limit;
        if resync_required {
            records.clear();
        }
        Ok(SinceResult { ledger_epoch: epoch, state_revision: head, records, resync_required, delivery })
    }

    /// Deadline scheduler input: unsettled records whose deadline is at or
    /// before `now`, oldest first (index `input_ledger_deadline`).
    pub fn due(&self, now: DateTime<Utc>, limit: usize) -> Result<Vec<DueInput>, LedgerError> {
        let mut guard = self.lock()?;
        let connection = guard.connection.as_mut().expect("opened under lock");
        let mut statement = connection.prepare_cached(
            "SELECT session, input_id, state, rev, deadline_ms FROM input_ledger
             WHERE deadline_ms IS NOT NULL AND deadline_ms <= ?1 ORDER BY deadline_ms LIMIT ?2",
        )?;
        let rows = statement.query_map(
            params![now.timestamp_millis(), i64::try_from(limit).unwrap_or(i64::MAX)],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, i64>(3)?, row.get::<_, i64>(4)?)),
        )?;
        let mut due = Vec::new();
        for row in rows {
            let (session, input_id, state, rev, deadline_ms) = row?;
            let state = InputState::parse(&state)
                .ok_or_else(|| LedgerError::Storage(anyhow::anyhow!("unknown stored state {state}")))?;
            due.push(DueInput { cutex_session_id: session, input_id, state, rev: rev as u64, deadline_ms });
        }
        Ok(due)
    }

    pub(crate) fn absolute_due(&self,now:DateTime<Utc>,limit:usize)->Result<Vec<DueInput>,LedgerError> {
        let mut guard=self.lock()?;let connection=guard.connection.as_mut().expect("opened under lock");
        let mut query=connection.prepare_cached("SELECT session,input_id,state,rev,observation_deadline_ms FROM input_ledger WHERE observation_deadline_ms IS NOT NULL AND observation_deadline_ms<=?1 AND state IN ('received','sending') AND surface IN ('nextUp','pendingNotes') ORDER BY observation_deadline_ms LIMIT ?2")?;
        let rows=query.query_map(params![now.timestamp_millis(),i64::try_from(limit).unwrap_or(i64::MAX)],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,i64>(3)?,row.get::<_,i64>(4)?)))?;
        let mut due=Vec::new();
        for row in rows {let (session,id,state,rev,deadline_ms)=row?;
            let state=InputState::parse(&state).ok_or_else(||LedgerError::Invalid("unknown observed state".into()))?;
            due.push(DueInput{cutex_session_id:session,input_id:id,state,rev:rev as u64,deadline_ms});
        }Ok(due)
    }
    fn unbudgeted(&self,limit:usize)->Result<Vec<InputRecord>,LedgerError> {
        let mut guard=self.lock()?;let connection=guard.connection.as_mut().expect("opened under lock");
        let mut query=connection.prepare_cached("SELECT record_json FROM input_ledger WHERE observation_deadline_ms IS NULL AND state IN ('received','sending') AND surface IN ('nextUp','pendingNotes') ORDER BY updated_revision LIMIT ?1")?;
        let rows=query.query_map([i64::try_from(limit).unwrap_or(i64::MAX)],|row|row.get::<_,String>(0))?;
        let mut records=Vec::new();for row in rows {records.push(decode_record(&row?)?);}Ok(records)
    }

    /// Delete terminal records (never `uncertain`: owner decision 9) last
    /// changed before `now - retention`, at most
    /// `limit` per call; raise each touched session's compaction floor so an
    /// older `sinceRevision` answers `resyncRequired`. Returns rows removed.
    pub fn compact(&self, now: DateTime<Utc>, retention: Duration, limit: usize) -> Result<usize, LedgerError> {
        let cutoff = now.timestamp_millis()
            .saturating_sub(i64::try_from(retention.as_millis()).unwrap_or(i64::MAX));
        let mut guard = self.lock_write()?;
        let LedgerGuard {connection,disposition,..}=&mut guard;
        let connection = connection.as_mut().expect("opened under lock");
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let victims: Vec<(String, String, i64)> = {
            let mut statement = tx.prepare_cached(
                "SELECT session, input_id, updated_revision FROM input_ledger
                 WHERE terminal = 1 AND updated_at_ms < ?1 LIMIT ?2",
            )?;
            let rows = statement.query_map(
                params![cutoff, i64::try_from(limit).unwrap_or(i64::MAX)],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut floors = std::collections::BTreeMap::<String, i64>::new();
        for (session, input_id, revision) in &victims {
            if let Some(record)=read_record(&tx,session,input_id)? {
                if matches!(record.state,InputState::Discarded|InputState::Superseded|InputState::Rejected) {
                    disposition.as_mut().expect("write fence").request(session,input_id,now).map_err(LedgerError::Storage)?;
                }
            }
            tx.execute("DELETE FROM input_ledger WHERE session = ?1 AND input_id = ?2", params![session, input_id])?;
            let floor = floors.entry(session.clone()).or_insert(0);
            *floor = (*floor).max(*revision);
        }
        for (session, floor) in floors {
            tx.execute(
                "UPDATE input_ledger_sessions SET compacted_through = MAX(compacted_through, ?2) WHERE session = ?1",
                params![session, floor],
            )?;
        }
        tx.commit()?;
        Ok(victims.len())
    }

    /// The open owner records' stored `actions` under this build's rules and
    /// the stored delivery: an older build wrote them, and only a delivery
    /// change recomputes them. A difference is published like a delivery
    /// change at one new revision, and keeps `updatedAt`: not a new write, so
    /// the restart reconcile still judges a row on a later page
    /// ([`super::input_ledger_settle::restart_next`]). None (or no delivery
    /// stored yet, its first write recomputes them) writes nothing and uses no
    /// revision.
    pub fn refresh_open_actions(&self, session: &str) -> Result<DeliveryChange, LedgerError> {
        validate_id("cutexSessionId", session)?;
        let now = self.now();
        let outbox = self.outbox_on();
        let mut guard = self.lock_write()?;
        let LedgerGuard {connection,disposition,..}=&mut guard;
        let connection = connection.as_mut().expect("opened under lock");
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let unchanged = |ledger_epoch: String, state_revision: u64, delivery: Value| DeliveryChange {
            delivery, changed: false, ledger_epoch, state_revision, outboxed: false, records: Vec::new(),
        };
        let Some(row) = session_row(&tx, session)? else {
            return Ok(unchanged(file_epoch(&tx)?, 0, Value::Null));
        };
        let Some(delivery) = parse_delivery(Some(&row))? else {
            return Ok(unchanged(row.epoch, row.state_revision, Value::Null));
        };
        let state_revision = row.state_revision.checked_add(1)
            .filter(|revision| *revision <= super::model::MAX_SAFE_SEQUENCE)
            .ok_or_else(|| LedgerError::Storage(anyhow::anyhow!("session stateRevision exhausted")))?;
        let stored = store_delivery(&tx, session, &row, &delivery, state_revision, now, outbox)?;
        let records = refresh_actions(&tx, disposition.as_ref().expect("write fence"), session, &row.epoch, &stored, state_revision, now, outbox, &[], 1, false)?;
        if records.is_empty() {
            // Dropped: the delivery write above rolls back.
            return Ok(unchanged(row.epoch, row.state_revision, delivery));
        }
        commit_timed(tx, outbox)?;
        Ok(DeliveryChange { delivery: stored, changed: true, ledger_epoch: row.epoch, state_revision, outboxed: outbox, records })
    }

    /// Store the session's SessionDelivery (§2.6) and give it the next
    /// `stateRevision`, so `deliveryChanged` and `stateChanged` share one
    /// gapless sequence (§2.8). `delivery` is the §2.6 object; its
    /// `stateRevision` is overwritten. Unchanged (ignoring `stateRevision` /
    /// `observedAt`) uses no revision.
    pub fn delivery_changed(&self, session: &str, delivery: &Value) -> Result<DeliveryChange, LedgerError> {
        validate_id("cutexSessionId", session)?;
        if !delivery.is_object() {
            return Err(LedgerError::Invalid("delivery must be an object".into()));
        }
        let now = self.now();
        let outbox = self.outbox_on();
        // 054 S25 timing (switch on only): the whole write incl. outbox rows.
        let _timing = timing::span(timing::Op::Transition, outbox);
        let lock_wait = timing::span(timing::Op::LockWait, outbox);
        let mut guard = self.lock_write()?;
        drop(lock_wait);
        let LedgerGuard {connection,disposition,..}=&mut guard;
        let connection = connection.as_mut().expect("opened under lock");
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row = match session_row(&tx, session)? {
            Some(row) => row,
            None => SessionRow { state_revision: 0, epoch: file_epoch(&tx)?, compacted_through: 0, delivery: None },
        };
        if let Some(stored) = parse_delivery(Some(&row))? {
            if comparable_delivery(&stored) == comparable_delivery(delivery) {
                drop(tx);
                return Ok(DeliveryChange {
                    delivery: stored, changed: false, ledger_epoch: row.epoch, state_revision: row.state_revision, outboxed: false,
                    records: Vec::new(),
                });
            }
        }
        let state_revision = row.state_revision.checked_add(1)
            .filter(|revision| *revision <= super::model::MAX_SAFE_SEQUENCE)
            .ok_or_else(|| LedgerError::Storage(anyhow::anyhow!("session stateRevision exhausted")))?;
        let stored = store_delivery(&tx, session, &row, delivery, state_revision, now, outbox)?;
        // `interruptSend` depends on the running turn: the open owner
        // records' actions follow the delivery at the same revision.
        let records = refresh_actions(&tx, disposition.as_ref().expect("write fence"), session, &row.epoch, &stored, state_revision, now, outbox, &[], 1, true)?;
        commit_timed(tx, outbox)?;
        Ok(DeliveryChange { delivery: stored, changed: true, ledger_epoch: row.epoch, state_revision, outboxed: outbox, records })
    }

    fn session_head(&self, tx: &rusqlite::Transaction<'_>, session: &str) -> Result<(String, u64), LedgerError> {
        Ok(match session_row(tx, session)? {
            Some(row) => (row.epoch, row.state_revision),
            // A session with no input yet: report the ledger's file epoch so a
            // client never sees the epoch change on the first record.
            None => (file_epoch(tx)?, 0),
        })
    }
}

fn new_epoch() -> String {
    format!("le-{}", uuid::Uuid::new_v4().simple())
}

fn open_database(path: &Path) -> anyhow::Result<Connection> {
    use anyhow::Context;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let db = Connection::open(path).with_context(|| format!("open input ledger: {}", path.display()))?;
    // Ledger writers queue on .write.lock before BEGIN, not by repeatedly
    // racing SQLite's busy handler. Keep this bounded for foreign connections
    // that do not participate in the ledger protocol.
    db.busy_timeout(Duration::from_secs(2))?;
    db.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=FULL;
         CREATE TABLE IF NOT EXISTS input_ledger_meta (
           singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
           format_version INTEGER NOT NULL,
           epoch TEXT NOT NULL,
           created_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS input_ledger_sessions (
           session TEXT PRIMARY KEY,
           state_revision INTEGER NOT NULL,
           epoch TEXT NOT NULL,
           compacted_through INTEGER NOT NULL DEFAULT 0,
           delivery_json TEXT
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS input_ledger (
           session TEXT NOT NULL,
           input_id TEXT NOT NULL,
           rev INTEGER NOT NULL,
           state TEXT NOT NULL,
           surface TEXT NOT NULL,
           terminal INTEGER NOT NULL CHECK (terminal IN (0, 1)),
           updated_revision INTEGER NOT NULL,
           updated_at_ms INTEGER NOT NULL,
           deadline_ms INTEGER,
           record_json TEXT NOT NULL,
           PRIMARY KEY (session, input_id)
         ) WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS input_ledger_by_revision ON input_ledger(session, updated_revision);
         CREATE INDEX IF NOT EXISTS input_ledger_open ON input_ledger(session, terminal, updated_at_ms);
         CREATE INDEX IF NOT EXISTS input_ledger_deadline ON input_ledger(deadline_ms) WHERE deadline_ms IS NOT NULL;
         CREATE TABLE IF NOT EXISTS input_ledger_losses (
           id INTEGER PRIMARY KEY,
           previous_epoch TEXT NOT NULL,
           epoch TEXT NOT NULL,
           detected_at TEXT NOT NULL,
           handled INTEGER NOT NULL DEFAULT 0,
           UNIQUE(previous_epoch, epoch)
         );
         CREATE TABLE IF NOT EXISTS input_ledger_payloads (
           session TEXT NOT NULL,
           input_id TEXT NOT NULL,
           payload_json TEXT NOT NULL,
           created_at_ms INTEGER NOT NULL,
           PRIMARY KEY (session, input_id)
         );
         CREATE TABLE IF NOT EXISTS input_ledger_outbox (
           session TEXT NOT NULL,
           state_revision INTEGER NOT NULL,
           seq INTEGER NOT NULL,
           method TEXT NOT NULL,
           params_json TEXT NOT NULL,
           attempts INTEGER NOT NULL DEFAULT 0,
           created_at_ms INTEGER NOT NULL,
           PRIMARY KEY (session, state_revision, seq)
         ) WITHOUT ROWID;",
    )?;
    let has_observation={
        let mut columns=db.prepare("PRAGMA table_info(input_ledger)")?;
        let names=columns.query_map([],|row|row.get::<_,String>(1))?.collect::<rusqlite::Result<Vec<_>>>()?;
        names.iter().any(|name|name=="observation_deadline_ms")
    };
    if !has_observation {db.execute_batch("ALTER TABLE input_ledger ADD COLUMN observation_deadline_ms INTEGER;")?;}
    db.execute_batch("DROP INDEX IF EXISTS input_ledger_observation; CREATE INDEX IF NOT EXISTS input_ledger_transport_observation ON input_ledger(observation_deadline_ms) WHERE observation_deadline_ms IS NOT NULL AND state IN ('received','sending');")?;
    let version: Option<i64> = db
        .query_row("SELECT format_version FROM input_ledger_meta WHERE singleton = 1", [], |row| row.get(0))
        .optional()?;
    match version {
        Some(FORMAT_VERSION) => {}
        Some(other) => anyhow::bail!("unsupported input ledger format version {other}"),
        None => {
            db.execute(
                "INSERT INTO input_ledger_meta (singleton, format_version, epoch, created_at) VALUES (1, ?1, ?2, ?3)",
                params![FORMAT_VERSION, new_epoch(), timestamp(Utc::now())],
            )?;
        }
    }
    Ok(db)
}

/// Side file next to the ledger naming the epoch it last held. A ledger that
/// is gone or carries another epoch while the side file exists was lost or
/// replaced (§2.12): the open records a loss for the settle code (C5) to
/// rebuild from the underlying stores and announce (`cutex/stream/reset`).
fn epoch_marker(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|name| name.to_os_string()).unwrap_or_default();
    name.push(".epoch");
    path.with_file_name(name)
}

fn looks_corrupt(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<rusqlite::Error>().is_some_and(|error| matches!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt)
        ))
    })
}

/// [`open_database`], plus: a corrupt file is moved aside (`.corrupt-<ms>`)
/// and a new one created; a loss (missing / corrupt / other epoch while the
/// marker exists) is recorded in `input_ledger_losses`; the marker is updated.
fn open_watched(path: &Path) -> anyhow::Result<Connection> {
    let marker = epoch_marker(path);
    let side_files = |path: &Path| ["-wal", "-shm"].map(|suffix| {
        let mut side = path.as_os_str().to_os_string();
        side.push(suffix);
        PathBuf::from(side)
    });
    if !path.exists() {
        // A WAL left by the lost file must not be replayed into the new one.
        for side in side_files(path) {
            let _ = std::fs::remove_file(side);
        }
    }
    let opened = match open_database(path) {
        Err(error) if looks_corrupt(&error) => {
            let aside = path.with_extension(format!("sqlite3.corrupt-{}", Utc::now().timestamp_millis()));
            eprintln!("cutex input ledger: {} is corrupt ({error:#}); moved to {}", path.display(), aside.display());
            std::fs::rename(path, &aside)?;
            for side in side_files(path) {
                let _ = std::fs::remove_file(side);
            }
            open_database(path)?
        }
        other => other?,
    };
    let epoch = file_epoch(&opened).map_err(|error| anyhow::anyhow!("{error}"))?;
    let previous = std::fs::read_to_string(&marker).ok().map(|text| text.trim().to_string()).filter(|text| !text.is_empty());
    if let Some(previous) = previous.filter(|previous| *previous != epoch) {
        opened.execute(
            "INSERT OR IGNORE INTO input_ledger_losses (previous_epoch, epoch, detected_at, handled) VALUES (?1, ?2, ?3, 0)",
            params![previous, epoch, timestamp(Utc::now())],
        )?;
    }
    if std::fs::read_to_string(&marker).ok().as_deref().map(str::trim) != Some(epoch.as_str()) {
        std::fs::write(&marker, &epoch)?;
    }
    Ok(opened)
}

/// A recorded ledger loss (§2.12) not yet handled by the settle code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerLoss {
    pub id: i64,
    pub previous_epoch: String,
    pub epoch: String,
}

impl InputLedger {
    /// The file epoch (opens the ledger, which also records a loss).
    pub fn epoch(&self) -> Result<String, LedgerError> {
        let mut guard = self.lock()?;
        let connection = guard.connection.as_mut().expect("opened under lock");
        file_epoch(connection)
    }

    /// Losses not yet rebuilt and announced.
    pub fn pending_losses(&self) -> Result<Vec<LedgerLoss>, LedgerError> {
        let mut guard = self.lock()?;
        let connection = guard.connection.as_mut().expect("opened under lock");
        let mut statement = connection.prepare_cached(
            "SELECT id, previous_epoch, epoch FROM input_ledger_losses WHERE handled = 0 ORDER BY id",
        )?;
        let rows = statement.query_map([], |row| Ok(LedgerLoss { id: row.get(0)?, previous_epoch: row.get(1)?, epoch: row.get(2)? }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn loss_handled(&self, id: i64) -> Result<(), LedgerError> {
        let mut guard = self.lock_write()?;
        let connection = guard.connection.as_mut().expect("opened under lock");
        connection.execute("UPDATE input_ledger_losses SET handled = 1 WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Unsettled records (not terminal and not `uncertain`) of every
    /// session after `after` (`(session, inputId)`, exclusive) in key order,
    /// at most `limit` (primary-key range scan: the reconcile / drift passes
    /// page through with a cursor, never the whole table at once).
    pub fn unsettled_page(&self, after: Option<&(String, String)>, limit: usize) -> Result<Vec<InputRecord>, LedgerError> {
        let mut guard = self.lock()?;
        let connection = guard.connection.as_mut().expect("opened under lock");
        let (session, input) = after.cloned().unwrap_or_default();
        let mut statement = connection.prepare_cached(
            "SELECT record_json FROM input_ledger WHERE (session > ?1 OR (session = ?1 AND input_id > ?2))
               AND terminal = 0 AND state <> 'uncertain' ORDER BY session, input_id LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![session, input, i64::try_from(limit).unwrap_or(i64::MAX)],
            |row| row.get::<_, String>(0),
        )?;
        let mut records = Vec::new();
        for row in rows {
            records.push(decode_record(&row?)?);
        }
        Ok(records)
    }

    /// `inContext` timeline records of every session after `after`
    /// (`(session, inputId)`, exclusive) in key order, at most `limit`
    /// (primary-key range scan, like [`Self::unsettled_page`]). The caller
    /// filters the unbound ones (`timelineItemId` null); the cursor is the
    /// last row returned.
    pub fn in_context_page(&self, after: Option<&(String, String)>, limit: usize) -> Result<Vec<InputRecord>, LedgerError> {
        let mut guard = self.lock()?;
        let connection = guard.connection.as_mut().expect("opened under lock");
        let (session, input) = after.cloned().unwrap_or_default();
        let mut statement = connection.prepare_cached(
            "SELECT record_json FROM input_ledger WHERE (session > ?1 OR (session = ?1 AND input_id > ?2))
               AND state = 'inContext' AND surface = 'timeline' ORDER BY session, input_id LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![session, input, i64::try_from(limit).unwrap_or(i64::MAX)],
            |row| row.get::<_, String>(0),
        )?;
        let mut records = Vec::new();
        for row in rows {
            records.push(decode_record(&row?)?);
        }
        Ok(records)
    }

    /// Every record of `session` bound to one of `turn_ids` (`turnId`
    /// exactly equal), in key order, any state (r274.2 §9.6: the records a
    /// history revert retracts). A scan of the session's key range.
    pub fn turn_records(&self, session: &str, turn_ids: &[String]) -> Result<Vec<InputRecord>, LedgerError> {
        let mut guard = self.lock()?;
        let connection = guard.connection.as_mut().expect("opened under lock");
        let mut statement = connection.prepare_cached(
            "SELECT record_json FROM input_ledger WHERE session = ?1 AND json_extract(record_json, '$.turnId') = ?2
               ORDER BY input_id",
        )?;
        let mut records = Vec::new();
        for turn in turn_ids.iter().filter(|turn| !turn.is_empty()) {
            for row in statement.query_map(params![session, turn], |row| row.get::<_, String>(0))? {
                let record = decode_record(&row?)?;
                if !records.iter().any(|known: &InputRecord| known.input_id == record.input_id) {
                    records.push(record);
                }
            }
        }
        records.sort_by(|a, b| a.input_id.cmp(&b.input_id));
        Ok(records)
    }

    /// Keep what `cutex/input/retry` needs to send an input again (the
    /// owner's validated submit params). Replaced on a repeat.
    pub fn put_payload(&self, session: &str, input_id: &str, payload: &Value) -> Result<(), LedgerError> {
        validate_id("cutexSessionId", session)?;
        validate_id("inputId", input_id)?;
        let now = self.now();
        let mut guard = self.lock_write()?;
        let connection = guard.connection.as_mut().expect("opened under lock");
        connection.execute(
            "INSERT INTO input_ledger_payloads (session, input_id, payload_json, created_at_ms) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(session, input_id) DO UPDATE SET payload_json = excluded.payload_json, created_at_ms = excluded.created_at_ms",
            params![session, input_id, serde_json::to_string(payload)?, now.timestamp_millis()],
        )?;
        Ok(())
    }

    pub fn payload(&self, session: &str, input_id: &str) -> Result<Option<Value>, LedgerError> {
        let mut guard = self.lock()?;
        let connection = guard.connection.as_mut().expect("opened under lock");
        let text: Option<String> = connection
            .prepare_cached("SELECT payload_json FROM input_ledger_payloads WHERE session = ?1 AND input_id = ?2")?
            .query_row(params![session, input_id], |row| row.get(0))
            .optional()?;
        Ok(match text {
            Some(text) => Some(serde_json::from_str(&text)?),
            None => None,
        })
    }

    /// Drop retry payloads older than `retention`, at most `limit`.
    pub fn compact_payloads(&self, now: DateTime<Utc>, retention: Duration, limit: usize) -> Result<usize, LedgerError> {
        let cutoff = now.timestamp_millis().saturating_sub(i64::try_from(retention.as_millis()).unwrap_or(i64::MAX));
        let mut guard = self.lock_write()?;
        let connection = guard.connection.as_mut().expect("opened under lock");
        Ok(connection.execute(
            // A record that can still go out keeps its text, however old:
            // not settled, or `uncertain` / `failed` with a retry on offer.
            "DELETE FROM input_ledger_payloads WHERE rowid IN
               (SELECT p.rowid FROM input_ledger_payloads p WHERE p.created_at_ms < ?1 AND NOT EXISTS
                  (SELECT 1 FROM input_ledger r WHERE r.session = p.session AND r.input_id = p.input_id
                     AND (r.terminal = 0 OR r.state = 'failed'))
                LIMIT ?2)",
            params![cutoff, i64::try_from(limit).unwrap_or(i64::MAX)],
        )?)
    }
}

fn parse_delivery(row: Option<&SessionRow>) -> Result<Option<Value>, LedgerError> {
    Ok(match row.and_then(|row| row.delivery.as_deref()) {
        Some(text) => Some(serde_json::from_str(text)?),
        None => None,
    })
}

/// A delivery without its `stateRevision` / `observedAt` (what "changed" means).
fn comparable_delivery(value: &Value) -> Value {
    let mut value = value.clone();
    if let Some(object) = value.as_object_mut() {
        object.remove("stateRevision");
        object.remove("observedAt");
    }
    value
}

/// Store `delivery` as the session's at `state_revision` (outbox seq 0).
fn store_delivery(
    tx: &rusqlite::Transaction<'_>,
    session: &str,
    row: &SessionRow,
    delivery: &Value,
    state_revision: u64,
    now: DateTime<Utc>,
    outbox: bool,
) -> Result<Value, LedgerError> {
    let mut stored = delivery.clone();
    stored["stateRevision"] = json!(state_revision);
    tx.execute(
        "INSERT INTO input_ledger_sessions (session, state_revision, epoch, compacted_through, delivery_json)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(session) DO UPDATE SET state_revision = excluded.state_revision, delivery_json = excluded.delivery_json",
        params![session, state_revision as i64, row.epoch, row.compacted_through as i64, serde_json::to_string(&stored)?],
    )?;
    if outbox {
        outbox_put(tx, session, state_revision, 0, DELIVERY_CHANGED_EVENT,
            &delivery_changed_params(session, &row.epoch, state_revision, &stored), now)?;
    }
    Ok(stored)
}

/// Bring the `actions` of the session's open owner records (`accepted`,
/// `held`, `waiting`: the only ones `interruptSend` can apply to) in line
/// with `delivery`; each one that changes takes `state_revision` (outbox
/// seq from `seq`). `skip`: records the caller writes in this transaction.
/// One indexed read of the session's open rows, only on a delivery change
/// or at a process start ([`InputLedger::refresh_open_actions`]).
#[allow(clippy::too_many_arguments)]
fn refresh_actions(
    tx: &rusqlite::Transaction<'_>,
    disposition: &super::input_disposal::Guard,
    session: &str,
    epoch: &str,
    delivery: &Value,
    state_revision: u64,
    now: DateTime<Utc>,
    outbox: bool,
    skip: &[&str],
    mut seq: u64,
    touch: bool,
) -> Result<Vec<InputRecord>, LedgerError> {
    let rows: Vec<String> = {
        let mut statement = tx.prepare_cached(
            "SELECT record_json FROM input_ledger WHERE session = ?1 AND terminal = 0 AND state IN ('accepted', 'held', 'waiting')",
        )?;
        let rows = statement.query_map([session], |row| row.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut out = Vec::new();
    for json in rows {
        let mut record = decode_record(&json)?;
        if record.kind != InputKind::Owner || skip.contains(&record.input_id.as_str()) {
            continue;
        }
        let before=record.clone();
        if let Some(decision)=disposition.get(session,&record.input_id).map_err(LedgerError::Storage)? {
            record=super::input_disposal::suppress(record,&decision);
        }
        let actions = record_actions(&record, Some(delivery));
        if actions == record.actions && !observably_changed(&before,&record) {
            continue;
        }
        record.actions = actions;
        record.rev += 1;
        record.state_revision = state_revision;
        if touch {
            record.updated_at = timestamp(now);
        }
        let input_id = record.input_id.clone();
        upsert_record(tx, session, &input_id, &record, state_revision, now)?;
        if outbox {
            outbox_put(tx, session, state_revision, seq, STATE_CHANGED_EVENT,
                &state_changed_params(session, epoch, state_revision, &record), now)?;
            seq += 1;
        }
        out.push(record);
    }
    Ok(out)
}

fn file_epoch(connection: &Connection) -> Result<String, LedgerError> {
    Ok(connection.query_row("SELECT epoch FROM input_ledger_meta WHERE singleton = 1", [], |row| row.get(0))?)
}

fn session_row(connection: &Connection, session: &str) -> Result<Option<SessionRow>, LedgerError> {
    let row = connection
        .prepare_cached("SELECT state_revision, epoch, compacted_through, delivery_json FROM input_ledger_sessions WHERE session = ?1")?
        .query_row([session], |row| {
            Ok(SessionRow {
                state_revision: row.get::<_, i64>(0)? as u64,
                epoch: row.get(1)?,
                compacted_through: row.get::<_, i64>(2)? as u64,
                delivery: row.get(3)?,
            })
        })
        .optional()?;
    Ok(row)
}

// 057 controls are derived, not durable enum values: exact r275 must be
// able to decode every record written here. Non-owner actions need no delivery.
fn decode_record(json: &str) -> serde_json::Result<InputRecord> {
    let mut record: InputRecord = serde_json::from_str(json)?;
    if record.kind != InputKind::Owner {
        record.actions = record_actions(&record, None);
    }
    Ok(record)
}

fn stored_record_json(record: &InputRecord) -> serde_json::Result<String> {
    if record.actions.iter().all(|action| !matches!(action, Action::Hold | Action::ReleaseHold)) {
        return serde_json::to_string(record);
    }
    let mut stored = record.clone();
    stored.actions.retain(|action| !matches!(action, Action::Hold | Action::ReleaseHold));
    serde_json::to_string(&stored)
}

fn read_record(connection: &Connection, session: &str, input_id: &str) -> Result<Option<InputRecord>, LedgerError> {
    let json: Option<String> = connection
        .prepare_cached("SELECT record_json FROM input_ledger WHERE session = ?1 AND input_id = ?2")?
        .query_row(params![session, input_id], |row| row.get(0))
        .optional()?;
    Ok(match json {
        Some(json) => Some(decode_record(&json)?),
        None => None,
    })
}

// ---------------------------------------------------------------------------
// Deadline scheduler (skeleton; the settle code in C5 owns the loop)

/// One scheduler pass: read due records (no lock held while `decide` runs),
/// then apply each decision with a CAS on the observed `rev`. A record that
/// moved meanwhile is skipped (`Conflict`), never overwritten. Returns how many
/// transitions were applied.
pub fn run_deadline_pass(
    ledger: &InputLedger,
    now: DateTime<Utc>,
    limit: usize,
    mut decide: impl FnMut(&DueInput) -> Option<Next>,
    mut applied: impl FnMut(&Transition),
) -> Result<usize, LedgerError> {
    let mut count = 0;
    for due in ledger.due(now, limit)? {
        // Expiry must not wait for a runtime check that can hang.
        let observed = ledger.get(&due.cutex_session_id, &[due.input_id.clone()])?.records.pop();
        let expired = observed.as_ref().filter(|record|matches!(record.state,InputState::Received|InputState::Sending)).and_then(|record| record.details["observationDeadline"].as_str())
            .and_then(|text| DateTime::parse_from_rfc3339(text).ok()).is_some_and(|at| at <= now);
        let legacy_cap=observed.as_ref().filter(|record|
            matches!(record.state,InputState::Held|InputState::Accepted)
                && record.deadline.is_some()
                && record.details["observationDeadline"].as_str()==record.deadline.as_deref());
        let next = if let Some(record)=legacy_cap {
            // Do not reinterpret the previous build's artificial cap as a
            // source receipt deadline (which could query a missing runtime
            // and invent uncertainty on upgrade). Retire metadata only.
            let mut details=record.details.clone();
            if let Some(object)=details.as_object_mut(){object.remove("observationDeadline");}
            let mut next=Next::to(record.state).details(details).deadline(None);
            next.reason=record.reason.clone();next.stage=record.stage;next
        } else if expired {
            Next::uncertain("outcomeUnknown").details(json!({"absoluteDeadlineElapsed":true}))
        } else {
            let Some(next) = decide(&due) else { continue }; next
        };
        match ledger.transition(&due.cutex_session_id, &due.input_id, Expect::rev(due.rev), next) {
            Ok(transition) => {
                if transition.changed {
                    count += 1;
                    applied(&transition);
                }
            }
            Err(LedgerError::Conflict { .. }) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(count)
}

/// Time of the next deadline (`None`: nothing pending). The loop sleeps until
/// then instead of polling (051 B8).
/// Pure storage/clock pass, run on its own thread independently of native IO.
pub(crate) fn expire_observations(ledger:&InputLedger,now:DateTime<Utc>,limit:usize)->Result<usize,LedgerError> {
    for record in ledger.unbudgeted(limit)? {
        let mut next=Next::to(record.state).details(record.details.clone());next.reason=record.reason.clone();next.stage=record.stage;
        let _=ledger.transition(&record.cutex_session_id,&record.input_id,Expect::rev(record.rev),next)?;
    }
    let mut count=0;
    for due in ledger.absolute_due(now,limit)? {
        match ledger.transition(&due.cutex_session_id,&due.input_id,Expect::rev(due.rev),Next::uncertain("outcomeUnknown").details(json!({"absoluteDeadlineElapsed":true}))) {
            Ok(done)=>{if done.changed {count+=1;let _=publish(&done);}},
            Err(LedgerError::Conflict{..})=>{},Err(error)=>return Err(error),
        }
    }Ok(count)
}

pub fn next_deadline(ledger: &InputLedger) -> Result<Option<DateTime<Utc>>, LedgerError> {
    let mut guard = ledger.lock()?;
    let connection = guard.connection.as_mut().expect("opened under lock");
    let at: Option<i64> = connection
        .query_row("SELECT MIN(deadline_ms) FROM input_ledger WHERE deadline_ms IS NOT NULL", [], |row| row.get(0))?;
    Ok(at.and_then(|ms| Utc.timestamp_millis_opt(ms).single()))
}

// ---------------------------------------------------------------------------
// Events (published by the writers after the ledger lock is released)

/// `cutex/input/stateChanged` for the session stream.
pub fn state_changed_event(transition: &Transition) -> Value {
    json!({
        "method": STATE_CHANGED_EVENT,
        "params": state_changed_params(&transition.record.cutex_session_id, &transition.ledger_epoch, transition.state_revision, &transition.record),
    })
}

fn state_changed_params(session: &str, epoch: &str, state_revision: u64, record: &InputRecord) -> Value {
    json!({"cutexSessionId": session, "ledgerEpoch": epoch, "stateRevision": state_revision, "record": record})
}

/// `cutex/session/deliveryChanged` for the session stream.
pub fn delivery_changed_event(session: &str, change: &DeliveryChange) -> Value {
    json!({
        "method": DELIVERY_CHANGED_EVENT,
        "params": delivery_changed_params(session, &change.ledger_epoch, change.state_revision, &change.delivery),
    })
}

fn delivery_changed_params(session: &str, epoch: &str, state_revision: u64, delivery: &Value) -> Value {
    json!({"cutexSessionId": session, "ledgerEpoch": epoch, "stateRevision": state_revision, "delivery": delivery})
}

/// Host-stream projection of a `stateChanged` event: the record without
/// `preview` and `actions` (§2.13). Only ONE durable event is written per
/// change (the session-stream one, [`publish`]); the host stream applies this
/// projection when it serves it, so host subscribers never see a change twice.
pub fn host_state_changed_event(transition: &Transition) -> Value {
    let mut event = state_changed_event(transition);
    if let Some(record) = event.pointer_mut("/params/record").and_then(Value::as_object_mut) {
        record.remove("preview");
        record.remove("actions");
    }
    event
}

/// Project a durable `stateChanged` event for the host stream (the same
/// projection as [`host_state_changed_event`], on the stored params).
pub fn host_projection(method: &str, params: &mut Value) {
    if method == STATE_CHANGED_EVENT {
        if let Some(record) = params.get_mut("record").and_then(Value::as_object_mut) {
            record.remove("preview");
            record.remove("actions");
        }
    }
}

#[cfg(test)]
thread_local! {
    static CAPTURED_EVENTS: std::cell::RefCell<Option<Vec<(String, String, Value)>>> = const { std::cell::RefCell::new(None) };
}

/// Tests: capture published events on this thread instead of appending them.
#[cfg(test)]
pub(crate) fn capture_events<T>(run: impl FnOnce() -> T) -> (T, Vec<(String, String, Value)>) {
    CAPTURED_EVENTS.with(|cell| *cell.borrow_mut() = Some(Vec::new()));
    let result = run();
    let events = CAPTURED_EVENTS.with(|cell| cell.borrow_mut().take().unwrap_or_default());
    (result, events)
}

/// Append directly: a ledger without an outbox (switch off, a test ledger),
/// or a test capturing its events on this thread.
fn append(session: &str, event: Value) -> anyhow::Result<()> {
    let method = event["method"].as_str().unwrap_or_default().to_string();
    let params = event["params"].clone();
    #[cfg(test)]
    {
        let captured = CAPTURED_EVENTS.with(|cell| {
            cell.borrow_mut().as_mut().map(|events| events.push((session.to_string(), method.clone(), params.clone())))
        });
        if captured.is_some() {
            return Ok(());
        }
    }
    append_now(session, method, params)
}

// ---------------------------------------------------------------------------
// Outbox (054 §2.8: a session's events reach its stream in `stateRevision`
// order, with no holes)
//
// Several processes write one ledger file (management, the agent bus server,
// CLIs), each assigning revisions in its own transactions. Publishing after
// the commit from each writer let a writer of one process overtake a lower
// revision of another (integration runs: the bus process's asks writer
// published revision 3 before management published 2). So the event is
// written to `input_ledger_outbox` IN THE SAME TRANSACTION that assigns its
// revision: a committed revision always has its event row, the outbox has no
// holes. Publishing is a drain:
//
// - One drainer at a time across processes: a writer, after its commit,
//   TRIES (never waits; 051, it may hold a session lane) the in-process
//   publisher mutex and then the publish lock file next to the ledger
//   (`<ledger>.publish.lock`, fs2 `LockFileEx` / `flock`, released by the OS
//   when its process dies). A writer that fails either returns at once.
// - The holder reads rows in (session, state_revision, seq) order in a short
//   read, appends them to the event journal with NO ledger lock or SQLite
//   transaction held, deletes the appended rows in a short write
//   transaction, releases the publish lock, then checks the outbox again and
//   repeats while it is not empty. A writer that failed the file try-lock
//   had committed its row before trying, so the holder's re-check after the
//   release sees it (no lost wakeup); within the process, the `rerun` flag
//   covers a writer that found the mutex busy.
// - A sweeper thread per process (`cutex-input-publish`, every
//   [`SWEEP_EVERY`] while the switch is on and the outbox holds rows) drains
//   rows a crashed holder left behind.
//
// Crash window: append-then-delete, so a holder that dies after appending
// some rows of a round and before its delete commits leaves those rows; the
// next drainer appends them again. The stream then carries those events
// twice (same session, same `stateRevision`, same params; the journal gives
// each a new eventId and has no producer-side dedupe key). Clients drop them
// by the §2.8 rule (`record.rev` / `stateRevision` not above local). The
// window is one round (at most [`DRAIN_BATCH`] rows) between the first
// append and the delete commit; nothing is lost and nothing is reordered.
//
// A row whose append fails stays at the head (later rows of the round are
// not appended, so order holds); the sweeper retries it, and after
// [`MAX_APPEND_ATTEMPTS`] failed rounds it is dropped and logged (a hole the
// client fills with `input/list{sinceRevision}`).
//
// A lost ledger file (§2.12) loses its unpublished rows with it: those
// revisions of the old epoch never reach the stream. The new file starts a
// new epoch with an empty outbox; the settle code rebuilds and sends
// `cutex/stream/reset`, and clients read `cutex/input/list` in full on the
// epoch change anyway.

/// Rows one drain round reads (and at most appends twice after a crash).
const DRAIN_BATCH: usize = 64;
/// Failed append rounds after which a row is dropped (logged).
const MAX_APPEND_ATTEMPTS: i64 = 5;
/// Backstop sweep interval.
const SWEEP_EVERY: Duration = Duration::from_secs(1);

fn outbox_put(
    tx: &rusqlite::Transaction<'_>,
    session: &str,
    state_revision: u64,
    seq: u64,
    method: &str,
    params: &Value,
    now: DateTime<Utc>,
) -> Result<(), LedgerError> {
    tx.prepare_cached(
        "INSERT INTO input_ledger_outbox (session, state_revision, seq, method, params_json, created_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?
    .execute(params![session, state_revision as i64, seq as i64, method, serde_json::to_string(params)?, now.timestamp_millis()])?;
    Ok(())
}

struct OutboxRow {
    session: String,
    state_revision: i64,
    seq: i64,
    method: String,
    params_json: String,
    attempts: i64,
}

/// Releases the publish lock file when a drain ends or unwinds.
struct PublishLockGuard<'a>(&'a std::fs::File);

impl Drop for PublishLockGuard<'_> {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(self.0);
    }
}

impl InputLedger {
    fn publish_lock_path(&self) -> PathBuf {
        let mut name = self.path.file_name().map(|name| name.to_os_string()).unwrap_or_default();
        name.push(".publish.lock");
        self.path.with_file_name(name)
    }

    /// Whether the outbox holds rows (one probe).
    pub(crate) fn outbox_pending(&self) -> Result<bool, LedgerError> {
        let mut guard = self.lock()?;
        let connection = guard.connection.as_mut().expect("opened under lock");
        Ok(connection.query_row("SELECT EXISTS(SELECT 1 FROM input_ledger_outbox)", [], |row| row.get(0))?)
    }

    fn outbox_head(&self) -> Result<Vec<OutboxRow>, LedgerError> {
        let mut guard = self.lock()?;
        let connection = guard.connection.as_mut().expect("opened under lock");
        let mut statement = connection.prepare_cached(
            "SELECT session, state_revision, seq, method, params_json, attempts FROM input_ledger_outbox
             ORDER BY session, state_revision, seq LIMIT ?1",
        )?;
        let rows = statement.query_map([DRAIN_BATCH as i64], |row| Ok(OutboxRow {
            session: row.get(0)?,
            state_revision: row.get(1)?,
            seq: row.get(2)?,
            method: row.get(3)?,
            params_json: row.get(4)?,
            attempts: row.get(5)?,
        }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Delete the appended rows; count a failed append on `failed` (dropped
    /// after [`MAX_APPEND_ATTEMPTS`]).
    fn outbox_done(&self, appended: &[&OutboxRow], failed: Option<&OutboxRow>) -> Result<(), LedgerError> {
        let mut guard = self.lock_write()?;
        let connection = guard.connection.as_mut().expect("opened under lock");
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let mut delete = tx.prepare_cached(
                "DELETE FROM input_ledger_outbox WHERE session = ?1 AND state_revision = ?2 AND seq = ?3",
            )?;
            for row in appended {
                delete.execute(params![row.session, row.state_revision, row.seq])?;
            }
            if let Some(row) = failed {
                if row.attempts + 1 >= MAX_APPEND_ATTEMPTS {
                    eprintln!(
                        "cutex input ledger: dropping {} {}@{} after {MAX_APPEND_ATTEMPTS} failed appends",
                        row.method, row.session, row.state_revision
                    );
                    delete.execute(params![row.session, row.state_revision, row.seq])?;
                } else {
                    tx.execute(
                        "UPDATE input_ledger_outbox SET attempts = attempts + 1
                         WHERE session = ?1 AND state_revision = ?2 AND seq = ?3",
                        params![row.session, row.state_revision, row.seq],
                    )?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// One hold of the publish lock: rounds until the outbox is empty or an
    /// append fails. Appends run with no ledger lock held.
    fn drain_rounds(&self) -> anyhow::Result<usize> {
        let mut published = 0;
        loop {
            let rows = self.outbox_head()?;
            if rows.is_empty() {
                return Ok(published);
            }
            let mut appended = Vec::with_capacity(rows.len());
            let mut failed = None;
            for row in &rows {
                let result = serde_json::from_str::<Value>(&row.params_json)
                    .map_err(anyhow::Error::from)
                    .and_then(|params| (self.sink)(&row.session, row.method.clone(), params));
                match result {
                    Ok(()) => appended.push(row),
                    Err(error) => {
                        failed = Some((row, error));
                        break;
                    }
                }
            }
            published += appended.len();
            self.outbox_done(&appended, failed.as_ref().map(|(row, _)| *row))?;
            if let Some((row, error)) = failed {
                return Err(error.context(format!("publish {} {}@{}", row.method, row.session, row.state_revision)));
            }
        }
    }

    /// Publish what the outbox holds, unless another publisher (of any
    /// process) is at it. Never waits for one. Returns how many events this
    /// call appended.
    pub(crate) fn drain_outbox(&self) -> anyhow::Result<usize> {
        use std::sync::atomic::Ordering::SeqCst;
        assert_ledger_lock_not_held("the input ledger publish lock");
        let mut published = 0;
        loop {
            // Announce first, then try: a holder that releases after this
            // store sees it and drains again.
            self.rerun.store(true, SeqCst);
            let mut holder = match self.publisher.try_lock() {
                Ok(holder) => holder,
                Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => return Ok(published),
            };
            self.rerun.store(false, SeqCst);
            if holder.is_none() {
                let path = self.publish_lock_path();
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let file = std::fs::OpenOptions::new()
                    .create(true).truncate(false).read(true).write(true)
                    .open(path)?;
                *holder = Some(file);
            }
            let file = holder.as_ref().expect("opened above");
            let outcome = match fs2::FileExt::try_lock_exclusive(file) {
                Ok(()) => {
                    let _locked = PublishLockGuard(file);
                    Some(self.drain_rounds())
                }
                // Another process drains; it checks again after releasing.
                Err(error) if crate::platform::file_lock::lock_contended(&error) => None,
                Err(error) => Some(Err(error.into())),
            };
            drop(holder);
            match outcome {
                Some(Ok(count)) => published += count,
                // Left for the sweeper.
                Some(Err(error)) => return Err(error),
                None if self.rerun.load(SeqCst) => continue,
                None => return Ok(published),
            }
            // Released: rows committed by a publisher that failed its try
            // meanwhile are ours to drain.
            if !self.rerun.load(SeqCst) && !self.outbox_pending()? {
                return Ok(published);
            }
        }
    }

    /// Drain now (the writers' trigger after a commit) and make sure this
    /// process's backstop sweeper runs.
    fn publish_outbox(&'static self) -> anyhow::Result<()> {
        self.ensure_sweeper();
        self.drain_outbox().map(|_| ())
    }

    /// The `cutex-input-publish` backstop: every [`SWEEP_EVERY`], while the
    /// outbox is on and holds rows (left by a crashed holder, or by an
    /// append that failed), drain it.
    pub(crate) fn ensure_sweeper(&'static self) {
        self.sweeper.get_or_init(|| {
            let spawned = std::thread::Builder::new().name("cutex-input-publish".into()).spawn(move || loop {
                std::thread::sleep(SWEEP_EVERY);
                if !self.outbox_on() || !self.outbox_pending().unwrap_or(false) {
                    continue;
                }
                if let Err(error) = self.drain_outbox() {
                    eprintln!("cutex input ledger: publish sweep: {error:#}");
                }
            });
            if let Err(error) = spawned {
                eprintln!("cutex input ledger: publish sweeper not started: {error}");
            }
        });
    }
}

fn append_now(session: &str, method: String, params: Value) -> anyhow::Result<()> {
    super::repository::management_v2_repository()?.append(super::model::PendingEvent {
        cutex_session_id: session.to_string(),
        host_id: crate::platform::host::current_host_name(),
        source: super::model::EventSource::Cutex,
        schema: None,
        correlation: Default::default(),
        native: None,
        cutex: Some(super::model::CutexMessage { method, params }),
    })?;
    Ok(())
}

/// Drain the process ledger's outbox (the event is already in it).
fn publish_outboxed() -> anyhow::Result<()> {
    let _timing = timing::span(timing::Op::Publish, true);
    default_ledger()?.publish_outbox()
}

/// Publish the one durable `cutex/input/stateChanged` of a changed
/// transition (no-op when `changed` is false). With the switch on it was
/// written to the outbox with the transition, so this drains the outbox (or
/// leaves it to whoever drains already). Call after the ledger lock is
/// released; never while holding the host fence.
pub fn publish(transition: &Transition) -> anyhow::Result<()> {
    if !transition.changed {
        return Ok(());
    }
    if transition.outboxed {
        return publish_outboxed();
    }
    append(&transition.record.cutex_session_id, state_changed_event(transition))
}

/// Publish `cutex/session/deliveryChanged` (no-op when unchanged).
pub fn publish_delivery(session: &str, change: &DeliveryChange) -> anyhow::Result<()> {
    if !change.changed {
        return Ok(());
    }
    if change.outboxed {
        return publish_outboxed();
    }
    append(session, delivery_changed_event(session, change))?;
    for record in &change.records {
        append(session, json!({"method": STATE_CHANGED_EVENT,
            "params": state_changed_params(session, &change.ledger_epoch, change.state_revision, record)}))?;
    }
    Ok(())
}

/// `COMMIT` of a ledger write, timed while the write is outboxed (054 S25).
fn commit_timed(tx: rusqlite::Transaction<'_>, timed: bool) -> rusqlite::Result<()> {
    let _commit = timing::span(timing::Op::Commit, timed);
    tx.commit()
}

fn upsert_session_revision(tx: &rusqlite::Transaction<'_>, session: &str, row: &SessionRow, state_revision: u64) -> Result<(), LedgerError> {
    tx.execute(
        "INSERT INTO input_ledger_sessions (session, state_revision, epoch, compacted_through)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(session) DO UPDATE SET state_revision = excluded.state_revision",
        params![session, state_revision as i64, row.epoch, row.compacted_through as i64],
    )?;
    Ok(())
}

fn upsert_record(
    tx: &rusqlite::Transaction<'_>,
    session: &str,
    input_id: &str,
    record: &InputRecord,
    state_revision: u64,
    now: DateTime<Utc>,
) -> Result<(), LedgerError> {
    let deadline_ms = record.deadline.as_deref()
        .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        .map(|at| at.timestamp_millis());
    let observation_ms=record.details["observationDeadline"].as_str()
        .and_then(|text|DateTime::parse_from_rfc3339(text).ok()).map(|at|at.timestamp_millis());
    tx.execute(
        "INSERT INTO input_ledger (session, input_id, rev, state, surface, terminal, updated_revision, updated_at_ms, deadline_ms, record_json, observation_deadline_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(session, input_id) DO UPDATE SET rev = excluded.rev, state = excluded.state,
           surface = excluded.surface, terminal = excluded.terminal, updated_revision = excluded.updated_revision,
           updated_at_ms = excluded.updated_at_ms, deadline_ms = excluded.deadline_ms,
           observation_deadline_ms = CASE
             WHEN excluded.state NOT IN ('received','sending') THEN NULL
             WHEN input_ledger.state NOT IN ('received','sending') THEN excluded.observation_deadline_ms
             ELSE COALESCE(input_ledger.observation_deadline_ms,excluded.observation_deadline_ms) END,
           record_json = excluded.record_json",
        params![
            session, input_id, record.rev as i64, record.state.as_str(), record.surface.as_str(),
            record.state.is_terminal(), state_revision as i64, now.timestamp_millis(), deadline_ms,
            stored_record_json(record)?, observation_ms,
        ],
    )?;
    Ok(())
}

/// [`record`] for a batch ([`InputLedger::transition_batch`]): one
/// transaction and one `stateRevision`; each changed record is published as
/// its own `cutex/input/stateChanged` carrying that shared revision.
pub fn record_batch(session: &str, items: Vec<(String, Expect, Next)>) -> Option<Vec<Transition>> {
    if !enabled() {
        return None;
    }
    let result = default_ledger()
        .map_err(LedgerError::Storage)
        .and_then(|ledger| ledger.transition_batch(session, items));
    match result {
        Ok(transitions) => {
            // Outboxed rows of the batch go out with one drain.
            let mut drained = false;
            for transition in transitions.iter().filter(|transition| transition.changed) {
                if transition.outboxed && std::mem::replace(&mut drained, true) {
                    continue;
                }
                if let Err(error) = publish(transition) {
                    eprintln!("cutex input ledger: publish {session}/{}: {error:#}", transition.record.input_id);
                }
            }
            Some(transitions)
        }
        Err(error) => {
            eprintln!("cutex input ledger: batch in {session}: {error}");
            None
        }
    }
}

/// The writers' entry point (C2–C5): a no-op unless the switch is on;
/// applies the transition on the default ledger and publishes it. Ledger
/// errors never fail the caller's own operation (its store is the source of
/// the underlying fact; §2.12 drift reconcile repairs the ledger) — they are
/// logged and `None` is returned. Use [`InputLedger::transition`] directly
/// when the caller must act on a conflict.
pub fn record(session: &str, input_id: &str, expect: Expect, next: Next) -> Option<Transition> {
    if !enabled() {
        return None;
    }
    let result = default_ledger()
        .map_err(LedgerError::Storage)
        .and_then(|ledger| ledger.transition(session, input_id, expect, next));
    match result {
        Ok(transition) => {
            if let Err(error) = publish(&transition) {
                eprintln!("cutex input ledger: publish {session}/{input_id}: {error:#}");
            }
            Some(transition)
        }
        Err(error) => {
            eprintln!("cutex input ledger: {session}/{input_id}: {error}");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Routes (`cutex/input/get`, `cutex/input/list`, `cutex/input/dismiss`,
// `cutex/input/lease/release`, HTTP)

pub fn is_method(method: &str) -> bool {
    matches!(method, GET_METHOD | LIST_METHOD | DISMISS_METHOD | LEASE_RELEASE_METHOD | RETRY_METHOD | HOLD_METHOD | super::input_disposal::METHOD)
}

/// Unread tracking that can be disposed by logical identity. Native read evidence
/// and terminal history remain truthful; source cleanup runs independently.
/// r274.2 §9.4: also an owner record that `failed`, or that a lost runtime
/// left stuck ([`owner_stuck`]).
pub fn dismissible(record: &InputRecord) -> bool {
    !matches!(record.state, InputState::InContext | InputState::Discarded | InputState::Superseded | InputState::Rejected)
}

/// Settle the underlying store first (per kind, in the block that owns it),
/// then the ledger (§2.9: the store is written before the ledger).
fn dismiss_underlying(record: &InputRecord) -> Result<bool, String> {
    match record.kind {
        InputKind::Owner => super::input_ledger_owner::dismiss(record).map(|_| false),
        InputKind::AgentMessage | InputKind::PeerMessage | InputKind::Alarm | InputKind::SystemNotice => {
            super::input_ledger_bus::dismiss(record).map(|_| false)
        }
        InputKind::DelegationRequest | InputKind::DelegationResult | InputKind::DelegationQuestion
        | InputKind::DelegationStatus => super::input_ledger_asks::dismiss(record),
    }
}

/// One dismiss item → `Ok(transition)` or `Err(failure json)`.
pub fn dismiss_one(ledger: &InputLedger, session: &str, input_id: &str, expected_rev: u64) -> Result<Transition, Value> {
    dismiss_one_with(ledger, session, input_id, expected_rev, dismiss_underlying)
}

fn dismiss_one_with(
    ledger: &InputLedger, session: &str, input_id: &str, expected_rev: u64,
    settle: impl FnOnce(&InputRecord) -> Result<bool, String>,
) -> Result<Transition, Value> {
    let failure = |code: &str, message: String, current: Option<u64>| {
        json!({"inputId": input_id, "code": code, "message": message, "currentRev": current})
    };
    // Persist first, independently of the source lane and the projected rev.
    // expectedRev remains a compatibility field; an owner decision is not
    // invalidated by a late receipt or another metadata update.
    let _ = (expected_rev, settle);
    super::input_disposal::request_at(&ledger.path,session,input_id,ledger.now())
        .map_err(|error|failure("event_persistence_unavailable",error.to_string(),None))?;
    let transition=ledger.transition(session,input_id,Expect::ANY,Next::discarded("ownerDiscarded"))
        .map_err(|error|failure("event_persistence_unavailable",error.to_string(),None))?;
    if let Err(error) = publish(&transition) {
        eprintln!("cutex input ledger: publish {session}/{input_id}: {error:#}");
    }
    Ok(transition)
}

fn parse_dismiss_items(object: &serde_json::Map<String, Value>) -> Result<Vec<(String, u64)>, String> {
    let item = |value: &Value| -> Result<(String, u64), String> {
        let object = value.as_object().ok_or("dismiss items must be objects")?;
        if object.len() != 2 {
            return Err("a dismiss item is exactly {inputId, expectedRev}".into());
        }
        let id = object.get("inputId").and_then(Value::as_str).filter(|id| !id.is_empty())
            .ok_or("inputId must be a non-empty string")?;
        let rev = parse_safe_integer(object.get("expectedRev"), "expectedRev")?
            .filter(|rev| *rev > 0)
            .ok_or("expectedRev must be a positive integer")?;
        Ok((id.to_string(), rev))
    };
    match (object.get("items"), object.len()) {
        (Some(Value::Array(items)), 1) if !items.is_empty() && items.len() <= MAX_DISMISS_ITEMS => items.iter().map(item).collect(),
        (Some(_), _) => Err(format!("items must hold 1..={MAX_DISMISS_ITEMS} {{inputId, expectedRev}} objects and nothing else")),
        (None, _) => Ok(vec![item(&Value::Object(object.clone()))?]),
    }
}

pub(super) fn route_error(code: &str, message: String, retryable: bool) -> super::user_input::UserInputExecutionError {
    super::user_input::UserInputExecutionError {
        stage: "route".to_string(),
        code: code.to_string(),
        message,
        retryable,
        details: json!({}),
        outcome_unknown: false,
    }
}

fn disabled_error() -> super::user_input::UserInputExecutionError {
    route_error("cutex_method_denied", "the input ledger is disabled (management.inputLedger.enabled)".into(), false)
}

pub(super) fn ledger_error(error: LedgerError) -> super::user_input::UserInputExecutionError {
    match error {
        LedgerError::Invalid(message) => route_error("invalid_request", message, false),
        other => route_error("event_persistence_unavailable", other.to_string(), true),
    }
}

/// Session requests (`POST /v2/sessions/{id}/requests`). The session id is the
/// request's `cutexSessionId`, so the params carry only the query.
pub(super) fn dispatch(
    cutex_session_id: &str,
    method: &str,
    params: &Value,
    owner: bool,
) -> Result<Value, super::user_input::UserInputExecutionError> {
    if method == super::input_disposal::METHOD {
        if !owner {return Err(route_error("owner_required", "disposing an input requires the owner's credential".into(),false));}
        let id=super::input_disposal::parse_request(params).map_err(|error|route_error("invalid_request",error.to_string(),false))?;
        let receipt=super::input_disposal::request(cutex_session_id,id).map_err(|error|route_error("event_persistence_unavailable",error.to_string(),true))?;
        super::input_disposal::ensure_started();
        return Ok(serde_json::to_value(receipt).expect("serializable disposition"));
    }
    if !enabled() {
        return Err(disabled_error());
    }
    if matches!(method, DISMISS_METHOD | LEASE_RELEASE_METHOD | RETRY_METHOD | HOLD_METHOD) && !owner {
        return Err(route_error("owner_required", format!("{method} is the owner's (Management root credential)"), false));
    }
    let object = params.as_object().ok_or_else(|| route_error("invalid_request", "params must be an object".into(), false))?;
    let ledger = default_ledger().map_err(|error| ledger_error(LedgerError::Storage(error)))?;
    match method {
        GET_METHOD => {
            if object.keys().any(|key| key != "inputIds") {
                return Err(route_error("invalid_request", "cutex/input/get accepts only inputIds".into(), false));
            }
            let ids = parse_ids(object.get("inputIds"))
                .map_err(|message| route_error("invalid_request", message, false))?;
            let result = ledger.get(cutex_session_id, &ids).map_err(ledger_error)?;
            Ok(serde_json::to_value(result).expect("serializable"))
        }
        LIST_METHOD => {
            if object.keys().any(|key| key != "sinceRevision" && key != "includeSettledSecs") {
                return Err(route_error("invalid_request", "cutex/input/list accepts sinceRevision and includeSettledSecs".into(), false));
            }
            let query = parse_list_query(object.get("sinceRevision"), object.get("includeSettledSecs"))
                .map_err(|message| route_error("invalid_request", message, false))?;
            let result = ledger.list(cutex_session_id, query).map_err(ledger_error)?;
            Ok(serde_json::to_value(result).expect("serializable"))
        }
        HOLD_METHOD => {
            let (id, rev, held) = parse_hold(object).map_err(|message| route_error("invalid_request", message, false))?;
            let lane = super::user_input::session_input_lock(cutex_session_id)
                .map_err(|error| route_error("event_persistence_unavailable", error.to_string(), true))?;
            let _lane = lane.lock().map_err(|_| route_error("event_persistence_unavailable", "session input lock poisoned".into(), true))?;
            let current = ledger.get(cutex_session_id, &[id.clone()]).map_err(ledger_error)?.records.into_iter().next()
                .ok_or_else(|| route_error("input_not_found", "input not found".into(), false))?;
            if current.rev != rev {
                return Err(route_error("revision_conflict", format!("input revision changed to {}", current.rev), false));
            }
            let action = if held { Action::Hold } else { Action::ReleaseHold };
            if !current.actions.contains(&action) {
                return Err(route_error("input_not_holdable", "input is no longer available for this action".into(), false));
            }
            let changed = match current.kind {
                InputKind::AgentMessage | InputKind::PeerMessage | InputKind::Alarm | InputKind::SystemNotice =>
                    super::agent_bus_state::agent_bus_message_repository().and_then(|store| store.set_owner_hold(cutex_session_id, &id, held)),
                InputKind::DelegationResult | InputKind::DelegationQuestion | InputKind::DelegationStatus =>
                    crate::asks::store::AskStore::open_default().and_then(|store| crate::asks::owner_carries::set_owner_hold(&store, cutex_session_id, &id, held, Utc::now())),
                _ => return Err(route_error("input_not_holdable", "only queued notices may be held".into(), false)),
            };
            if !changed.map_err(|error| route_error("input_not_holdable", error.to_string(), false))? {
                return Err(route_error("input_not_holdable", "the source input is no longer available for this action".into(), false));
            }
            // Source commits publish the authority change. Ask projections run
            // on their existing writer; give that bounded queue a chance to drain.
            crate::asks::ledger::flush(Duration::from_millis(100));
            super::input_ledger_settle::mark_dirty(cutex_session_id);
            let result = ledger.get(cutex_session_id, &[id]).map_err(ledger_error)?;
            let record = result.records.into_iter().next().ok_or_else(|| route_error("input_not_found", "input no longer available".into(), false))?;
            Ok(json!({"ledgerEpoch": result.ledger_epoch, "stateRevision": result.state_revision, "record": record}))
        }
        DISMISS_METHOD => {
            let items = parse_dismiss_items(object).map_err(|message| route_error("invalid_request", message, false))?;
            let mut records = Vec::new();
            let mut failed = Vec::new();
            for (id, rev) in items {
                match dismiss_one(ledger, cutex_session_id, &id, rev) {
                    Ok(transition) => records.push(transition.record),
                    Err(failure) => failed.push(failure),
                }
            }
            let head = ledger.get(cutex_session_id, &[]).map_err(ledger_error)?;
            Ok(json!({"ledgerEpoch": head.ledger_epoch, "stateRevision": head.state_revision, "records": records, "failed": failed}))
        }
        LEASE_RELEASE_METHOD => {
            if object.len() != 1 || !object.get("expectedLease").is_some_and(Value::is_object) {
                return Err(route_error("invalid_request", "cutex/input/lease/release takes exactly {expectedLease}".into(), false));
            }
            let result = super::input_ledger_asks::lease_release(cutex_session_id, &object["expectedLease"])
                .unwrap_or_else(|| Err(route_error("cutex_method_denied", "lease release is not served yet (054 C4)".into(), false)));
            super::input_ledger_settle::mark_dirty(cutex_session_id);
            result
        }
        // The session route serves it with the submit path (server.rs); here
        // only the ledger half runs and an owner input is not sent again.
        RETRY_METHOD => super::input_ledger_settle::retry(owner, cutex_session_id, params, &mut |_| {
            Err(route_error("app_server_unavailable", "no submit path on this route".into(), true))
        }),
        _ => Err(route_error("cutex_method_denied", format!("{method} is not an input ledger method"), false)),
    }
}

fn parse_hold(object: &serde_json::Map<String, Value>) -> Result<(String, u64, bool), String> {
    if object.len() != 3 || object.keys().any(|key| !matches!(key.as_str(), "inputId" | "expectedRev" | "held")) {
        return Err("cutex/input/hold takes exactly {inputId, expectedRev, held}".into());
    }
    let id = object.get("inputId").and_then(Value::as_str).ok_or("inputId must be a string")?;
    validate_id("inputId", id).map_err(|error| error.to_string())?;
    let rev = parse_safe_integer(object.get("expectedRev"), "expectedRev")?.filter(|rev| *rev > 0).ok_or("expectedRev must be positive")?;
    let held = object.get("held").and_then(Value::as_bool).ok_or("held must be boolean")?;
    Ok((id.into(), rev, held))
}

fn parse_ids(value: Option<&Value>) -> Result<Vec<String>, String> {
    let ids = value.and_then(Value::as_array).ok_or("inputIds must be an array")?;
    if ids.is_empty() || ids.len() > MAX_GET_IDS {
        return Err(format!("inputIds must hold 1..={MAX_GET_IDS} ids"));
    }
    ids.iter()
        .map(|id| id.as_str().filter(|id| !id.is_empty()).map(str::to_string).ok_or_else(|| "inputIds must be non-empty strings".to_string()))
        .collect()
}

pub(super) fn parse_safe_integer(value: Option<&Value>, label: &str) -> Result<Option<u64>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_u64()
            .filter(|value| *value <= super::model::MAX_SAFE_SEQUENCE)
            .map(Some)
            .ok_or_else(|| format!("{label} must be a JSON-safe non-negative integer")),
    }
}

fn parse_list_query(since: Option<&Value>, settled: Option<&Value>) -> Result<ListQuery, String> {
    Ok(ListQuery {
        since_revision: parse_safe_integer(since, "sinceRevision")?,
        include_settled_secs: parse_safe_integer(settled, "includeSettledSecs")?
            .unwrap_or(DEFAULT_INCLUDE_SETTLED_SECS)
            .min(SETTLED_RETENTION_SECS),
    })
}

/// `GET /v2/sessions/{id}/inputs[?since=&includeSettledSecs=]` and
/// `GET /v2/sessions/{id}/inputs/{inputId}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRoute {
    pub cutex_session_id: String,
    pub input_id: Option<String>,
}

pub fn parse_path(path: &str) -> Option<HttpRoute> {
    let rest = path.split('?').next()?.strip_prefix("/v2/sessions/")?;
    let (session, tail) = rest.split_once("/inputs")?;
    let input_id = match tail {
        "" => None,
        tail => Some(super::attachments::percent_decode(tail.strip_prefix('/')?)?),
    };
    let session = super::attachments::percent_decode(session)?;
    let valid = |id: &str| !id.is_empty() && !id.contains(['/', '\\']) && !id.chars().any(char::is_control);
    (valid(&session) && input_id.as_deref().is_none_or(valid))
        .then_some(HttpRoute { cutex_session_id: session, input_id })
}

pub fn handle(
    stream: &mut std::net::TcpStream,
    request: &crate::http::server::SimpleHttpRequest,
    route: HttpRoute,
) -> anyhow::Result<()> {
    use super::server::write_v2_error;
    if request.method != "GET" {
        return write_v2_error(stream, 405, "Method Not Allowed", "method_not_allowed", "use GET for inputs", false, json!({}));
    }
    if !enabled() {
        return write_v2_error(stream, 404, "Not Found", "not_found", "the input ledger is disabled", false, json!({}));
    }
    let mut since = None;
    let mut settled = None;
    let query = request.path.split_once('?').map(|(_, query)| query).unwrap_or("");
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        let parsed = value.parse::<u64>().map(Value::from).unwrap_or(Value::Null);
        match key.as_ref() {
            "since" if route.input_id.is_none() && since.is_none() && !parsed.is_null() => since = Some(parsed),
            "includeSettledSecs" if route.input_id.is_none() && settled.is_none() && !parsed.is_null() => settled = Some(parsed),
            _ => return write_v2_error(stream, 400, "Bad Request", "invalid_request", &format!("invalid query parameter {key}"), false, json!({})),
        }
    }
    let ledger = default_ledger()?;
    let result = match &route.input_id {
        Some(id) => ledger.get(&route.cutex_session_id, std::slice::from_ref(id)).map(|result| serde_json::to_value(result).expect("serializable")),
        None => match parse_list_query(since.as_ref(), settled.as_ref()) {
            Ok(query) => ledger.list(&route.cutex_session_id, query).map(|result| serde_json::to_value(result).expect("serializable")),
            Err(message) => return write_v2_error(stream, 400, "Bad Request", "invalid_request", &message, false, json!({})),
        },
    };
    match result {
        Ok(value) => crate::http::server::write_json_response(stream, 200, "OK", &value),
        Err(error) => write_v2_error(stream, 503, "Service Unavailable", "event_persistence_unavailable", &error.to_string(), true, json!({})),
    }
}

pub const EXPORTED_SCHEMA: super::schema_export::ExportedSchema = super::schema_export::ExportedSchema {
    name: "input-states",
    title: "Input ledger states, reason codes and surfaces",
    routes: &["cutex/input/get", "cutex/input/list", "cutex/input/stateChanged", "GET /v2/sessions/{id}/inputs", "GET /v2/sessions/{id}/inputs/{inputId}"],
    text: include_str!("schema/export/input-states.json"),
};

#[cfg(test)]
#[path = "input_ledger_tests.rs"]
mod tests;
