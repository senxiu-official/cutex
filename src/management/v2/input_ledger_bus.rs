//! Input ledger hooks owned by block C3 (bus notes, alarms, notices, Claude
//! stages; 054 §2.9 bus rows, §2.11, RC6/RC7/RC8, owner decisions 2 and 9).
//! C3 keeps its ledger mapping here so the edits in the bus / bridge files
//! stay a few lines each.
//!
//! How the bus writes the ledger: every agent-bus repository write that
//! names its messages (`DirtyRows::Messages`) compares a few small facts of
//! each message before and after the write ([`BusFacts`]); a change becomes
//! a ledger target ([`target_of`]) that is applied after the repository lock
//! is released (store first, then ledger, §2.9). The target is reached over
//! allowed edges only ([`plan`]): a missing record is created on the way, a
//! record that is already further along is never moved back. Only the
//! submit itself (`sending`, invariant 7: native IO started) and a refused
//! tell are written explicitly by the bridges / the bus server.
//!
//! Everything here is a no-op while `management.inputLedger.enabled` is off.

use std::cell::Cell;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use super::input_ledger::{
    self, Evidence, Expect, InputIntent, InputKind, InputLedger, InputRecord, InputRemote, InputSender,
    InputState, LedgerError, Next, NewInput, SenderKind, Stage, Surface, Timing, READ_SIGNAL,
};
use crate::agent_bus::model::AgentBusMessage;
use crate::app_server::external_input::{DeliveryState, ProcessingState, Status};

/// Backfill batch size (054 §2.12: batches of 200).
pub const BACKFILL_BATCH: usize = 200;
/// A backfilled message that reached the native runtime this long ago without
/// a receipt cannot be proven any more: `uncertain{receiptTimeout}`.
pub const BACKFILL_UNPROVABLE_SECS: i64 = 10 * 60;

// ---------------------------------------------------------------------------
// Switch, ledger and the test seams

#[cfg(test)]
thread_local! {
    static TEST_LEDGER: Cell<Option<&'static InputLedger>> = const { Cell::new(None) };
}

thread_local! {
    /// Set while the owner's `cutex/input/dismiss` settles the bus row: the
    /// dismiss itself writes `discarded{ownerDiscarded}` with its own CAS.
    static SUPPRESSED: Cell<bool> = const { Cell::new(false) };
}

/// Tests: run `body` with `ledger` as this thread's ledger and the switch on.
#[cfg(test)]
pub(crate) fn with_test_ledger<T>(ledger: &'static InputLedger, body: impl FnOnce() -> T) -> T {
    TEST_LEDGER.with(|cell| cell.set(Some(ledger)));
    let result = body();
    TEST_LEDGER.with(|cell| cell.set(None));
    result
}

/// Whether bus writes are mapped into the ledger now (switch on, not inside
/// the owner's dismiss).
pub fn active() -> bool {
    if SUPPRESSED.with(Cell::get) {
        return false;
    }
    #[cfg(test)]
    if TEST_LEDGER.with(Cell::get).is_some() {
        return true;
    }
    input_ledger::enabled()
}

fn ledger() -> anyhow::Result<&'static InputLedger> {
    #[cfg(test)]
    if let Some(ledger) = TEST_LEDGER.with(Cell::get) {
        return Ok(ledger);
    }
    input_ledger::default_ledger()
}

fn suppressed<T>(body: impl FnOnce() -> T) -> T {
    let before = SUPPRESSED.with(|cell| cell.replace(true));
    let result = body();
    SUPPRESSED.with(|cell| cell.set(before));
    result
}

/// One transition + its `cutex/input/stateChanged`; errors are logged, never
/// returned (the bus store is the source of the fact; §2.12 drift reconcile).
fn write(session: &str, id: &str, next: Next) -> Option<InputRecord> {
    let result = ledger()
        .map_err(LedgerError::Storage)
        .and_then(|ledger| ledger.transition(session, id, Expect::ANY, next));
    match result {
        Ok(transition) => {
            if let Err(error) = input_ledger::publish(&transition) {
                eprintln!("cutex input ledger (bus): publish {session}/{id}: {error:#}");
            }
            Some(transition.record)
        }
        Err(error) => {
            eprintln!("cutex input ledger (bus): {session}/{id}: {error}");
            None
        }
    }
}

fn current(session: &str, id: &str) -> Option<InputRecord> {
    let ledger = ledger().ok()?;
    match ledger.get(session, &[id.to_string()]) {
        Ok(mut result) => result.records.pop(),
        Err(error) => {
            eprintln!("cutex input ledger (bus): read {session}/{id}: {error}");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// What a bus message is (kind, sender, intent)

/// The ledger identity of a bus message: kind / sender by who sent it, and
/// the timing it was queued with (`intent.timing` is never defaulted, #12).
pub fn new_input(
    canonical: Option<&AgentBusMessage>,
    message_id: &str,
    from_cutex_session_id: Option<&str>,
    delivery_mode: &str,
    content: &str,
    sender_name: Option<&str>,
) -> NewInput {
    let approval = is_approval(canonical, message_id);
    let timing = timing_of(canonical, message_id, delivery_mode);
    let mode = match delivery_mode {
        "passive" => input_ledger::DeliveryMode::Passive,
        "soon" => input_ledger::DeliveryMode::Soon,
        "interrupt" => input_ledger::DeliveryMode::Interrupt,
        _ => input_ledger::DeliveryMode::AfterTurn,
    };
    let from = canonical.map(|message| message.from.clone()).filter(|from| !from.is_empty());
    let system = |kind: InputKind, sender: SenderKind, fallback: &str| {
        let id = from.clone().unwrap_or_else(|| fallback.to_string());
        (kind, InputSender { kind: sender, name: id.clone(), id, host: None, cutex_session_id: None }, None)
    };
    let peer = canonical
        .filter(|message| crate::agent_bus::peer_tell::is_peer_tell(message))
        .and_then(|message| message.control_payload.clone())
        .and_then(|payload| serde_json::from_value::<crate::agent_bus::peer_tell::PeerTellFacts>(payload).ok());
    let (kind, sender, remote) = if let Some(session) = from_cutex_session_id {
        let name = sender_name.map(str::to_string).filter(|name| !name.is_empty()).unwrap_or_else(|| session.to_string());
        (InputKind::AgentMessage,
            InputSender { kind: SenderKind::Agent, id: session.to_string(), name, host: None, cutex_session_id: Some(session.to_string()) },
            None)
    } else if let Some(facts) = peer {
        let label = facts.sender_label();
        let host = crate::peers::wire::sanitize_name(&facts.host);
        let remote = InputRemote {
            host: host.clone(),
            peer_id: facts.peer_id.clone(),
            sender_id: crate::peers::wire::sanitize_name(&facts.sender_id),
        };
        (InputKind::PeerMessage,
            InputSender { kind: SenderKind::Peer, id: label.clone(), name: label, host: Some(host), cutex_session_id: None },
            Some(remote))
    } else if approval {
        system(InputKind::SystemNotice, SenderKind::Approval, crate::agent_bus::alarms::ALARM_SYSTEM_SENDER)
    } else if canonical.is_some_and(|m| m.sender_kind.is_alarm_system() || m.from == crate::agent_bus::alarms::ALARM_SYSTEM_SENDER)
        || canonical.is_none() && message_id.starts_with("alarm-")
    {
        system(InputKind::Alarm, SenderKind::Alarm, crate::agent_bus::alarms::ALARM_SYSTEM_SENDER)
    } else if canonical.is_some_and(|m| m.sender_kind.is_job_service_system()) {
        system(InputKind::SystemNotice, SenderKind::JobService, "Cutex Job Service")
    } else if canonical.is_some_and(|m| m.from == crate::agent_management::AGENT_MANAGEMENT_SYSTEM_SENDER) {
        system(InputKind::SystemNotice, SenderKind::Management, crate::agent_management::AGENT_MANAGEMENT_SYSTEM_SENDER)
    } else if canonical.is_some_and(crate::agent_bus::claude_task::is_claude_task)
        || canonical.is_none() && message_id.starts_with(crate::agent_bus::claude_task::MESSAGE_ID_PREFIX)
    {
        system(InputKind::SystemNotice, SenderKind::ClaudeTask, crate::agent_bus::claude_task::SOURCE_ID)
    } else if canonical.is_some_and(|m| m.sender_kind.is_agent()) {
        let id = from.clone().unwrap_or_else(|| "unknown".into());
        (InputKind::AgentMessage, InputSender { kind: SenderKind::Agent, name: id.clone(), id, host: None, cutex_session_id: None }, None)
    } else {
        system(InputKind::SystemNotice, SenderKind::Cutex, "cutex")
    };
    NewInput {
        kind,
        sender,
        intent: InputIntent { strategy: None, delivery_mode: Some(mode), timing },
        preview: content.to_string(),
        ask_id: None,
        remote,
    }
}

fn is_approval(canonical: Option<&AgentBusMessage>, message_id: &str) -> bool {
    canonical.is_some_and(crate::agent_bus::delivery::is_approval_notice)
        || canonical.is_none() && message_id.starts_with(crate::agent_bus::alarms::APPROVAL_NOTICE_MESSAGE_PREFIX)
}

/// `intent.timing` of a bus message: an approval notice and a `soon` wake are
/// read at the next step, a note passively, everything else after the turn.
pub fn timing_of(canonical: Option<&AgentBusMessage>, message_id: &str, delivery_mode: &str) -> Timing {
    if is_approval(canonical, message_id) || delivery_mode == "soon" {
        Timing::NextStep
    } else if delivery_mode == "passive" {
        Timing::Passive
    } else {
        Timing::AfterTurn
    }
}

fn timing_reason(timing: Timing) -> &'static str {
    match timing {
        Timing::AfterTurn => "afterTurn",
        Timing::NextStep => "nextStep",
        Timing::Passive => "passive",
    }
}

// ---------------------------------------------------------------------------
// Bus facts → ledger target

/// What the native runtime last said about a frozen message (RC8: these are
/// no longer folded into "accepted").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeObserved {
    /// Source policy annotates the small observation projection without adding
    /// native acceptance evidence. Kept here so BusFacts' public constructors
    /// remain compatible; only the source store constructs this wrapper.
    OwnerPolicy { hold: Option<bool>, carried: bool, native: Option<Box<NativeObserved>> },
    /// Pending / claimed / context persisted without a committed receipt yet.
    Accepted,
    Held(String),
    Retry,
    Conflict,
}

impl NativeObserved {
    pub fn of(status: &Status) -> Option<Self> {
        match (&status.delivery_state, &status.processing.state) {
            (DeliveryState::Conflict, _) => Some(Self::Conflict),
            (DeliveryState::RetryableError, _) => Some(Self::Retry),
            (_, ProcessingState::Held) => Some(Self::Held(
                status.processing.reason.as_ref()
                    .and_then(|reason| serde_json::to_value(reason).ok())
                    .and_then(|reason| reason.as_str().map(str::to_string))
                    .unwrap_or_else(|| "unknown".into()),
            )),
            (DeliveryState::Pending | DeliveryState::ContextPersisted, _) => Some(Self::Accepted),
            (DeliveryState::Unknown, ProcessingState::Pending | ProcessingState::Claimed | ProcessingState::OutputObserved) => {
                Some(Self::Accepted)
            }
            (DeliveryState::Unknown, ProcessingState::None) => None,
        }
    }
}

/// The small facts of one stored bus message the ledger depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusFacts {
    pub state: String,
    pub timing: Timing,
    pub frozen: bool,
    pub a2: bool,
    pub inbox: bool,
    pub a4: bool,
    pub external_receipt: bool,
    pub observed: Option<NativeObserved>,
    pub turn: Option<String>,
    pub settlement_closed: bool,
    pub error_code: Option<String>,
    /// The native timeline item a delivered message is (054 §3.2): the
    /// receipt's `responseItemId` (ExternalInput / A4), else the message id
    /// (the Claude adapter projects the message as an `external_event` item
    /// with the message id as its id). `None` while not delivered.
    pub item: Option<String>,
}

impl BusFacts {
    pub fn native_evidence(&self) -> bool {
        let observed = match &self.observed {
            Some(NativeObserved::OwnerPolicy { native, .. }) => native.is_some(),
            other => other.is_some(),
        };
        self.frozen || self.a2 || self.inbox || self.a4 || self.external_receipt || observed
    }
}

/// Where the ledger record of a bus message should be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Carried(Box<Target>),
    Waiting { reason: &'static str, details: Option<Value> },
    Sending,
    Accepted { turn: Option<String> },
    InContext { evidence: Evidence, turn: Option<String>, item: Option<String> },
    Uncertain(&'static str),
    Rejected(&'static str),
    Discarded(&'static str),
}

impl Target {
    pub(super) fn state(&self) -> InputState {
        match self {
            Self::Carried(inner) => inner.state(),
            Self::Waiting { .. } => InputState::Waiting,
            Self::Sending => InputState::Sending,
            Self::Accepted { .. } => InputState::Accepted,
            Self::InContext { .. } => InputState::InContext,
            Self::Uncertain(_) => InputState::Uncertain,
            Self::Rejected(_) => InputState::Rejected,
            Self::Discarded(_) => InputState::Discarded,
        }
    }
}

/// Waiting reasons that report what the native runtime said after a submit:
/// the only ones that may follow `sending` (§2.3 `sending → waiting`).
fn native_waiting(reason: &str) -> bool {
    matches!(reason, "nativeHeld" | "nativeRetry" | "returned")
}

/// §2.9 bus rows: the ledger target of a stored message.
pub fn target_of(facts: &BusFacts) -> Target {
    if let Some(NativeObserved::OwnerPolicy { hold, carried, native }) = &facts.observed {
        let mut inner = facts.clone();
        inner.observed = native.as_deref().cloned();
        let target = target_of(&inner);
        if *carried { return Target::Carried(Box::new(target)); }
        if let (Some(held), Target::Waiting { reason, details }) = (hold, &target) {
            let mut details = details.clone().unwrap_or_else(|| json!({}));
            details["ownerHold"] = json!(held);
            return Target::Waiting { reason: if *held { "ownerPause" } else { reason }, details: Some(details) };
        }
        return target;
    }
    let owner_discarded = matches!(facts.error_code.as_deref(),
        Some(super::agent_bus_state::OWNER_DISCARD_CODE | "stale_message_discarded"));
    match facts.state.as_str() {
        "delivered" => {
            let evidence = if facts.a4 {
                Evidence::A4Receipt
            } else if facts.external_receipt {
                Evidence::ExternalInputPersisted
            } else {
                Evidence::ContextPersisted
            };
            return Target::InContext { evidence, turn: facts.turn.clone(), item: facts.item.clone() };
        }
        _ if facts.settlement_closed => return Target::Discarded("settlementClosed"),
        "discarded" => return Target::Discarded("ownerDiscarded"),
        "quarantined" if owner_discarded => return Target::Discarded("ownerDiscarded"),
        "quarantined" if facts.error_code.as_deref() == Some("native_semantic_conflict") => {
            return Target::Uncertain("nativeConflict");
        }
        "quarantined" => return Target::Rejected("quarantined"),
        "failed" if facts.error_code.as_deref() == Some("target_permanently_retired") => {
            return Target::Rejected("targetUnavailable");
        }
        "failed" => return Target::Rejected("invalid"),
        "stale_held" => return Target::Waiting { reason: "staleHeld", details: None },
        _ => {}
    }
    match &facts.observed {
        Some(NativeObserved::Held(reason)) => {
            return Target::Waiting { reason: "nativeHeld", details: Some(json!({"nativeReason": reason})) };
        }
        Some(NativeObserved::Retry) => return Target::Waiting { reason: "nativeRetry", details: None },
        Some(NativeObserved::Conflict) => return Target::Uncertain("nativeConflict"),
        Some(NativeObserved::Accepted) => return Target::Accepted { turn: None },
        Some(NativeObserved::OwnerPolicy { .. }) => unreachable!("source policy unwrapped above"),
        None => {}
    }
    if facts.a2 || facts.inbox || facts.state == "inbox_accepted" {
        return Target::Accepted { turn: None };
    }
    Target::Waiting { reason: timing_reason(facts.timing), details: None }
}

fn next_of(target: &Target) -> Next {
    match target {
        Target::Carried(inner) => {
            let next = next_of(inner);
            if matches!(inner.as_ref(), Target::Accepted { .. } | Target::InContext { .. }) {
                next.reason("carriedByOwner")
            } else { next }
        }
        Target::Waiting { reason, details } => {
            let next = Next::waiting(reason);
            match details {
                Some(details) => next.details(details.clone()),
                None => next,
            }
        }
        Target::Sending => Next::sending(),
        Target::Accepted { turn } => match turn {
            Some(turn) => Next::accepted().turn(turn.clone()),
            None => Next::accepted(),
        },
        Target::InContext { evidence, turn, item } => {
            let mut next = Next::in_context(*evidence);
            if let Some(turn) = turn {
                next = next.turn(turn.clone());
            }
            if let Some(item) = item {
                next = next.timeline_item(item.clone());
            }
            next
        }
        Target::Uncertain(reason) => Next::uncertain(reason),
        Target::Rejected(reason) => Next::rejected(reason),
        Target::Discarded(reason) => Next::discarded(reason),
    }
}

/// The transitions that take a record in `current` (None: no record yet) to
/// `target` over §2.3 edges, or nothing when the record is already further
/// along (no `accepted → waiting`, nothing after a terminal state).
/// `timing` is the waiting reason used when a record must be created on the
/// way to `discarded`.
pub fn plan(current: Option<InputState>, target: &Target, timing: Timing) -> Vec<Next> {
    if let Target::Carried(inner) = target {
        return plan(current, inner, timing).into_iter().map(|next| {
            if matches!(next.state, InputState::Accepted | InputState::InContext) {
                next.reason("carriedByOwner")
            } else { next }
        }).collect();
    }
    use InputState::*;
    let via_sending = |next: Next| vec![Next::sending(), next];
    let next = next_of(target);
    match (current, target) {
        (None, Target::Waiting { .. } | Target::Sending | Target::Rejected(_)) => vec![next],
        (None, Target::Accepted { .. } | Target::InContext { .. } | Target::Uncertain(_)) => via_sending(next),
        (None, Target::Discarded(_)) => vec![Next::waiting(timing_reason(timing)), next],
        (Some(Waiting), Target::Waiting { .. } | Target::Sending | Target::Rejected(_) | Target::Discarded(_)) => vec![next],
        (Some(Waiting), Target::Accepted { .. } | Target::InContext { .. } | Target::Uncertain(_)) => via_sending(next),
        (Some(Sending), Target::Waiting { reason, .. }) if native_waiting(reason) => vec![next],
        (Some(Sending), Target::Accepted { .. } | Target::InContext { .. } | Target::Uncertain(_)) => vec![next],
        (Some(Sending | Accepted), Target::Discarded(_)) => vec![Next::uncertain("outcomeUnknown"), next],
        (Some(Sending | Accepted), Target::Rejected(_)) => vec![Next::uncertain("outcomeUnknown")],
        (Some(Accepted), Target::Accepted { turn: Some(_) } | Target::InContext { .. } | Target::Uncertain(_)) => vec![next],
        (Some(Uncertain), Target::InContext { .. } | Target::Discarded(_)) => vec![next],
        _ => Vec::new(),
    }
}

/// Drive the record of `id` in `session` to `target` (creating it on the way
/// with `input`). Returns the last record written.
pub fn drive(session: &str, id: &str, input: &NewInput, target: &Target) -> Option<InputRecord> {
    if !active() {
        return None;
    }
    let existing = current(session, id);
    let mut last = None;
    for next in plan(existing.as_ref().map(|record| record.state), target, input.intent.timing) {
        last = write(session, id, next.create(input.clone()));
        if last.is_none() {
            break;
        }
    }
    last
}

/// One message a repository write changed.
#[derive(Debug, Clone)]
pub struct BusChange {
    pub session: String,
    pub message_id: String,
    pub before: Option<BusFacts>,
    pub after: BusFacts,
    pub input: NewInput,
}

thread_local! {
    static DEFERRED: std::cell::RefCell<Option<Vec<BusChange>>> = const { std::cell::RefCell::new(None) };
}

/// Changes a repository write makes inside `body` are collected instead of
/// applied: for callers that hold the host authority fence (no ledger IO
/// under it, 054 §2.13 / 051). Apply the result after the fence is released.
pub fn deferred<T>(body: impl FnOnce() -> T) -> (T, Vec<BusChange>) {
    let outer = DEFERRED.with(|cell| cell.replace(Some(Vec::new())));
    let result = body();
    let changes = DEFERRED.with(|cell| cell.replace(outer)).unwrap_or_default();
    (result, changes)
}

/// Writes ledger changes off this thread (a worker that may hold the
/// authority fence); order per message is kept by the single writer.
fn background(changes: Vec<BusChange>) {
    use std::sync::mpsc;
    static WRITER: std::sync::OnceLock<std::sync::Mutex<mpsc::Sender<Vec<BusChange>>>> = std::sync::OnceLock::new();
    let sender = WRITER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel::<Vec<BusChange>>();
        let spawned = std::thread::Builder::new().name("cutex-input-ledger-bus".into()).spawn(move || {
            for changes in receiver {
                apply_now(changes);
            }
        });
        if let Err(error) = spawned {
            eprintln!("cutex input ledger (bus): writer thread not started: {error}");
        }
        std::sync::Mutex::new(sender)
    });
    let sent = sender.lock().map(|sender| sender.send(changes));
    if !matches!(sent, Ok(Ok(()))) {
        eprintln!("cutex input ledger (bus): writer unavailable; the drift reconcile repairs the ledger");
    }
}

/// Apply what a committed repository write changed (called after the
/// repository lock is released). Inside [`deferred`] the changes are
/// collected; on an asks worker holding the authority fence they go to the
/// background writer.
pub fn apply_changes(changes: Vec<BusChange>) {
    let mut pending = Some(changes);
    DEFERRED.with(|cell| {
        if let Some(collected) = cell.borrow_mut().as_mut() {
            collected.extend(pending.take().unwrap_or_default());
        }
    });
    let Some(changes) = pending else { return };
    if crate::asks::pause_admission::worker_fence_held() {
        background(changes);
        return;
    }
    apply_now(changes);
}

fn apply_now(changes: Vec<BusChange>) {
    for change in changes {
        let target = target_of(&change.after);
        if change.before.as_ref().is_some_and(|before| target_of(before) == target) {
            continue;
        }
        drive(&change.session, &change.message_id, &change.input, &target);
    }
}

// ---------------------------------------------------------------------------
// Explicit writes (bridges, bus server)

/// Native IO starts now (freeze done, submit next; invariant 7).
pub fn sending(session: &str, id: &str, input: &NewInput) {
    drive(session, id, input, &Target::Sending);
}

/// r274.2 §9.1: the Claude adapter emitted the native `external_event` item
/// of frozen r8 message `id` in `turn` (claimed into that turn's prompt; an
/// audit projection, not a context receipt). The record moves once to the
/// timeline as `accepted` with `turnId` and `timelineItemId` = the message
/// id (`details.readSignal: "nativeItem"`); `inContext` still waits for the
/// validated receipt. Nothing for a record that is already `inContext`,
/// `uncertain` or terminal (the receipt / a check decides those).
pub fn native_item(session: &str, id: &str, input: &NewInput, turn: &str) -> Option<InputRecord> {
    native_item_with_carry(session, id, input, turn, false)
}

pub(super) fn native_item_with_carry(session: &str, id: &str, input: &NewInput, turn: &str, carried: bool) -> Option<InputRecord> {
    if !active() {
        return None;
    }
    let record = current(session, id);
    let mut read = Next::accepted().turn(turn).timeline_item(id);
    if carried || record.as_ref().is_some_and(|record| record.reason.as_deref() == Some("carriedByOwner")) {
        read = read.reason("carriedByOwner");
    }
    read = match &record {
        Some(record) => read.details(with_read_signal(record, "nativeItem")),
        None => read.details(json!({READ_SIGNAL: "nativeItem"})),
    };
    // Read again by another turn (released by the one before): a receipt
    // deadline of the earlier turn no longer applies.
    if record.as_ref().is_some_and(|record| record.state == InputState::Accepted && record.turn_id.as_deref() != Some(turn)) {
        read = read.deadline(None);
    }
    let nexts = match record.map(|record| record.state) {
        None | Some(InputState::Waiting) => vec![Next::sending(), read],
        Some(InputState::Sending | InputState::Accepted) => vec![read],
        _ => Vec::new(),
    };
    let mut last = None;
    for next in nexts {
        last = write(session, id, next.create(input.clone()));
        if last.is_none() {
            break;
        }
    }
    // The record now names its turn, so the session delivery's `turn.kind`
    // and `inputIds` change; no native item event marks the session dirty.
    if last.is_some() {
        super::input_ledger_settle::mark_dirty(session);
    }
    last
}

/// `record`'s details plus `readSignal: signal` (a same-state write keeps
/// its other details; a state change starts from none).
pub fn with_read_signal(record: &InputRecord, signal: &str) -> Value {
    let mut details = if record.state == InputState::Accepted && record.details.is_object() { record.details.clone() } else { json!({}) };
    details[READ_SIGNAL] = json!(signal);
    details
}

/// A waiting record's reason changes (e.g. the recipient has no endpoint at
/// redrive: `waiting{agentOffline}`). Never moves a record back to waiting.
pub fn waiting_reason(session: &str, id: &str, input: &NewInput, reason: &'static str) {
    if !active() {
        return;
    }
    match current(session, id).map(|record| record.state) {
        None | Some(InputState::Waiting) => {
            write(session, id, Next::waiting(reason).create(input.clone()));
        }
        _ => {}
    }
}

/// Owner decision 2: an `agent_tell` to a target without a runtime endpoint
/// stays refused. The refusal is recorded twice, both `rejected{targetUnavailable}`
/// with surface none and the same id (the one returned to the caller):
/// - in the TARGET's ledger naming the sender (the input that never arrived);
/// - in the SENDER's ledger as an outgoing notice (`kind systemNotice`, sender
///   cutex, `details.direction = "outgoing"`, `details.targetCutexSessionId`), so the
///   sender side keeps a record of the refused tell (`cutex/input/list` on the
///   sender session, kept 7 days like every terminal record).
pub fn tell_refused(
    target_session: &str,
    message_id: &str,
    sender_session: &str,
    sender_name: Option<&str>,
    text: &str,
    wake: bool,
) -> Option<String> {
    if !active() {
        return None;
    }
    if current(target_session, message_id).is_some() {
        return Some(message_id.to_string());
    }
    let input = new_input(None, message_id, Some(sender_session), if wake { "after_turn" } else { "passive" }, text, sender_name);
    let next = Next::rejected("targetUnavailable")
        .surface(Surface::None)
        .details(json!({"code": "target_unavailable"}))
        .create(input);
    let recorded = write(target_session, message_id, next).map(|_| message_id.to_string());
    if current(sender_session, message_id).is_none() {
        let notice = NewInput {
            kind: InputKind::SystemNotice,
            sender: InputSender { kind: SenderKind::Cutex, id: "cutex".into(), name: "Cutex".into(), host: None, cutex_session_id: None },
            intent: InputIntent { strategy: None, delivery_mode: None, timing: Timing::Passive },
            preview: format!("tell refused, target offline: {text}"),
            ask_id: None,
            remote: None,
        };
        write(sender_session, message_id, Next::rejected("targetUnavailable").surface(Surface::None)
            .details(json!({"code": "target_unavailable", "direction": "outgoing", "targetCutexSessionId": target_session}))
            .create(notice));
    }
    recorded
}

/// §2.11: Claude `claude/input/stageChanged` / `claude/input/contextPersisted`
/// for an owner insertion (record id = its clientId, created by the owner
/// block). Advances `accepted.stage` monotonically (registered → offered) or
/// to `inContext{contextPersisted}`. A missing record or an input without a
/// clientId (bus inputs are settled by their validated receipts) is ignored.
pub fn claude_input_stage(session: &str, method: &str, params: Option<&Value>) {
    if !matches!(method, "claude/input/stageChanged" | "claude/input/contextPersisted") || !active() {
        return;
    }
    let Some(params) = params else { return };
    let Some(id) = params.get("clientId").and_then(Value::as_str).filter(|id| !id.trim().is_empty()) else {
        return;
    };
    let turn = params.get("turnId").and_then(Value::as_str).filter(|turn| !turn.is_empty());
    let stage = if method == "claude/input/contextPersisted" {
        None
    } else {
        match params.get("stage").and_then(Value::as_str) {
            Some("registered") => Some(Stage::Registered),
            Some("offered") => Some(Stage::Offered),
            Some("contextPersisted") => None,
            _ => return,
        }
    };
    let Some(record) = current(session, id) else { return };
    let rank = |stage: Option<Stage>| match stage {
        None => 0,
        Some(Stage::Registered) => 1,
        Some(Stage::Offered) => 2,
    };
    let mut next = match stage {
        Some(stage) => {
            let allowed = match record.state {
                // `sending{interruptSend}`: the owner took this steer out of
                // the turn being interrupted; a late stage of that steer must
                // not make it `accepted` there again (the interrupted turn's
                // settle would then fail it while it is being sent anew).
                InputState::Sending => record.reason.as_deref() != Some(super::input_interrupt_send::SENDING_REASON),
                InputState::Accepted => rank(Some(stage)) > rank(record.stage),
                _ => false,
            };
            if !allowed {
                return;
            }
            let mut next = Next::accepted().stage(stage);
            if stage == Stage::Offered {
                // r274.2 §9.1: offered (or the steer echo, whichever comes
                // first) moves the record to the timeline, still `accepted`;
                // the steer item's id is the message id by construction.
                if let Some(item) = params.get("messageId").and_then(Value::as_str).filter(|item| !item.is_empty()) {
                    next = next.timeline_item(item);
                }
                if record.surface != Surface::Timeline {
                    next = next.details(with_read_signal(&record, "offered"));
                }
            }
            next
        }
        None => {
            if !matches!(record.state, InputState::Sending | InputState::Accepted | InputState::Uncertain) {
                return;
            }
            Next::in_context(Evidence::ContextPersisted)
        }
    };
    if let Some(turn) = turn {
        next = next.turn(turn);
    }
    write(session, id, next);
}

// ---------------------------------------------------------------------------
// Owner dismiss (`cutex/input/dismiss`, §2.14, owner decision 9)

/// Settle the bus message before the ledger marks it
/// `discarded{ownerDiscarded}`. Never refuses for a settlement / evidence
/// reason: a row that is still open is owner-discarded (protected ones too:
/// the owner asked); a row that is gone, already terminal or already closed
/// needs nothing more (ledger only).
pub(crate) fn dismiss(record: &InputRecord) -> Result<(), String> {
    let repository = super::agent_bus_state::agent_bus_message_repository().map_err(|error| format!("{error:#}"))?;
    if !dismiss_with(repository, record, Utc::now())? {
        return Ok(());
    }
    // Like the stuck-message discard: take it out of the live queue too
    // (best effort; a bridge also ACKs an owner-discarded message it polls).
    let bus = crate::agent_bus::client::AgentBusHttpClient::from_config(&crate::config::store::load_codez_config());
    if let Err(error) = bus.withdraw_messages(std::slice::from_ref(&record.input_id), "dismissed by the owner") {
        eprintln!("cutex input ledger (bus): withdraw {} after dismiss: {error:#}", record.input_id);
    }
    Ok(())
}

pub(crate) fn dismiss_with(
    repository: &super::agent_bus_state::AgentBusMessageRepository,
    record: &InputRecord,
    now: DateTime<Utc>,
) -> Result<bool, String> {
    let outcome = suppressed(|| {
        repository.owner_discard(&record.input_id, None, "dismissed by the owner (cutex/input/dismiss)", true, now)
    })
    .map_err(|error| format!("{error:#}"))?;
    match outcome {
        Ok(outcome) => Ok(outcome.changed),
        // Gone, already terminal, already closed: nothing left to settle.
        Err(super::agent_bus_state::OwnerDiscardError::NotFound
            | super::agent_bus_state::OwnerDiscardError::NotDiscardable { .. }) => Ok(false),
        Err(error) => Err(error.message()),
    }
}

// ---------------------------------------------------------------------------
// Backfill (decision 9): ledger records for bus messages that were open
// before the switch was turned on.

/// One open bus message, as the backfill reads it.
#[derive(Debug, Clone)]
pub struct BackfillItem {
    pub session: String,
    pub message_id: String,
    pub facts: BusFacts,
    pub input: NewInput,
    pub updated_at: Option<DateTime<Utc>>,
}

/// The backfill target: like [`target_of`], except that a message that
/// reached the native runtime long ago and was never confirmed cannot be
/// proven any more (`uncertain{receiptTimeout}`, dismissible), and a message
/// frozen but never observed is still waiting (the bridge submits it again
/// and writes `sending` then).
pub fn backfill_target(item: &BackfillItem, now: DateTime<Utc>) -> Target {
    let target = target_of(&item.facts);
    let old = item.updated_at.is_none_or(|at| (now - at).num_seconds() >= BACKFILL_UNPROVABLE_SECS);
    match target {
        Target::Accepted { .. } | Target::Sending if old => Target::Uncertain("receiptTimeout"),
        Target::Sending => Target::Waiting { reason: timing_reason(item.facts.timing), details: None },
        other => other,
    }
}

/// Create the missing records of one batch. Returns how many were created.
pub fn backfill_batch(items: &[BackfillItem], now: DateTime<Utc>) -> usize {
    if !active() || items.is_empty() {
        return 0;
    }
    let Ok(ledger) = ledger() else { return 0 };
    let mut sessions: std::collections::BTreeMap<&str, Vec<&BackfillItem>> = Default::default();
    for item in items {
        sessions.entry(item.session.as_str()).or_default().push(item);
    }
    let mut created = 0;
    for (session, items) in sessions {
        for chunk in items.chunks(input_ledger::MAX_GET_IDS) {
            let ids = chunk.iter().map(|item| item.message_id.clone()).collect::<Vec<_>>();
            let missing = match ledger.get(session, &ids) {
                Ok(result) => result.missing,
                Err(error) => {
                    eprintln!("cutex input ledger (bus backfill): read {session}: {error}");
                    continue;
                }
            };
            for item in chunk.iter().filter(|item| missing.contains(&item.message_id)) {
                if drive(session, &item.message_id, &item.input, &backfill_target(item, now)).is_some() {
                    created += 1;
                }
            }
        }
    }
    created
}

#[cfg(test)]
#[path = "input_ledger_bus_tests.rs"]
mod tests;
