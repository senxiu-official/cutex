//! Input ledger settlement and session delivery (054 block C5).
//!
//! - §2.10 turn end: a turn that ended `interrupted` / `failed` gets ONE
//!   check per `accepted` record it carried (Codex / Claude ExternalInput
//!   status, the Claude local inbox status, or one single-turn item read for
//!   an owner steer); a `completed` turn starts the receipt grace of its
//!   still-`accepted` records, whose deadline runs the same single check.
//! - §2.12 disconnect (`stalled{runtimeDisconnected}`, deadlines paused),
//!   reconnect (same generation: one check per record whose turn ended while
//!   away; another generation: `uncertain{runtimeLost}` when uncheckable),
//!   management restart reconcile (unsettled records, batches of 200, this
//!   background thread, no host fence), the 30 s drift check (ledger vs the
//!   underlying stores only, at most 200 records per pass, logged as
//!   `cutex-input-ledger-drift`), and a lost / corrupt ledger (new
//!   `ledgerEpoch`, unsettled records rebuilt from the stores,
//!   `cutex/stream/reset` on every local session).
//! - The §2.3 deadline loop (sleeps until the next deadline, never polls
//!   history).
//! - §2.6 `SessionDelivery`, computed without the host fence and published as
//!   `cutex/session/deliveryChanged` only when it changed.
//! - §2.14 `cutex/input/retry`.
//!
//! Everything runs on one thread (`cutex-input-settle`) in the management
//! process and only while `management.inputLedger.enabled` is on. It learns
//! about turns and runtimes from the durable event stream (an in-process
//! subscriber, like the SSE streams), never by reading native history on a
//! tick. Locks: the thread holds nothing while it does native IO; the ledger
//! lock stays a leaf (every ledger call is one short statement).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{json, Value};

use super::input_ledger::{
    self as il, Evidence, Expect, InputKind, InputLedger, InputRecord, InputState, LedgerError, Next, Stalled,
    Surface, Transition,
};
use super::user_input::UserInputExecutionError;

/// Records per restart-reconcile / drift / rebuild batch (§2.12).
pub const BATCH: usize = 200;
/// Drift check period (§2.12 "平时的漂移核对").
pub const DRIFT_EVERY: Duration = Duration::from_secs(30);
/// A record younger than this is never "drifted": its writer may be between
/// its own store and the ledger (§2.9 order).
pub const DRIFT_MIN_AGE_SECS: i64 = 30;
/// An owner record left `received` this long with nothing in any store lost
/// its route (§2.3 `received` → `failed{routeLost}`).
pub const ROUTE_LOST_AFTER_SECS: i64 = 60;
/// Retry payloads (owner submit params) kept this long.
pub const PAYLOAD_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Log name of a drift correction.
pub const DRIFT_LOG: &str = "cutex-input-ledger-drift";
/// A record the native runtime holds after an interrupt is checked once
/// more this long after the turn end (054 RC1: settled within 60 s).
pub const INTERRUPTED_RECHECK: Duration = Duration::from_secs(10);
/// A start request (`cutex/runtime/onlineRequested`) shows `starting` at
/// most this long without the runtime connecting (§2.6 order 2).
pub const STARTING_AT_MOST: Duration = Duration::from_secs(120);
/// Waiting reasons that are not "should move now" (§2.2 `stalled`): the
/// record waits for something other than an idle session.
pub const STALL_EXEMPT: &[&str] = &[
    "passive", "agentOffline", "paused", "ownerPause", "stopHold", "reviewPending", "staleHeld", "nativeHeld",
    "nativeRetry", "resultCoalescing", "targetBusy", "runtimeDisconnected",
];

fn rfc3339(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn parse_time(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text).ok().map(|at| at.with_timezone(&Utc))
}

fn after(now: DateTime<Utc>, wait: Duration) -> DateTime<Utc> {
    now + chrono::Duration::from_std(wait).unwrap_or_else(|_| chrono::Duration::seconds(60))
}

fn is_bus(kind: InputKind) -> bool {
    matches!(kind, InputKind::AgentMessage | InputKind::PeerMessage | InputKind::Alarm | InputKind::SystemNotice)
}

fn is_delegation(kind: InputKind) -> bool {
    matches!(
        kind,
        InputKind::DelegationRequest | InputKind::DelegationResult | InputKind::DelegationQuestion | InputKind::DelegationStatus
    )
}

// ---------------------------------------------------------------------------
// Runtime facts and native checks (a trait, so the rules test without a daemon)

/// What the management process knows of a session's runtime (in memory).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeView {
    /// A runtime entry exists for the session's current generation.
    pub present: bool,
    pub connected: bool,
    /// The runtime was stopped on purpose (closing / offline receipt).
    pub stopped: bool,
    pub generation: Option<u64>,
    pub thread_id: Option<String>,
    pub active_turn: Option<String>,
    pub turn_observed_at: Option<String>,
}

/// One §2.10 check result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    /// In the model context (the only upgrade of `uncertain`).
    Persisted(Evidence),
    /// The runtime says it never got it.
    Absent,
    /// Held by the runtime (`details.nativeReason`).
    Held(Option<String>),
    /// Known but not read yet: waiting for a turn (nothing to settle).
    Queued,
    /// Claimed by a turn that ended without persisting it.
    Unpersisted,
    /// `conflict`: another payload under this id.
    Conflict,
    /// Could not ask (transport, no runtime, history unreadable).
    Unavailable,
}

pub trait Runtime {
    fn view(&self, session: &str) -> RuntimeView;
    /// One native request on the session's runtime: `result` or an error text.
    fn request(&self, session: &str, method: &str, params: Value) -> Result<Value, String>;
    /// The frozen ExternalInput key of a bus message (`ownerId`, `threadId`,
    /// `messages: [key]`), or `None` for a legacy local-inbox message.
    fn bus_external_key(&self, message_id: &str) -> Option<(String, String, Value)>;
}

/// `thread/externalInput/status` → [`Check`] (Codex native and Claude r8).
pub fn parse_external_status(response: &Value) -> Check {
    let Some(status) = response.pointer("/statuses/0") else { return Check::Unavailable };
    let processing = status.pointer("/processing/state").and_then(Value::as_str).unwrap_or("none");
    let reason = status.pointer("/processing/reason").and_then(Value::as_str).map(str::to_string);
    match status.get("deliveryState").and_then(Value::as_str).unwrap_or("") {
        "context_persisted" => Check::Persisted(Evidence::ExternalInputPersisted),
        "unknown" => Check::Absent,
        "conflict" => Check::Conflict,
        "retryable_error" => Check::Held(Some("retryableError".into())),
        "pending" => match processing {
            "held" => Check::Held(reason),
            "claimed" | "output_observed" => Check::Unpersisted,
            _ => Check::Queued,
        },
        _ => Check::Unavailable,
    }
}

/// Claude adapter `claude/input/status` (local inbox receipt) → [`Check`].
pub fn parse_local_status(response: &Value) -> Check {
    if response.get("contextPersisted") == Some(&Value::Bool(true)) {
        return Check::Persisted(Evidence::ContextPersisted);
    }
    match response.get("state").and_then(Value::as_str).unwrap_or("") {
        "interrupted" | "failed" => Check::Held(response["state"].as_str().map(str::to_string)),
        "processing" => Check::Unpersisted,
        "queued" | "pending" => Check::Queued,
        _ => Check::Unavailable,
    }
}

/// One page of `thread/items/list` of the turn → whether it holds the
/// owner's `userMessage` with this clientId (Codex and Claude both project
/// steered owner input as such an item).
pub fn parse_turn_items(response: &Value, client_id: &str) -> Check {
    parse_turn_items_bound(response, client_id).0
}

/// [`parse_turn_items`] and the id of the `userMessage` item found (the
/// owner record's native timeline item, 054 §3.2).
pub fn parse_turn_items_bound(response: &Value, client_id: &str) -> (Check, Option<String>) {
    let Some(rows) = response.get("data").and_then(Value::as_array) else { return (Check::Unavailable, None) };
    let found = rows.iter().map(|row| row.get("item").unwrap_or(row)).find(|item| {
        item.get("type").and_then(Value::as_str) == Some("userMessage")
            && (item.get("clientId").and_then(Value::as_str) == Some(client_id)
                || item.get("clientUserMessageId").and_then(Value::as_str) == Some(client_id))
    });
    match found {
        Some(item) => (
            Check::Persisted(Evidence::HistoryRead),
            item.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()).map(str::to_string),
        ),
        None => (Check::Absent, None),
    }
}

/// The single §2.10 check of one record (one native request at most).
pub fn check(runtime: &dyn Runtime, record: &InputRecord, turn: Option<&str>) -> Check {
    check_bound(runtime, record, turn).0
}

/// [`check`] and, when it proves the record persisted, the native timeline
/// item it is (054 §3.2): the owner's `userMessage` item the turn read
/// found; for a bus message its message id (ExternalInput items and the
/// Claude adapter's `external_event` items carry the message id).
pub fn check_bound(runtime: &dyn Runtime, record: &InputRecord, turn: Option<&str>) -> (Check, Option<String>) {
    let (check, item, _) = check_full(runtime, record, turn);
    (check, item)
}

/// Include the carrying turn only when the native receipt supplies it.
fn check_full(runtime: &dyn Runtime, record: &InputRecord, turn: Option<&str>) -> (Check, Option<String>, Option<String>) {
    let session = &record.cutex_session_id;
    let view = runtime.view(session);
    if !view.connected { return (Check::Unavailable, None, None); }
    if record.kind == InputKind::Owner {
        let (Some(thread), Some(turn)) = (view.thread_id.as_deref(), turn.or(record.turn_id.as_deref())) else {
            return (Check::Unavailable, None, None);
        };
        let params = json!({"threadId": thread, "turnId": turn, "limit": 200, "sortDirection": "asc"});
        let (checked, item) = match runtime.request(session, "thread/items/list", params) {
            Ok(response) => parse_turn_items_bound(&response, &record.input_id),
            Err(_) => (Check::Unavailable, None),
        };
        return (checked, item, None);
    }
    check_unbound(runtime, record, &view)
}

/// `next` bound to the native item a check found (only an `inContext` one).
pub fn bind_checked(next: Option<Next>, item: Option<String>) -> Option<Next> {
    match (next, item) {
        (Some(next), Some(item)) if next.state == InputState::InContext => Some(next.timeline_item(item)),
        (next, _) => next,
    }
}

fn bind_receipt_turn(next: Option<Next>, turn: Option<String>) -> Option<Next> {
    next.map(|next| match turn {
        Some(turn) if next.state == InputState::InContext => next.turn(turn),
        _ => next,
    })
}

/// [`check_bound`] of a record that is not the owner's.
fn check_unbound(runtime: &dyn Runtime, record: &InputRecord, view: &RuntimeView) -> (Check, Option<String>, Option<String>) {
    let session = &record.cutex_session_id;
    if is_bus(record.kind) {
        return match runtime.bus_external_key(&record.input_id) {
            Some((owner, thread, key)) => {
                let params = json!({"version": 1, "ownerId": owner, "threadId": thread,
                    "runtimeGeneration": view.generation, "messages": [key]});
                match runtime.request(session, "thread/externalInput/status", params) {
                    Ok(response) => {
                        let checked = parse_external_status(&response);
                        let turn = matches!(checked, Check::Persisted(_)).then(|| response.pointer("/statuses/0/receipt/turnId")
                            .and_then(Value::as_str).filter(|id| !id.is_empty()).map(str::to_string)).flatten();
                        // The queried native key can differ from the stable
                        // ledger id after explicit retry. Bind that exact item.
                        let item = matches!(checked, Check::Persisted(_)).then(|| key["messageId"].as_str()
                            .filter(|id| !id.is_empty()).map(str::to_string)).flatten();
                        (checked, item, turn)
                    }
                    Err(_) => (Check::Unavailable, None, None),
                }
            }
            None => {
                let Some(thread) = view.thread_id.as_deref() else { return (Check::Unavailable, None, None) };
                match runtime.request(session, "claude/input/status", json!({"threadId": thread, "messageId": record.input_id})) {
                    Ok(response) => {
                        let checked = parse_local_status(&response);
                        let turn = matches!(checked, Check::Persisted(_)).then(|| response.get("turnId")
                            .and_then(Value::as_str).filter(|id| !id.is_empty()).map(str::to_string)).flatten();
                        let item = matches!(checked, Check::Persisted(_)).then(|| record.input_id.clone());
                        (checked, item, turn)
                    }
                    Err(error) if error.contains("-32004") || error.contains("not found") => (Check::Absent, None, None),
                    Err(_) => (Check::Unavailable, None, None),
                }
            }
        };
    }
    (Check::Unavailable, None, None)
}

/// Why a check runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Occasion {
    /// The carrying turn ended `interrupted`.
    TurnInterrupted,
    /// The carrying turn ended `failed`.
    TurnFailed,
    /// The receipt grace after the turn ended passed (§2.3 accepted deadline).
    ReceiptDeadline,
    /// The runtime came back with another generation (§2.12).
    GenerationChanged,
}

/// §2.10 table: what an `accepted` record becomes after its one check.
/// `None`: leave it (no evidence either way; a deadline may follow).
///
/// Contract reading (054 invariant 8 vs §2.10): `accepted` never goes back
/// to `waiting`, so an explicit Absent of a non-owner record that was
/// already accepted is `uncertain` (the owner resends), not `waiting{returned}`.
pub fn settle_next(kind: InputKind, occasion: Occasion, check: &Check) -> Option<Next> {
    let owner = kind == InputKind::Owner;
    let unproven = match occasion {
        Occasion::TurnInterrupted => "turnInterrupted",
        Occasion::TurnFailed => "turnFailed",
        Occasion::ReceiptDeadline => "receiptTimeout",
        Occasion::GenerationChanged => "runtimeLost",
    };
    match check {
        Check::Persisted(evidence) => Some(Next::in_context(*evidence)),
        Check::Absent if owner => Some(Next::failed("notDelivered")),
        Check::Absent => Some(Next::uncertain(unproven)),
        Check::Held(reason) => {
            let next = Next::uncertain(unproven);
            Some(match reason {
                Some(reason) => next.details(json!({"nativeReason": reason})),
                None => next,
            })
        }
        Check::Conflict => Some(Next::uncertain("nativeConflict")),
        Check::Unpersisted => Some(Next::uncertain(unproven)),
        // Not read by any turn yet (a note waiting for the next one).
        Check::Queued if occasion == Occasion::ReceiptDeadline || occasion == Occasion::GenerationChanged => {
            (occasion == Occasion::GenerationChanged).then(|| Next::uncertain(unproven))
        }
        Check::Queued => None,
        // Nothing could be asked: the deadline decides (receiptTimeout), a
        // lost generation is final.
        Check::Unavailable => match occasion {
            Occasion::ReceiptDeadline | Occasion::GenerationChanged => Some(Next::uncertain(unproven)),
            Occasion::TurnInterrupted | Occasion::TurnFailed => None,
        },
    }
}

// ---------------------------------------------------------------------------
// SessionDelivery (§2.6)

/// Everything §2.6 decides from, read without the host fence.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeliveryFacts {
    pub runtime: RuntimeView,
    pub record_generation: Option<u64>,
    /// Held owner inputs: their deadlines (`heldUntil` is the first).
    pub held_deadlines: Vec<String>,
    /// The queue pause after `cutex/turn/stop` (or a manual queue pause).
    pub stop_hold: bool,
    /// Agent / project pause.
    pub paused: bool,
    /// The runtime reported its turn is finishing. No runtime reports it:
    /// Codex has no such notification and the Claude adapter (r34) does not
    /// send its `accepting=False` session state, so `finishing` is never
    /// emitted (§2.6 order 7, "做不到就不发").
    pub finishing: bool,
    /// A start was requested (`cutex/runtime/onlineRequested`) and the
    /// runtime has not connected yet (§2.6 order 2).
    pub starting: bool,
    /// `lease` (asks `lease_view`).
    pub lease: Option<Value>,
    /// Kind of the running turn and its inputs (from the lease / the ledger).
    pub turn_kind: Option<String>,
    pub turn_ask_id: Option<String>,
    pub turn_inputs: Vec<String>,
}

/// §2.6 decision order 1–10. `stateRevision` is filled in by the ledger.
pub fn session_delivery(facts: &DeliveryFacts, now: DateTime<Utc>) -> Value {
    let runtime = &facts.runtime;
    let lease = facts.lease.clone().unwrap_or(Value::Null);
    let lease_holder = lease.get("holder").and_then(Value::as_str);
    let turn = || {
        runtime.active_turn.as_ref().map(|turn| {
            let lease_turn = lease.get("turnId").and_then(Value::as_str) == Some(turn.as_str());
            let (kind, ask_id) = match (lease_turn, lease_holder) {
                (true, Some("ask")) => ("ask".to_string(), lease.pointer("/inputIds/0").cloned().unwrap_or(Value::Null)),
                (true, Some("event")) => ("delivery".to_string(), Value::Null),
                (true, Some("owner")) => ("owner".to_string(), Value::Null),
                _ => (
                    facts.turn_kind.clone().unwrap_or_else(|| "unknown".into()),
                    facts.turn_ask_id.clone().map_or(Value::Null, Value::from),
                ),
            };
            json!({"turnId": turn, "kind": kind, "askId": ask_id,
                "startedAt": runtime.turn_observed_at, "inputIds": facts.turn_inputs})
        })
    };
    let (state, reason, turn) = if facts.starting && !runtime.connected {
        // 2: launching (checked before 1: a runtime being started has no
        // entry yet).
        ("starting", None, None)
    } else if !runtime.present || runtime.stopped {
        ("offline", None, None)
    } else if !runtime.connected {
        // 3: a lost observation cannot prove a running turn (never `turn`).
        ("disconnected", None, None)
    } else if !facts.held_deadlines.is_empty() {
        ("held", Some("hostBusy"), turn())
    } else if facts.stop_hold {
        ("held", Some("stopHold"), turn())
    } else if facts.paused {
        ("held", Some("paused"), turn())
    } else if facts.finishing {
        ("finishing", None, turn())
    } else if runtime.active_turn.is_some() {
        ("turn", None, turn())
    } else if lease_holder == Some("ask") {
        ("held", Some("delegationLease"), None)
    } else if lease_holder == Some("event") {
        ("held", Some("eventLease"), None)
    } else {
        ("idle", None, None)
    };
    json!({
        "state": state,
        "reason": reason,
        "turn": turn,
        "lease": lease,
        "runtimeConnected": runtime.connected,
        "runtimeGeneration": runtime.generation.or(facts.record_generation),
        "heldCount": facts.held_deadlines.len(),
        "heldUntil": facts.held_deadlines.first(),
        "stateRevision": 0,
        "observedAt": rfc3339(now),
    })
}

/// §2.2 `stalled{noProgress}`: a `waiting` record that should move (its
/// reason is not one of [`STALL_EXEMPT`]) while its session is `idle`, and
/// did not change for `stall_after`, is marked; the mark goes when the
/// session is no longer idle (a state change clears it anyway). Not a
/// [`il::stranded_resend`]: no turn can move it, its mark and actions stay.
/// `since` is when the wait began. Returns the transition to write, or `None`.
pub fn stall_next(record: &InputRecord, idle: bool, now: DateTime<Utc>, stall_after: Duration) -> Option<Next> {
    if record.state != InputState::Waiting {
        return None;
    }
    let reason = record.reason.as_deref().unwrap_or("afterTurn");
    let marked = record.stalled.as_ref().is_some_and(|stalled| stalled.reason == "noProgress");
    let eligible = idle && !STALL_EXEMPT.contains(&reason);
    if marked && !eligible && !il::stranded_resend(record) {
        return Some(Next::waiting(reason).stalled(None));
    }
    if !eligible || record.stalled.is_some() {
        return None;
    }
    let quiet = parse_time(&record.updated_at)
        .is_some_and(|at| now - at >= chrono::Duration::from_std(stall_after).unwrap_or_default());
    quiet.then(|| Next::waiting(reason).stalled(Some(Stalled { reason: "noProgress".into(), since: record.since.clone() })))
}

/// When [`stall_next`] would next mark one of these records (`None`: none
/// is eligible).
pub fn next_stall_check(records: &[InputRecord], idle: bool, stall_after: Duration) -> Option<DateTime<Utc>> {
    if !idle {
        return None;
    }
    records
        .iter()
        .filter(|record| record.state == InputState::Waiting && record.stalled.is_none())
        .filter(|record| !STALL_EXEMPT.contains(&record.reason.as_deref().unwrap_or("afterTurn")))
        .filter_map(|record| parse_time(&record.updated_at))
        .map(|at| after(at, stall_after))
        .min()
}

/// r274.2 §9.4 action gaps (Director 2026-11-03, no auto-redelivery): why an
/// owner `accepted` record will not progress by itself, from facts only
/// (no native call), or `None`:
/// - `generationChanged`: it was accepted by runtime generation `g` and the
///   connected runtime is a later one (a new runtime never reads an earlier
///   one's pending steer; the reconnect check did not settle it because the
///   change was not watched, e.g. across a management restart);
/// - `leaseStale`: the session's admission lease is `stale` (its reconcile
///   could not prove the turn ended) and holds this record's turn or names
///   the record: only the owner's lease release moves it;
/// - `orphan`: connected, no turn is running, no deadline will check it
///   (no receipt grace was started: the end of its turn was never seen), it
///   is not in the Management queue, the agent was not shown it (not
///   `offered`, no echo), and it has not changed for `settle_after`.
///
/// Never a record the agent may read next: a running turn, a pending
/// deadline (its check settles it), a queued item, or an `offered` one are
/// all excluded.
pub fn owner_stuck_case(
    record: &InputRecord,
    view: &RuntimeView,
    lease: Option<&Value>,
    queued: bool,
    now: DateTime<Utc>,
    settle_after: Duration,
) -> Option<&'static str> {
    if record.kind != InputKind::Owner || record.state != InputState::Accepted {
        return None;
    }
    if record.stalled.as_ref().is_some_and(|stalled| stalled.reason == "runtimeDisconnected") {
        return None;
    }
    if view.connected && record.runtime_generation.zip(view.generation).is_some_and(|(accepted, current)| accepted < current) {
        return Some("generationChanged");
    }
    if let Some(lease) = lease.filter(|lease| lease.pointer("/reconcile/state").and_then(Value::as_str) == Some("stale")) {
        let turn = lease.get("turnId").and_then(Value::as_str);
        let holds_turn = turn.is_some() && turn == record.turn_id.as_deref();
        let names = lease.get("inputIds").and_then(Value::as_array).is_some_and(|ids| ids.iter().any(|id| id == record.input_id.as_str()));
        if holds_turn || names {
            return Some("leaseStale");
        }
    }
    let quiet = parse_time(&record.since)
        .is_some_and(|since| now - since >= chrono::Duration::from_std(settle_after).unwrap_or_default());
    let orphan = view.connected
        && view.active_turn.is_none()
        && record.deadline.is_none()
        && !queued
        && !il::shown_to_agent(record)
        && quiet;
    orphan.then_some("orphan")
}

/// The write that marks (or unmarks) `record` per [`owner_stuck_case`]:
/// `stalled{noProgress}` with `details.stuck` (then `il::owner_stuck` gives
/// it `[retry, dismiss]`), or the mark cleared once the case no longer
/// holds. `None`: nothing to change.
pub fn owner_stuck_next(record: &InputRecord, case: Option<&'static str>, now: DateTime<Utc>) -> Option<Next> {
    if record.kind != InputKind::Owner || record.state != InputState::Accepted {
        return None;
    }
    let marked = record.stalled.as_ref().is_some_and(|stalled| stalled.reason == "noProgress");
    let current = il::stuck_case(record).filter(|_| marked);
    if current == case {
        return None;
    }
    let mut details = if record.details.is_object() { record.details.clone() } else { json!({}) };
    match case {
        Some(case) => {
            details[il::STUCK] = json!(case);
            Some(Next::accepted().stalled(Some(Stalled { reason: "noProgress".into(), since: rfc3339(now) })).details(details))
        }
        None if marked => {
            if let Some(object) = details.as_object_mut() {
                object.remove(il::STUCK);
            }
            Some(Next::accepted().stalled(None).details(details))
        }
        None => None,
    }
}

/// When an `accepted` owner record of `records` may next become an
/// `orphan` (its quiet period ends), for the engine's one timer per session.
pub fn next_orphan_check(records: &[InputRecord], now: DateTime<Utc>, settle_after: Duration) -> Option<DateTime<Utc>> {
    records
        .iter()
        .filter(|record| record.kind == InputKind::Owner && record.state == InputState::Accepted)
        .filter(|record| record.stalled.is_none() && record.deadline.is_none() && !il::shown_to_agent(record))
        .filter_map(|record| parse_time(&record.since))
        .map(|at| after(at, settle_after))
        .filter(|at| *at > now)
        .min()
}

// ---------------------------------------------------------------------------
// Ledger helpers

fn unsettled(ledger: &InputLedger, session: &str) -> Vec<InputRecord> {
    match ledger.list(session, il::ListQuery { since_revision: None, include_settled_secs: 0 }) {
        Ok(list) => list.records.into_iter().filter(|record| !record.state.is_settled()).collect(),
        Err(error) => {
            eprintln!("cutex input ledger (settle): list {session}: {error}");
            Vec::new()
        }
    }
}

/// CAS on the observed rev; a record that moved meanwhile is left alone.
fn apply(ledger: &InputLedger, record: &InputRecord, next: Next) -> Option<Transition> {
    match ledger.transition(&record.cutex_session_id, &record.input_id, Expect::rev(record.rev), next) {
        Ok(transition) => {
            if let Err(error) = il::publish(&transition) {
                eprintln!("cutex input ledger (settle): publish {}/{}: {error:#}", record.cutex_session_id, record.input_id);
            }
            Some(transition)
        }
        Err(LedgerError::Conflict { .. }) => None,
        Err(error) => {
            eprintln!("cutex input ledger (settle): {}/{}: {error}", record.cutex_session_id, record.input_id);
            None
        }
    }
}

/// §2.10 for the records of `session` that turn `turn` carried.
pub fn turn_ended(ledger: &InputLedger, runtime: &dyn Runtime, session: &str, turn: &str, status: &str, now: DateTime<Utc>) -> usize {
    let occasion = match status {
        "interrupted" => Some(Occasion::TurnInterrupted),
        "failed" => Some(Occasion::TurnFailed),
        _ => None,
    };
    let mut changed = 0;
    for record in unsettled(ledger, session) {
        if record.state != InputState::Accepted || is_delegation(record.kind) {
            continue;
        }
        let carried = record.turn_id.as_deref() == Some(turn);
        match occasion {
            Some(occasion) => {
                // A record without a turn may have been claimed by this one
                // (a note read at a step): its one check tells.
                if !carried && record.turn_id.is_some() {
                    continue;
                }
                let (result, item, receipt_turn) = check_full(runtime, &record, Some(turn));
                // A late terminal event is not proof that an unbound bus
                // input belongs to that old turn. Its current native claim
                // may be in the different turn that is still running.
                if !carried && is_bus(record.kind) && result == Check::Unpersisted
                    && runtime.view(session).active_turn.as_deref().is_some_and(|active| active != turn) {
                    continue;
                }
                // Held for another reason (plan mode, size policy) was never
                // in this turn: the bus projection keeps that record.
                if !carried && matches!(&result, Check::Held(reason) if reason.as_deref() != Some("interrupted")) {
                    continue;
                }
                let interrupted_hold = occasion == Occasion::TurnInterrupted
                    && matches!(&result, Check::Held(reason) if reason.as_deref() == Some("interrupted"));
                let next = match settle_next(record.kind, occasion, &result) {
                    // Held by the interrupt: a runtime that retries it on its
                    // own (Codex ExternalInput) persists it shortly; one more
                    // check after INTERRUPTED_RECHECK decides (RC1).
                    Some(_) if interrupted_hold => Some(
                        Next::accepted().turn(turn).deadline(Some(after(now, INTERRUPTED_RECHECK))),
                    ),
                    Some(next) => bind_receipt_turn(bind_checked(Some(next), item), receipt_turn.clone()),
                    // Unreachable now: the receipt deadline decides.
                    None if carried && result == Check::Unavailable => Some(
                        Next::accepted().deadline(Some(after(now, ledger.deadlines().receipt_grace))),
                    ),
                    None => None,
                };
                if let Some(mut next) = next {
                    // The runtime took it into this turn (a note claimed at a
                    // step): it was read in, so it belongs to the timeline.
                    let taken = matches!(result, Check::Persisted(_) | Check::Held(_) | Check::Unpersisted);
                    if !carried && taken && next.state != InputState::Accepted {
                        next = next.turn(receipt_turn.as_deref().unwrap_or(turn));
                        // An owner record moves by the ledger rule (only once
                        // read, r274.2): an uncertain one keeps its surface.
                        if record.kind != InputKind::Owner && matches!(record.surface, Surface::NextUp | Surface::PendingNotes) {
                            next = next.surface(Surface::Timeline);
                        }
                    }
                    changed += usize::from(apply(ledger, &record, next).is_some_and(|t| t.changed));
                }
            }
            None => {
                // Completed: the receipt grace starts (§2.3 accepted).
                let owner_without_turn = record.kind == InputKind::Owner && record.turn_id.is_none();
                if (carried || owner_without_turn) && record.deadline.is_none() {
                    let mut next = Next::accepted().deadline(Some(after(now, ledger.deadlines().receipt_grace)));
                    if owner_without_turn {
                        next = next.turn(turn);
                    }
                    changed += usize::from(apply(ledger, &record, next).is_some_and(|t| t.changed));
                }
            }
        }
    }
    changed
}

/// §2.12 disconnect: records keep their state, deadlines pause,
/// `stalled{runtimeDisconnected}`.
pub fn disconnected(ledger: &InputLedger, session: &str, now: DateTime<Utc>) -> usize {
    let stalled = Stalled { reason: "runtimeDisconnected".into(), since: rfc3339(now) };
    let mut changed = 0;
    for record in unsettled(ledger, session) {
        if !matches!(record.state, InputState::Sending | InputState::Accepted)
            || record.stalled.as_ref().is_some_and(|s| s.reason == "runtimeDisconnected")
        {
            continue;
        }
        let next = Next::to(record.state).stalled(Some(stalled.clone())).deadline(None);
        changed += usize::from(apply(ledger, &record, next).is_some_and(|t| t.changed));
    }
    changed
}

/// §2.12 reconnect: ONE check per record whose turn ended while away (same
/// generation), `uncertain{runtimeLost}` for what another generation cannot
/// prove; deadlines restart now.
pub fn reconnected(ledger: &InputLedger, runtime: &dyn Runtime, session: &str, generation_changed: bool, now: DateTime<Utc>) -> usize {
    let view = runtime.view(session);
    let mut changed = 0;
    for record in unsettled(ledger, session) {
        if !matches!(record.state, InputState::Sending | InputState::Accepted) {
            continue;
        }
        let was_stalled = record.stalled.as_ref().is_some_and(|s| s.reason == "runtimeDisconnected");
        let next = match record.state {
            InputState::Sending if generation_changed => Some(Next::uncertain("runtimeLost")),
            InputState::Sending if was_stalled => {
                Some(Next::sending().stalled(None).deadline(Some(after(now, ledger.deadlines().sending))))
            }
            InputState::Accepted if is_delegation(record.kind) => was_stalled.then(|| Next::accepted().stalled(None)),
            InputState::Accepted if generation_changed => {
                let (result, item, carrying_turn) = check_full(runtime, &record, None);
                bind_receipt_turn(bind_checked(settle_next(record.kind, Occasion::GenerationChanged, &result), item), carrying_turn)
            }
            InputState::Accepted => {
                let running = record.turn_id.is_some() && view.active_turn == record.turn_id;
                if running {
                    was_stalled.then(|| Next::accepted().stalled(None))
                } else {
                    // The turn ended while nobody watched: one check now; no
                    // answer yet → the receipt grace restarts now.
                    match check_full(runtime, &record, None) {
                        (result @ (Check::Persisted(_) | Check::Absent | Check::Held(_) | Check::Conflict | Check::Unpersisted), item, carrying_turn) => {
                            bind_receipt_turn(bind_checked(settle_next(record.kind, Occasion::ReceiptDeadline, &result), item), carrying_turn)
                        }
                        (Check::Queued, _, _) if record.turn_id.is_none() => {
                            // A native pending item still waits for its first
                            // carrying turn. Admission alone is not a receipt.
                            was_stalled.then(|| Next::accepted().stalled(None))
                        }
                        (Check::Queued | Check::Unavailable, _, _) => {
                            Some(Next::accepted().stalled(None).deadline(Some(after(now, ledger.deadlines().receipt_grace))))
                        }
                    }
                }
            }
            _ => None,
        };
        if let Some(next) = next {
            changed += usize::from(apply(ledger, &record, next).is_some_and(|t| t.changed));
        }
    }
    changed
}

/// One deadline decision (§2.3 "after the deadline"), reading only the
/// record and, for the owner, its own stores; `accepted` runs the one check.
pub fn deadline_decision(
    record: &InputRecord,
    runtime: &dyn Runtime,
    stores: &dyn OwnerStores,
    now: DateTime<Utc>,
) -> Option<Next> {
    let owner = record.kind == InputKind::Owner;
    let age = parse_time(&record.since).map_or(i64::MAX, |since| (now - since).num_seconds());
    match record.state {
        InputState::Received if owner => {
            if age < ROUTE_LOST_AFTER_SECS {
                Some(Next::received().deadline(Some(now + chrono::Duration::seconds(15))))
            } else if stores.known(&record.cutex_session_id, &record.input_id) {
                Some(Next::received().deadline(None))
            } else {
                Some(Next::failed("routeLost"))
            }
        }
        // The held worker owns its timeout (failed{admission_timeout}); only a
        // held record nothing holds any more expires here.
        InputState::Held if owner && stores.held(&record.cutex_session_id, &record.input_id) => {
            Some(Next::held("hostBusy").deadline(Some(now + chrono::Duration::seconds(5))))
        }
        InputState::Sending if owner => match stores.native_delivery(&record.cutex_session_id, &record.input_id) {
            OwnerDelivery::Sending => Some(Next::sending().deadline(Some(after(now, Duration::from_secs(30))))),
            OwnerDelivery::Accepted(turn) => Some(match turn {
                Some(turn) => Next::accepted().turn(turn),
                None => Next::accepted(),
            }),
            OwnerDelivery::Unknown | OwnerDelivery::None => Some(Next::uncertain("outcomeUnknown")),
        },
        InputState::Accepted => {
            let (result, item, carrying_turn) = check_full(runtime, record, None);
            if is_bus(record.kind) && record.turn_id.is_none() && result == Check::Queued {
                // A pending native queue is positive evidence of waiting.
                // Retire this one-shot check instead of polling an expired deadline.
                return Some(Next::accepted().deadline(None));
            }
            // Still held by an interrupt (turn_ended re-checks once).
            let occasion = match &result {
                Check::Held(reason) if reason.as_deref() == Some("interrupted") => Occasion::TurnInterrupted,
                _ => Occasion::ReceiptDeadline,
            };
            bind_receipt_turn(bind_checked(settle_next(record.kind, occasion, &result), item), carrying_turn)
        }
        _ => il::deadline_next(record),
    }
}

// ---------------------------------------------------------------------------
// Owner stores (C-010 delivery, queue, held), read without the fence

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerDelivery {
    None,
    Sending,
    Accepted(Option<String>),
    Unknown,
}

pub trait OwnerStores {
    fn queued(&self, session: &str, id: &str) -> bool;
    fn held(&self, session: &str, id: &str) -> bool;
    fn native_delivery(&self, session: &str, id: &str) -> OwnerDelivery;
    fn known(&self, session: &str, id: &str) -> bool {
        self.queued(session, id) || self.held(session, id) || self.native_delivery(session, id) != OwnerDelivery::None
    }
}

struct LiveOwnerStores;

impl OwnerStores for LiveOwnerStores {
    fn queued(&self, session: &str, id: &str) -> bool {
        super::user_input::user_input_repository()
            .and_then(|repository| repository.list(session))
            .is_ok_and(|(_, items)| items.iter().any(|item| item.client_user_message_id == id))
    }
    fn held(&self, session: &str, id: &str) -> bool {
        super::owner_held::find(session, id).ok().flatten().is_some()
    }
    fn native_delivery(&self, session: &str, id: &str) -> OwnerDelivery {
        use super::user_input::InputDelivery;
        match super::user_input::user_input_repository().and_then(|repository| repository.delivery(session, id)) {
            Ok(Some(InputDelivery::Sending { .. })) => OwnerDelivery::Sending,
            Ok(Some(InputDelivery::Accepted { turn_id, .. })) => OwnerDelivery::Accepted(turn_id),
            Ok(Some(InputDelivery::Unknown { .. })) => OwnerDelivery::Unknown,
            Ok(None) | Err(_) => OwnerDelivery::None,
        }
    }
}

/// Drift (§2.12): what the owner stores say an owner record should be when
/// the ledger missed it. Only records older than [`DRIFT_MIN_AGE_SECS`].
pub fn owner_drift(record: &InputRecord, stores: &dyn OwnerStores, now: DateTime<Utc>) -> Vec<Next> {
    let age = parse_time(&record.updated_at).map_or(0, |at| (now - at).num_seconds());
    if age < DRIFT_MIN_AGE_SECS {
        return Vec::new();
    }
    let (session, id) = (&record.cutex_session_id, &record.input_id);
    match (record.state, stores.native_delivery(session, id)) {
        (InputState::Received | InputState::Held | InputState::Waiting, OwnerDelivery::Accepted(turn)) => {
            let accepted = match &turn {
                Some(turn) => Next::accepted().turn(turn.clone()),
                None => Next::accepted(),
            };
            vec![Next::sending(), accepted]
        }
        (InputState::Waiting, OwnerDelivery::None)
            if age >= ROUTE_LOST_AFTER_SECS && !stores.queued(session, id) && !stores.held(session, id)
                && record.reason.as_deref() == Some("explicitQueue") =>
        {
            vec![Next::discarded("ownerRemoved")]
        }
        (InputState::Held, OwnerDelivery::None) if !stores.held(session, id) && !stores.queued(session, id) => {
            vec![Next::failed("admission_timeout")]
        }
        _ => Vec::new(),
    }
}

/// What the §2.12 restart reconcile does with one unsettled record.
#[derive(Debug, Clone, PartialEq)]
pub enum Restart {
    /// Not a leftover of an earlier process.
    Skip,
    /// `accepted`: one §2.10 check once the runtime is there.
    ReconnectCheck,
    Apply(Next),
    Drift,
}

/// §2.12 "管理服务重启": the records left unsettled by an earlier process.
/// A record last written at or after `process_started` belongs to this
/// process: its writer is alive (an owner `sending` may be waiting for its
/// native reply) and its own deadline settles it, so it is skipped; judging
/// it here turned a live steer into `uncertain{outcomeUnknown}` when the
/// lazily started engine reached its first page after the send began.
pub fn restart_next(record: &InputRecord, stores: &dyn OwnerStores, process_started: DateTime<Utc>) -> Restart {
    if parse_time(&record.updated_at).is_some_and(|at| at >= process_started) {
        return Restart::Skip;
    }
    match record.state {
        InputState::Accepted => Restart::ReconnectCheck,
        InputState::Sending if record.kind == InputKind::Owner => {
            Restart::Apply(match stores.native_delivery(&record.cutex_session_id, &record.input_id) {
                OwnerDelivery::Accepted(Some(turn)) => Next::accepted().turn(turn),
                OwnerDelivery::Accepted(None) => Next::accepted(),
                // In flight in a process that died, or unknown: C-010.
                OwnerDelivery::Sending | OwnerDelivery::Unknown | OwnerDelivery::None => Next::uncertain("outcomeUnknown"),
            })
        }
        _ => Restart::Drift,
    }
}

// ---------------------------------------------------------------------------
// Process hooks

pub type StatusLoader = fn(&str) -> anyhow::Result<Option<crate::app_server::manager::AppServerManagedRuntimeStatus>>;
pub type NativeForward =
    fn(&str, Value) -> Result<Value, crate::management::server::ManagementNativeForwardError>;

#[derive(Clone, Copy)]
struct Hooks {
    status: StatusLoader,
    forward: NativeForward,
}

static HOOKS: OnceLock<Hooks> = OnceLock::new();

struct Signal {
    dirty: BTreeSet<String>,
}

static SIGNAL: Mutex<Signal> = Mutex::new(Signal { dirty: BTreeSet::new() });
static WAKE: Condvar = Condvar::new();

/// The session's delivery may have changed (lease release, lease check,
/// queue changes): recompute it soon. Cheap; safe under any lock.
pub fn mark_dirty(session: &str) {
    if HOOKS.get().is_none() || !il::enabled() {
        return;
    }
    let mut signal = SIGNAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if signal.dirty.insert(session.to_string()) {
        WAKE.notify_one();
    }
}

fn take_dirty() -> BTreeSet<String> {
    std::mem::take(&mut SIGNAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner).dirty)
}

/// Install the management process hooks and start `cutex-input-settle`
/// (once). The thread idles while the switch is off.
pub fn ensure_started(status: StatusLoader, forward: NativeForward) {
    super::input_disposal::ensure_started();
    if HOOKS.get().is_some() {
        return;
    }
    if HOOKS.set(Hooks { status, forward }).is_err() {
        return;
    }
    if let Err(error) = std::thread::Builder::new().name("cutex-input-settle".into()).spawn(run) {
        eprintln!("cutex input ledger (settle): thread not started: {error}");
    }
}

struct LiveRuntime(Hooks);

fn session_record(session: &str) -> Option<(u64, String)> {
    let store = crate::session::store::load_cutex_session_store_shared().ok()?;
    let record = store.sessions.get(session)?;
    Some((record.runtime_generation, record.app_server_thread_id().unwrap_or_default().to_string()))
}

impl Runtime for LiveRuntime {
    fn view(&self, session: &str) -> RuntimeView {
        let record = session_record(session);
        let status = (self.0.status)(session).ok().flatten()
            .filter(|status| record.as_ref().is_none_or(|(generation, _)| status.runtime_generation == *generation));
        let stopped = record.as_ref().is_some_and(|(generation, _)| {
            status.as_ref().is_none_or(|status| !status.connected)
                && matches!(
                    crate::session::runtime_stop::state(session, *generation),
                    Some(crate::session::runtime_stop::StopState::Closing | crate::session::runtime_stop::StopState::Offline)
                )
        });
        match status {
            Some(status) => RuntimeView {
                present: true,
                connected: status.connected,
                stopped,
                generation: Some(status.runtime_generation),
                thread_id: Some(status.thread_id.clone()),
                active_turn: status.active_turn_id.clone().filter(|_| status.connected),
                turn_observed_at: status.active_turn_observed_at.clone().filter(|_| status.connected),
            },
            None => RuntimeView {
                present: false,
                stopped,
                generation: record.as_ref().map(|(generation, _)| *generation),
                thread_id: record.map(|(_, thread)| thread).filter(|thread| !thread.is_empty()),
                ..RuntimeView::default()
            },
        }
    }

    fn request(&self, session: &str, method: &str, params: Value) -> Result<Value, String> {
        let message = json!({"jsonrpc": "2.0", "id": format!("cutex-settle-{}", uuid::Uuid::new_v4().simple()),
            "method": method, "params": params});
        match (self.0.forward)(session, message) {
            Ok(response) => match response.get("error") {
                Some(error) => Err(format!("{method}: {error}")),
                None => Ok(response.get("result").cloned().unwrap_or(Value::Null)),
            },
            Err(error) => Err(format!("{method}: {error:?}")),
        }
    }

    fn bus_external_key(&self, message_id: &str) -> Option<(String, String, Value)> {
        let snapshot = super::agent_bus_state::agent_bus_message_repository().ok()?.snapshot_by_message_id(message_id).ok()??;
        let envelope = snapshot.external_input?;
        let key = serde_json::to_value(envelope.key()).ok()?;
        Some((envelope.owner_id.clone(), envelope.thread_id.clone(), key))
    }
}

// ---------------------------------------------------------------------------
// The thread

struct Watch {
    generation: Option<u64>,
}

struct Engine {
    runtime: LiveRuntime,
    subscription: Option<super::repository::EventSubscription>,
    /// Sessions seen disconnected (polled until they come back).
    watched: BTreeMap<String, Watch>,
    /// Sessions with a published delivery (refreshed every drift pass).
    known: BTreeSet<String>,
    /// Sessions whose accepted records wait for the runtime after a restart.
    reconnect_checks: BTreeSet<String>,
    restart_cursor: Option<Option<(String, String)>>,
    /// The last session whose actions the restart reconcile recomputed: pages
    /// are in session order, one that spans two pages is done once.
    restart_refreshed: Option<String>,
    /// When this management process started: the restart reconcile only
    /// judges records last written before it ([`restart_next`]).
    process_started: DateTime<Utc>,
    drift_cursor: Option<(String, String)>,
    last_drift: Instant,
    last_watch: Instant,
    last_compact: Option<Instant>,
    /// Sessions whose start was requested (§2.6 `starting`), since when.
    starting: BTreeMap<String, Instant>,
    /// Sessions with a record that may become `stalled{noProgress}`, when.
    stall_checks: BTreeMap<String, DateTime<Utc>>,
    /// r274.1: `timelineItemId` of records already in context (once per
    /// engine start, one batch per pass).
    timeline_backfill: super::input_ledger_bind::Backfill,
    /// r274.2: passive delegation records left from before cutex stopped
    /// creating them (once per engine start, one page per pass).
    passive_notices: super::input_ledger_asks::PassiveNotices,
    /// r274.2 §9.6: records of turns a committed history revert removed
    /// (persisted receipts; once per engine start).
    history_reverts: super::input_ledger_revert::Recovery,
}

fn run() {
    let Some(hooks) = HOOKS.get().copied() else { return };
    let mut engine: Option<Engine> = None;
    loop {
        if !il::enabled() {
            engine = None;
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }
        let engine = engine.get_or_insert_with(|| Engine::start(hooks));
        if let Err(error) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| engine.pass())) {
            eprintln!("cutex input ledger (settle): pass panicked: {error:?}");
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

impl Engine {
    fn start(hooks: Hooks) -> Self {
        let mut engine = Engine {
            runtime: LiveRuntime(hooks),
            subscription: None,
            watched: BTreeMap::new(),
            known: BTreeSet::new(),
            reconnect_checks: BTreeSet::new(),
            restart_cursor: Some(None),
            restart_refreshed: None,
            // The engine starts lazily (first v2 request) and slowly under
            // load; records this process wrote before that are not leftovers
            // either, so the OS process creation time, not the engine's.
            process_started: crate::platform::process::process_started_at(std::process::id())
                .unwrap_or_else(|_| Utc::now()),
            drift_cursor: None,
            last_drift: Instant::now(),
            last_watch: Instant::now(),
            last_compact: None,
            starting: BTreeMap::new(),
            stall_checks: BTreeMap::new(),
            timeline_backfill: super::input_ledger_bind::Backfill::start(),
            passive_notices: super::input_ledger_asks::PassiveNotices::start(),
            history_reverts: super::input_ledger_revert::Recovery::start(),
        };
        if let Ok(ledger) = il::default_ledger() {
            // Opening the ledger detects a lost / corrupt file (§2.12).
            let _ = ledger.epoch();
            handle_losses(ledger);
        }
        engine.subscribe();
        // A first delivery for every local session.
        for session in local_sessions() {
            engine.known.insert(session);
        }
        let known: Vec<String> = engine.known.iter().cloned().collect();
        for session in known {
            engine.refresh(&session);
        }
        engine
    }

    fn subscribe(&mut self) {
        self.subscription = None;
        let Ok(repository) = super::repository::management_v2_repository() else { return };
        let Ok(checkpoint) = repository.checkpoint() else { return };
        let query = super::repository::ReplayQuery {
            stream_id: Some(checkpoint.stream_id.clone()),
            after: checkpoint.cursor.clone(),
            limit: 1,
            cutex_session_id: None,
        };
        match repository.page_and_subscribe(query, 4096) {
            Ok(subscription) => self.subscription = Some(subscription),
            Err(error) => eprintln!("cutex input ledger (settle): subscribe: {error:?}"),
        }
    }

    fn pass(&mut self) {
        let Ok(ledger) = il::default_ledger() else {
            std::thread::sleep(Duration::from_secs(1));
            return;
        };
        // Wait for events (or a dirty mark / the next deadline).
        let wait = match il::next_deadline(ledger) {
            Ok(Some(at)) => (at - Utc::now()).to_std().unwrap_or(Duration::ZERO).min(Duration::from_millis(250)),
            _ => Duration::from_millis(250),
        };
        let mut events = Vec::new();
        match &self.subscription {
            Some(subscription) => {
                if let Ok(event) = subscription.receiver.recv_timeout(wait) {
                    events.push(event);
                    events.extend(subscription.receiver.try_iter().take(4096));
                }
                if subscription.lagged.load(std::sync::atomic::Ordering::SeqCst) {
                    self.subscribe();
                    let known: Vec<String> = self.known.iter().cloned().collect();
                    for session in known {
                        mark_dirty(&session);
                    }
                }
            }
            None => {
                let guard = SIGNAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let _ = WAKE.wait_timeout(guard, wait);
                self.subscribe();
            }
        }
        let mut dirty = take_dirty();
        let now = Utc::now();
        for event in &events {
            self.on_event(ledger, event, now, &mut dirty);
        }
        if self.last_watch.elapsed() >= Duration::from_secs(1) {
            self.last_watch = Instant::now();
            dirty.extend(self.watched.keys().cloned());
            dirty.extend(self.reconnect_checks.iter().cloned());
            dirty.extend(self.starting.keys().cloned());
            let due: Vec<String> =
                self.stall_checks.iter().filter(|(_, at)| **at <= now).map(|(session, _)| session.clone()).collect();
            for session in due {
                self.stall_checks.remove(&session);
                dirty.insert(session);
            }
        }
        for session in dirty {
            self.refresh(&session);
        }
        if !self.history_reverts.done() {
            self.history_reverts.step(ledger);
        }
        self.restart_step(ledger);
        if !self.timeline_backfill.done() {
            self.timeline_backfill.step(ledger, &super::input_ledger_bind::Stores);
        }
        if !self.passive_notices.done() {
            self.passive_notices.step(ledger);
        }
        if let Err(error) = il::run_deadline_pass(
            ledger,
            Utc::now(),
            BATCH,
            |due| {
                let record = ledger.get(&due.cutex_session_id, std::slice::from_ref(&due.input_id)).ok()?.records.pop()?;
                if record.rev != due.rev {
                    return None;
                }
                deadline_decision(&record, &self.runtime, &LiveOwnerStores, Utc::now())
            },
            |transition| {
                if let Err(error) = il::publish(transition) {
                    eprintln!("cutex input ledger (settle): publish: {error:#}");
                }
            },
        ) {
            eprintln!("cutex input ledger (settle): deadlines: {error}");
        }
        if self.last_drift.elapsed() >= DRIFT_EVERY {
            self.last_drift = Instant::now();
            self.drift(ledger);
            let known: Vec<String> = self.known.iter().cloned().collect();
            for session in known {
                self.refresh(&session);
            }
        }
        if self.last_compact.is_none_or(|at| at.elapsed() >= Duration::from_secs(24 * 60 * 60)) {
            self.last_compact = Some(Instant::now());
            let now = Utc::now();
            let _ = ledger.compact(now, Duration::from_secs(il::SETTLED_RETENTION_SECS), 1000);
            let _ = ledger.compact_payloads(now, PAYLOAD_RETENTION, 1000);
        }
    }

    fn on_event(&mut self, ledger: &InputLedger, event: &super::model::EventEnvelope, now: DateTime<Utc>, dirty: &mut BTreeSet<String>) {
        let session = event.cutex_session_id.as_str();
        if session.is_empty() {
            return;
        }
        if let Some(native) = &event.native {
            match native.message.get("method").and_then(Value::as_str) {
                Some("turn/started" | "thread/status/changed") => {
                    dirty.insert(session.to_string());
                }
                Some("turn/completed") => {
                    let params = &native.message["params"];
                    let turn = params.pointer("/turn/id").or(params.get("turnId")).and_then(Value::as_str);
                    let status = params.pointer("/turn/status").and_then(Value::as_str).unwrap_or("completed");
                    if let Some(turn) = turn {
                        turn_ended(ledger, &self.runtime, session, turn, status, now);
                    }
                    dirty.insert(session.to_string());
                }
                _ => {}
            }
            return;
        }
        if let Some(cutex) = &event.cutex {
            match cutex.method.as_str() {
                il::STATE_CHANGED_EVENT => {
                    let record = &cutex.params["record"];
                    if matches!(record["kind"].as_str(), Some("agentMessage" | "peerMessage"))
                        && matches!(record["state"].as_str(), Some("waiting" | "held"))
                        && record["wakeSchedule"].is_null() { dirty.insert(session.to_string()); }
                }
                il::DELIVERY_CHANGED_EVENT | "cutex/stream/reset" => {}
                "cutex/runtime/onlineRequested" => {
                    self.starting.insert(session.to_string(), Instant::now());
                    dirty.insert(session.to_string());
                }
                "cutex/runtime/disconnected" | "cutex/runtime/offline" => {
                    self.starting.remove(session);
                    if !self.watched.contains_key(session) {
                        disconnected(ledger, session, now);
                        self.watched.insert(session.to_string(), Watch {
                            generation: cutex.params.get("runtimeGeneration").and_then(Value::as_u64),
                        });
                    }
                    dirty.insert(session.to_string());
                }
                method => {
                    // A start that ended without a runtime (closed, error).
                    if method.starts_with("cutex/runtime/")
                        && matches!(
                            cutex.params.get("status").and_then(Value::as_str),
                            Some("offline" | "closed" | "closing" | "error")
                        )
                    {
                        self.starting.remove(session);
                    }
                    dirty.insert(session.to_string());
                }
            }
        }
    }

    /// Recompute and publish one session's delivery; notice a disconnect /
    /// reconnect on the way.
    fn refresh(&mut self, session: &str) {
        let Ok(ledger) = il::default_ledger() else { return };
        let now = Utc::now();
        let view = self.runtime.view(session);
        if view.present && !view.connected && !view.stopped && !self.watched.contains_key(session) {
            disconnected(ledger, session, now);
            self.watched.insert(session.to_string(), Watch { generation: view.generation });
        } else if view.connected {
            if let Some(watch) = self.watched.remove(session) {
                let changed = watch.generation.is_some() && watch.generation != view.generation;
                reconnected(ledger, &self.runtime, session, changed, now);
            }
            if self.reconnect_checks.remove(session) {
                reconnected(ledger, &self.runtime, session, false, now);
            }
        } else if !view.present || view.stopped {
            self.watched.remove(session);
        }
        let starting = match self.starting.get(session) {
            Some(_) if view.connected => {
                self.starting.remove(session);
                false
            }
            Some(at) if at.elapsed() >= STARTING_AT_MOST => {
                self.starting.remove(session);
                false
            }
            Some(_) => true,
            None => false,
        };
        let mut facts = delivery_facts(ledger, session, view.clone());
        facts.starting = starting;
        let delivery = session_delivery(&facts, now);
        super::input_ledger_wake::refresh(ledger, session, &delivery);
        self.stall(ledger, session, delivery["state"] == "idle", now);
        self.stuck_owner(ledger, session, &view, facts.lease.as_ref(), now);
        match ledger.delivery_changed(session, &delivery) {
            Ok(change) => {
                if let Err(error) = il::publish_delivery(session, &change) {
                    eprintln!("cutex input ledger (settle): publish delivery {session}: {error:#}");
                }
                self.known.insert(session.to_string());
            }
            Err(error) => eprintln!("cutex input ledger (settle): delivery {session}: {error}"),
        }
    }

    /// §2.2 `stalled{noProgress}` for the session's waiting records, and
    /// when to look again (no polling: one timer per session).
    fn stall(&mut self, ledger: &InputLedger, session: &str, idle: bool, now: DateTime<Utc>) {
        let stall_after = ledger.deadlines().stall_after;
        let records = unsettled(ledger, session);
        for record in &records {
            if let Some(next) = stall_next(record, idle, now, stall_after) {
                apply(ledger, record, next);
            }
        }
        match next_stall_check(&records, idle, stall_after) {
            Some(at) => {
                self.stall_checks.insert(session.to_string(), at);
            }
            None => {
                self.stall_checks.remove(session);
            }
        }
    }

    /// r274.2 §9.4: mark (or unmark) the session's owner `accepted` records
    /// that will not progress by themselves ([`owner_stuck_case`]); an
    /// orphan candidate is looked at again when its quiet period ends.
    fn stuck_owner(&mut self, ledger: &InputLedger, session: &str, view: &RuntimeView, lease: Option<&Value>, now: DateTime<Utc>) {
        let settle_after = ledger.deadlines().stall_after;
        let records: Vec<InputRecord> = unsettled(ledger, session)
            .into_iter()
            .filter(|record| record.kind == InputKind::Owner && record.state == InputState::Accepted)
            .collect();
        for record in &records {
            let queued = LiveOwnerStores.queued(session, &record.input_id);
            let case = owner_stuck_case(record, view, lease, queued, now, settle_after);
            if let Some(next) = owner_stuck_next(record, case, now) {
                if apply(ledger, record, next).is_some_and(|t| t.changed) {
                    eprintln!("cutex input ledger (settle): {session}/{}: stuck owner input: {}", record.input_id, case.unwrap_or("cleared"));
                }
            }
        }
        if let Some(at) = next_orphan_check(&records, now, settle_after) {
            let entry = self.stall_checks.entry(session.to_string()).or_insert(at);
            *entry = (*entry).min(at);
        }
    }

    /// §2.12 restart reconcile: one batch of unsettled records per pass.
    fn restart_step(&mut self, ledger: &InputLedger) {
        let Some(cursor) = self.restart_cursor.clone() else { return };
        let page = match ledger.unsettled_page(cursor.as_ref(), BATCH) {
            Ok(page) => page,
            Err(error) => {
                eprintln!("cutex input ledger (settle): restart reconcile: {error}");
                self.restart_cursor = None;
                return;
            }
        };
        let now = Utc::now();
        // Rows an older build wrote: their actions, once per session.
        let sessions = restart_refresh_sessions(&page, self.restart_refreshed.as_deref());
        if let Some(last) = sessions.last() {
            self.restart_refreshed = Some((*last).to_string());
        }
        for session in sessions {
            match ledger.refresh_open_actions(session) {
                Ok(change) => {
                    if let Err(error) = il::publish_delivery(session, &change) {
                        eprintln!("cutex input ledger (settle): publish {session}: {error:#}");
                    }
                }
                Err(error) => eprintln!("cutex input ledger (settle): restart actions {session}: {error}"),
            }
        }
        for record in &page {
            match restart_next(record, &LiveOwnerStores, self.process_started) {
                Restart::Skip => {}
                Restart::ReconnectCheck => {
                    self.reconnect_checks.insert(record.cutex_session_id.clone());
                }
                Restart::Apply(next) => {
                    apply(ledger, record, next);
                }
                Restart::Drift => drift_one(ledger, record, now),
            }
        }
        self.restart_cursor = match page.last() {
            Some(last) if page.len() == BATCH => Some(Some((last.cutex_session_id.clone(), last.input_id.clone()))),
            _ => None,
        };
    }

    /// §2.12 drift: at most [`BATCH`] unsettled records per pass, ledger vs
    /// stores only (no native calls).
    fn drift(&mut self, ledger: &InputLedger) {
        let page = match ledger.unsettled_page(self.drift_cursor.as_ref(), BATCH) {
            Ok(page) => page,
            Err(error) => {
                eprintln!("cutex input ledger (settle): drift: {error}");
                return;
            }
        };
        let now = Utc::now();
        for record in &page {
            drift_one(ledger, record, now);
        }
        self.drift_cursor = match page.last() {
            Some(last) if page.len() == BATCH => Some((last.cutex_session_id.clone(), last.input_id.clone())),
            _ => None,
        };
    }
}

fn drift_one(ledger: &InputLedger, record: &InputRecord, now: DateTime<Utc>) {
    if carried_input_left_behind(ledger, record, now) {
        let next = Next::uncertain("outcomeUnknown").details(record.details.clone());
        if apply(ledger, record, next).is_some() {
            eprintln!("{DRIFT_LOG}: {}/{}: {} -> uncertain (its carrier is over)", record.cutex_session_id, record.input_id, record.state);
        }
        return;
    }
    let steps = match record.kind {
        InputKind::Owner => owner_drift(record, &LiveOwnerStores, now),
        kind if is_delegation(kind) => {
            let age = parse_time(&record.updated_at).map_or(0, |at| (now - at).num_seconds());
            if age >= DRIFT_MIN_AGE_SECS {
                if let Ok(store) = crate::asks::store::AskStore::open_default() {
                    if let Ok(Some(job)) = store.project_snapshot(|state| crate::asks::ledger::drift_job(state, record)) {
                        let before = record.state;
                        let applied = crate::asks::ledger::apply_job(ledger, &job);
                        for transition in &applied {
                            let _ = il::publish(transition);
                        }
                        if let Some(last) = applied.last().filter(|t| t.changed && t.record.state != before) {
                            eprintln!("{DRIFT_LOG}: {}/{}: {} -> {} (asks store)", record.cutex_session_id, record.input_id, before, last.record.state);
                        }
                    }
                }
            }
            Vec::new()
        }
        kind if is_bus(kind) => {
            bus_drift_one(ledger, record, now);
            Vec::new()
        }
        _ => Vec::new(),
    };
    let mut current = record.clone();
    for next in steps {
        let to = next.state;
        match apply(ledger, &current, next) {
            Some(transition) => {
                eprintln!("{DRIFT_LOG}: {}/{}: {} -> {} (owner stores)", record.cutex_session_id, record.input_id, current.state, to);
                current = transition.record;
            }
            None => break,
        }
    }
}

/// The sessions of a restart `page` whose open owner rows still need their
/// actions recomputed: pages are in session order, so only the first can be
/// the one `done` on the page before.
fn restart_refresh_sessions<'a>(page: &'a [InputRecord], done: Option<&str>) -> Vec<&'a str> {
    let mut sessions: Vec<&str> = page.iter()
        .filter(|record| record.kind == InputKind::Owner && matches!(record.state, InputState::Accepted | InputState::Held | InputState::Waiting))
        .map(|record| record.cutex_session_id.as_str())
        .collect();
    sessions.dedup();
    sessions.retain(|session| done != Some(*session));
    sessions
}

/// Whether the send that carried `input` can no longer get it read (057
/// section 6): that send is gone, no longer carries it, was given up, or was
/// read in a turn that has ended (`turn_over`: asked only then). A carrier the
/// owner can still resend (`uncertain`, `failed`) is not over.
fn carrier_over(input: &str, carrier: Option<&InputRecord>, turn_over: impl FnOnce() -> bool) -> bool {
    let Some(carrier) = carrier else { return true };
    if !carrier.carries.iter().any(|id| id == input) {
        return true;
    }
    match carrier.state {
        InputState::InContext => turn_over(),
        InputState::Rejected | InputState::Discarded | InputState::Superseded => true,
        _ => false,
    }
}

/// A carried input still reserved for a send that is over, not read, and not
/// given back (no return capability, or the runtime never had it). Nothing
/// else moves it: `uncertain` gives it its own retry and dismiss, and a later
/// receipt still settles it.
fn carried_input_left_behind(ledger: &InputLedger, record: &InputRecord, now: DateTime<Utc>) -> bool {
    if !matches!(record.state, InputState::Sending | InputState::Accepted)
        || il::shown_to_agent(record)
        || record.details.get(il::READ_SIGNAL).is_some()
        || parse_time(&record.updated_at).is_some_and(|at| (now - at).num_seconds() < DRIFT_MIN_AGE_SECS)
    {
        return false;
    }
    let Some(owner) = record.details.get("carriedByOwner").and_then(Value::as_str) else { return false };
    let session = &record.cutex_session_id;
    // Bus notes only: a carried delegation result is moved by the asks store
    // when its event's turn ends (`uncertain{turnInterrupted}`).
    let reserved = is_bus(record.kind) && super::agent_bus_state::agent_bus_message_repository()
        .and_then(|repository| repository.owner_carry_ids(session, owner))
        .is_ok_and(|ids| ids.contains(&record.input_id));
    if !reserved {
        return false;
    }
    let Ok(carrier) = ledger.get(session, &[owner.to_string()]).map(|mut got| got.records.pop()) else { return false };
    carrier_over(&record.input_id, carrier.as_ref(), || thread_idle(ledger, session))
}

/// An input the owner retried that an idle thread does not take: the runtime
/// still holds that id and does not run it again, and `accepted` has no
/// action. Back to `uncertain`, so it can at least be dismissed. (A passive
/// input waits for a next turn; that is not this.)
fn retry_not_taken(ledger: &InputLedger, record: &InputRecord) -> bool {
    record.state == InputState::Accepted
        && record.attempt >= 2
        && record.turn_id.is_none()
        && record.intent.timing != il::Timing::Passive
        && !il::shown_to_agent(record)
        && record.details.get(il::READ_SIGNAL).is_none()
        && thread_idle(ledger, &record.cutex_session_id)
}

fn thread_idle(ledger: &InputLedger, session: &str) -> bool {
    ledger.list(session, il::ListQuery::default())
        .is_ok_and(|list| list.delivery.is_some_and(|delivery| delivery.get("turn").is_none_or(Value::is_null)))
}

/// Drift (§2.12) of a bus record: the agent-bus store says where it should
/// be ([`super::input_ledger_bus::target_of`]). Only a different STATE the
/// §2.3 edges allow is corrected (waiting reasons written by other paths —
/// hostBusy, agentOffline, ownerRetry … — are not the store's to judge).
/// No native calls.
pub fn bus_drift(
    record: &InputRecord,
    view: Option<&(String, super::input_ledger_bus::BusFacts, il::NewInput)>,
    now: DateTime<Utc>,
) -> Option<super::input_ledger_bus::Target> {
    use super::input_ledger_bus::{plan, target_of};
    let age = parse_time(&record.updated_at).map_or(0, |at| (now - at).num_seconds());
    if age < DRIFT_MIN_AGE_SECS {
        return None;
    }
    let (owner, facts, _) = view?;
    if owner != &record.cutex_session_id {
        return None;
    }
    let target = target_of(facts);
    let state = target.state();
    (state != record.state && !plan(Some(record.state), &target, facts.timing).is_empty()).then_some(target)
}

fn bus_drift_one(_ledger: &InputLedger, record: &InputRecord, now: DateTime<Utc>) {
    if parse_time(&record.updated_at).is_some_and(|at| (now - at).num_seconds() < DRIFT_MIN_AGE_SECS) {
        return;
    }
    let view = match super::agent_bus_state::agent_bus_message_repository()
        .and_then(|repository| repository.input_ledger_view(&record.input_id))
    {
        Ok(view) => view,
        Err(error) => {
            eprintln!("cutex input ledger (settle): bus drift {}/{}: {error:#}", record.cutex_session_id, record.input_id);
            return;
        }
    };
    if retry_not_taken(_ledger, record) {
        let next = Next::uncertain("outcomeUnknown").details(record.details.clone());
        if apply(_ledger, record, next).is_some() {
            eprintln!("{DRIFT_LOG}: {}/{}: accepted -> uncertain (the runtime holds the retried input)", record.cutex_session_id, record.input_id);
        }
        return;
    }
    let Some(target) = bus_drift(record, view.as_ref(), now) else { return };
    let Some((_, _, input)) = view else { return };
    let before = record.state;
    if let Some(after) = super::input_ledger_bus::drive(&record.cutex_session_id, &record.input_id, &input, &target) {
        if after.state != before {
            eprintln!("{DRIFT_LOG}: {}/{}: {} -> {} (agent-bus store)", record.cutex_session_id, record.input_id, before, after.state);
        }
    }
}

fn delivery_facts(ledger: &InputLedger, session: &str, runtime: RuntimeView) -> DeliveryFacts {
    let record_generation = session_record(session).map(|(generation, _)| generation);
    let held_deadlines = super::owner_held::held_for(session)
        .map(|held| held.into_iter().map(|held| held.deadline).collect())
        .unwrap_or_default();
    let stop_hold = super::user_input::user_input_repository()
        .and_then(|repository| repository.pause_state(session))
        .ok()
        .flatten()
        .is_some();
    let asks = crate::asks::store::AskStore::open_default().ok();
    let paused = asks
        .as_ref()
        .and_then(|store| store.peek_shared().ok().flatten())
        .is_some_and(|state| crate::asks::agent_pause::is_paused(&state, session)
            || state.pauses.get(session).is_some_and(|pause| pause.stop_hold));
    let lease = super::input_ledger_asks::lease_view(session);
    let (turn_kind, turn_inputs) = match &runtime.active_turn {
        Some(turn) => turn_inputs(ledger, session, turn),
        None => (None, Vec::new()),
    };
    DeliveryFacts {
        runtime,
        record_generation,
        held_deadlines,
        stop_hold,
        paused,
        // cutex/input/interruptSend interrupted the turn and has not sent yet.
        finishing: super::input_interrupt_send::interrupting(session),
        starting: false,
        lease,
        turn_kind,
        turn_ask_id: None,
        turn_inputs,
    }
}

/// The session's §2.6 delivery as of now (no fence; `None` before the
/// management hooks are installed). `starting` (a start request the settle
/// thread tracks) is not known here: a starting runtime has no turn.
pub fn current_delivery(session: &str) -> Option<Value> {
    let hooks = HOOKS.get().copied()?;
    let ledger = il::default_ledger().ok()?;
    let view = LiveRuntime(hooks).view(session);
    Some(session_delivery(&delivery_facts(ledger, session, view), Utc::now()))
}

/// The running turn's kind and inputs from the ledger (records carried by it).
fn turn_inputs(ledger: &InputLedger, session: &str, turn: &str) -> (Option<String>, Vec<String>) {
    let records = match ledger.list(session, il::ListQuery { since_revision: None, include_settled_secs: 600 }) {
        Ok(list) => list.records,
        Err(_) => return (None, Vec::new()),
    };
    let carried: Vec<&InputRecord> = records.iter().filter(|record| record.turn_id.as_deref() == Some(turn)).collect();
    let kind = if carried.iter().any(|record| record.kind == InputKind::Owner && record.evidence == Some(Evidence::TurnStartInput)) {
        Some("owner")
    } else if carried.iter().any(|record| record.kind == InputKind::DelegationRequest) {
        Some("ask")
    } else if carried.iter().any(|record| record.kind != InputKind::Owner
        && (record.evidence == Some(Evidence::TurnStartInput) || turn_start_read_signal(record)))
    {
        Some("delivery")
    } else if carried.iter().any(|record| record.kind == InputKind::Owner) {
        Some("owner")
    } else {
        None
    };
    (kind.map(str::to_string), carried.iter().map(|record| record.input_id.clone()).collect())
}

/// A non-owner after-turn record its turn was started with or showed first
/// (`details.readSignal` `nativeItem` / `turnStart`, r274.2): a bus wake or
/// results event carried by its own delivery turn, before a receipt sets
/// `turnStartInput`. Display only (the turn's `kind`).
fn turn_start_read_signal(record: &InputRecord) -> bool {
    record.intent.timing == il::Timing::AfterTurn
        && matches!(record.details.get(il::READ_SIGNAL).and_then(Value::as_str), Some("nativeItem" | "turnStart"))
}

fn local_sessions() -> Vec<String> {
    let Ok(store) = crate::session::store::load_cutex_session_store_shared() else { return Vec::new() };
    let host = crate::platform::host::current_host_name();
    store
        .sessions
        .values()
        .filter(|record| record.is_active())
        .filter(|record| crate::runtime::lifecycle::cutex_session_host_is_local(&record.host_id, &host))
        .map(|record| record.cutex_session_id.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// Ledger loss (§2.12): rebuild from the stores, then announce

fn handle_losses(ledger: &InputLedger) {
    let losses = match ledger.pending_losses() {
        Ok(losses) => losses,
        Err(error) => {
            eprintln!("cutex input ledger (settle): losses: {error}");
            return;
        }
    };
    for loss in losses {
        eprintln!(
            "cutex input ledger: ledger lost or replaced (epoch {} -> {}); rebuilding unsettled records from the stores",
            loss.previous_epoch, loss.epoch
        );
        let sessions = local_sessions();
        let rebuilt = rebuild(ledger, &sessions);
        announce_reset(&sessions);
        eprintln!("cutex input ledger: rebuilt {rebuilt} record(s); cutex/stream/reset sent to {} session(s)", sessions.len());
        if let Err(error) = ledger.loss_handled(loss.id) {
            eprintln!("cutex input ledger (settle): loss {}: {error}", loss.id);
        }
    }
}

/// Unsettled records from the underlying stores: owner queue items and held
/// inputs, open bus messages (C3 backfill), open delegations (C4).
pub fn rebuild(ledger: &InputLedger, sessions: &[String]) -> usize {
    let mut rebuilt = 0;
    if let Ok(repository) = super::user_input::user_input_repository() {
        for session in sessions {
            let held = super::owner_held::held_for(session).unwrap_or_default();
            for input in held {
                let strategy = match input.params.get("strategy").and_then(Value::as_str) {
                    Some("queue") => super::user_input::UserInputStrategy::Queue,
                    Some("interrupt") => super::user_input::UserInputStrategy::Interrupt,
                    _ => super::user_input::UserInputStrategy::Auto,
                };
                let items = input.params.get("input").and_then(Value::as_array).cloned().unwrap_or_default();
                let mut facts=input.admission.clone();
                if facts.is_none() && strategy==super::user_input::UserInputStrategy::Queue {
                    facts=Some(super::input_ledger_owner::SourceAdmission{strategy,surface:Surface::NextUp,reason:Some("hostBusy".into())});
                }
                let delivery=repository.delivery(session,&input.client_user_message_id).ok().flatten();
                rebuilt+=usize::from(rebuild_owner_source(ledger,session,&input.client_user_message_id,&items,facts.as_ref(),delivery.as_ref(),true));
            }
            let Ok((_, items)) = repository.list(session) else { continue };
            for item in items {
                let facts=repository.source_admission(session,&item.client_user_message_id).ok().flatten();
                let delivery=repository.delivery(session,&item.client_user_message_id).ok().flatten();
                rebuilt+=usize::from(rebuild_owner_source(ledger,session,&item.client_user_message_id,&item.input,facts.as_ref(),delivery.as_ref(),false));
            }
        }
    }
    match super::agent_bus_state::agent_bus_message_repository().and_then(|repository| repository.input_ledger_backfill()) {
        Ok(created) => rebuilt += created,
        Err(error) => eprintln!("cutex input ledger (settle): bus rebuild: {error:#}"),
    }
    match crate::asks::store::AskStore::open_default().and_then(|store| crate::asks::ledger::rebuild_open(&store)) {
        Ok(created) => rebuilt += created,
        Err(error) => eprintln!("cutex input ledger (settle): asks rebuild: {error:#}"),
    }
    rebuilt
}

/// Restore recorded placement; missing legacy facts become explicitly
/// unconfirmed rather than pretending every queue entry was an explicit Queue.
/// This function never sends, removes a source, or clears C-010 evidence.
fn rebuild_owner_source(
    ledger:&InputLedger,session:&str,id:&str,input:&[Value],
    facts:Option<&super::input_ledger_owner::SourceAdmission>,
    delivery:Option<&super::user_input::InputDelivery>,held:bool,
)->bool {
    use super::user_input::{UserInputStrategy,InputDelivery};
    let strategy=facts.map(|facts|facts.strategy).unwrap_or(UserInputStrategy::Auto);
    let surface=facts.map(|facts|facts.surface).filter(|surface|matches!(surface,Surface::Timeline|Surface::NextUp)).unwrap_or(Surface::NextUp);
    let create=super::input_ledger_owner::new_input(strategy,input);
    let unknown= facts.is_none() || matches!(delivery,Some(InputDelivery::Sending{..}|InputDelivery::Unknown{..}));
    let next=if unknown {
        Next::received().create(create).surface(surface)
    } else if matches!(delivery,Some(InputDelivery::Accepted{..})) {
        Next::sending().create(create).surface(surface)
    } else if held {
        Next::held("hostBusy").create(create).surface(surface)
    } else if let Some(reason)=facts.and_then(|facts|facts.reason.as_deref()).filter(|reason|il::WAITING_REASONS.contains(reason)) {
        Next::waiting(reason).create(create).surface(surface)
    } else {
        Next::received().create(create).surface(surface)
    };
    let Ok(first)=ledger.transition(session,id,Expect::NEW,next) else {return false;};
    if !first.created {return false;}
    let _=il::publish(&first);
    if first.record.state==InputState::Discarded{return true;}
    let next=if unknown||first.record.state==InputState::Received {
        Some(Next::uncertain("outcomeUnknown").details(json!({"sourceRecoveryUnconfirmed":true,"originalPlacementRetained":facts.is_some(),"recoveryDescription":"Original admission details cannot be fully confirmed. The source input is retained. Rebuilding this row does not send it again."})))
    } else if let Some(InputDelivery::Accepted{turn_id,..})=delivery {
        Some(match turn_id {Some(turn)=>Next::accepted().turn(turn),None=>Next::accepted()})
    } else {None};
    if let Some(next)=next {if let Ok(done)=ledger.transition(session,id,Expect::rev(first.record.rev),next){let _=il::publish(&done);}}
    true
}

fn create_missing(ledger: &InputLedger, session: &str, id: &str, next: Next) -> bool {
    match ledger.transition(session, id, Expect::NEW, next) {
        Ok(transition) => {
            let _ = il::publish(&transition);
            transition.created
        }
        Err(LedgerError::Conflict { .. }) => false,
        Err(error) => {
            eprintln!("cutex input ledger (settle): rebuild {session}/{id}: {error}");
            false
        }
    }
}

/// The `cutex/stream/reset`-class notice: clients see `state_lost`, drop
/// their per-session input state and read `cutex/input/list` in full (the
/// new `ledgerEpoch` tells them too, §2.8).
fn announce_reset(sessions: &[String]) {
    let Ok(repository) = super::repository::management_v2_repository() else { return };
    let detected = rfc3339(Utc::now());
    for session in sessions {
        let event = super::model::PendingEvent {
            cutex_session_id: session.clone(),
            host_id: crate::platform::host::current_host_name(),
            source: super::model::EventSource::Cutex,
            schema: None,
            correlation: Default::default(),
            native: None,
            cutex: Some(super::model::CutexMessage {
                method: "cutex/stream/reset".into(),
                params: json!({"previousStreamId": null, "previousLastSequence": 0, "reason": "state_lost",
                    "lostEventCount": null, "resyncRequired": true, "detectedAt": detected}),
            }),
        };
        if let Err(error) = repository.append(event) {
            eprintln!("cutex input ledger (settle): reset notice for {session}: {error:#}");
        }
    }
}

// ---------------------------------------------------------------------------
// `cutex/input/retry` (§2.14)

/// Keep what a retry needs to send an owner input again (the submit params,
/// `allowHeld` included). Switch on only.
pub fn remember_owner_submit(session: &str, client_user_message_id: &str, params: &Value) {
    if !il::enabled() {
        return;
    }
    if let Err(error) = il::default_ledger()
        .map_err(LedgerError::Storage)
        .and_then(|ledger| ledger.put_payload(session, client_user_message_id, params))
    {
        eprintln!("cutex input ledger (settle): retry payload {session}/{client_user_message_id}: {error}");
    }
}

fn retry_error(code: &str, message: String, details: Value) -> UserInputExecutionError {
    UserInputExecutionError {
        stage: "route".to_string(),
        code: code.to_string(),
        message,
        retryable: false,
        details,
        outcome_unknown: false,
    }
}

/// Parse `{inputId, expectedRev}` (a `cutexSessionId` must name the request's session).
pub fn parse_retry(session: &str, params: &Value) -> Result<(String, u64), UserInputExecutionError> {
    let invalid = |message: &str| retry_error("invalid_request", message.to_string(), json!({}));
    let object = params.as_object().ok_or_else(|| invalid("params must be an object"))?;
    if object.keys().any(|key| !matches!(key.as_str(), "inputId" | "expectedRev" | "cutexSessionId")) {
        return Err(invalid("cutex/input/retry takes {inputId, expectedRev}"));
    }
    if object.get("cutexSessionId").is_some_and(|value| value.as_str() != Some(session)) {
        return Err(invalid("cutexSessionId must be the request's session"));
    }
    let id = object.get("inputId").and_then(Value::as_str).filter(|id| !id.is_empty())
        .ok_or_else(|| invalid("inputId must be a non-empty string"))?;
    let rev = il::parse_safe_integer(object.get("expectedRev"), "expectedRev")
        .map_err(|message| invalid(&message))?
        .filter(|rev| *rev > 0)
        .ok_or_else(|| invalid("expectedRev must be a positive integer"))?;
    Ok((id.to_string(), rev))
}

/// Which records the owner can resend (§2.14): every `failed` /
/// `uncertain` one. Owner inputs go again under the same
/// clientUserMessageId, delegation requests through the ask's retry, bus
/// messages and delegation results / questions / status with the same id
/// (`details.mayArriveTwice`: the recipient may see them twice, §2.10).
/// r274.2 §9.4: also an owner record a lost runtime left stuck
/// (`il::owner_stuck`), never resent by cutex itself.
pub fn retryable(record: &InputRecord) -> bool {
    matches!(record.state, InputState::Failed | InputState::Uncertain) || il::owner_stuck(record)
}

/// Where a retry of `record` is sent again, checked BEFORE the ledger moves
/// (a store that cannot resend it refuses the retry; the record stays).
fn redelivery_precheck(session: &str, record: &InputRecord) -> Result<(), UserInputExecutionError> {
    let refuse = |(code, message): (&'static str, String)| {
        retry_error("input_not_retryable", message, json!({"inputId": record.input_id, "currentRev": record.rev,
            "state": record.state, "kind": record.kind, "storeCode": code}))
    };
    let unavailable = |error: anyhow::Error| UserInputExecutionError { retryable: true,
        ..retry_error("store_unavailable", format!("{error:#}"), json!({"inputId": record.input_id})) };
    match record.kind {
        InputKind::DelegationRequest=> {
            let valid=crate::asks::store::AskStore::open_default().and_then(|store|store.project_snapshot(|state|
                state.asks.get(record.ask_id.as_deref().unwrap_or(&record.input_id)).is_some_and(|ask|
                    ask.request_input_id()==record.input_id&&ask.is_open()&&ask.execution==crate::asks::model::ExecutionState::Interrupted)))
                .map_err(unavailable)?;
            if valid {Ok(())} else {Err(refuse(("request_occurrence_changed","the request occurrence is no longer retryable; it can still be dismissed".into())))}
        }
        kind if is_bus(kind) => super::agent_bus_state::agent_bus_message_repository()
            .and_then(|repository| repository.input_ledger_redeliverable(session, &record.input_id))
            .map_err(unavailable)?
            .map_err(refuse),
        InputKind::DelegationResult | InputKind::DelegationQuestion | InputKind::DelegationStatus => {
            crate::asks::store::AskStore::open_default()
                .and_then(|store| store.project_snapshot(|state| crate::asks::ledger::result_redeliverable(state, session, &record.input_id)))
                .map_err(unavailable)?
                .map_err(refuse)
        }
        _ => Ok(()),
    }
}

/// The store half of a bus retry: drop the earlier attempt's receipts
/// (written without a ledger echo: the record already says
/// `waiting{ownerRetry}`), then ask the Agent Bus to queue its pending
/// messages again (the startup redrive; it skips what is still queued), so
/// the recipient's bridge submits this one again with the same id.
fn redeliver_bus(session: &str, id: &str) -> Value {
    let reset = super::agent_bus_state::agent_bus_message_repository().and_then(|repository| {
        let (result, _echo) = super::input_ledger_bus::deferred(|| repository.input_ledger_redeliver(session, id, Utc::now()));
        result
    });
    match reset {
        Ok(true) => {}
        Ok(false) => return json!({"path": "busRedeliver", "ok": false,
            "error": {"code": "bus_message_settled", "message": "the message was settled meanwhile", "retryable": false, "details": {}}}),
        Err(error) => return json!({"path": "busRedeliver", "ok": false,
            "error": {"code": "bus_unavailable", "message": format!("{error:#}"), "retryable": true, "details": {}}}),
    }
    let client = crate::agent_bus::client::AgentBusHttpClient::from_config(&crate::config::store::load_codez_config());
    match client.redrive_durable_messages() {
        Ok(_) => json!({"path": "busRedeliver", "ok": true, "error": null, "mayArriveTwice": true}),
        // Reset durably: the next Agent Bus start (or redrive) queues it.
        Err(error) => json!({"path": "busRedeliver", "ok": false, "mayArriveTwice": true,
            "error": {"code": "bus_redrive_failed", "message": format!("{error:#}"), "retryable": true, "details": {}}}),
    }
}

/// The store half of a delegation result retry ([`crate::asks::ledger::retry_result`]):
/// the scheduler sends it again in a new event under the same deliveryId.
fn redeliver_result(session: &str, id: &str) -> Value {
    let result = crate::asks::store::AskStore::open_default()
        .and_then(|store| store.with_state(|state| crate::asks::ledger::retry_result(state, session, id, Utc::now())));
    match result {
        Ok(Ok(())) => json!({"path": "delegationResultRequeue", "ok": true, "error": null, "mayArriveTwice": true}),
        Ok(Err((code, message))) => json!({"path": "delegationResultRequeue", "ok": false,
            "error": {"code": code, "message": message, "retryable": false, "details": {}}}),
        Err(error) => json!({"path": "delegationResultRequeue", "ok": false,
            "error": {"code": "asks_unavailable", "message": format!("{error:#}"), "retryable": true, "details": {}}}),
    }
}

/// The ledger half of a retry: `failed` / `uncertain` → `waiting{ownerRetry}`
/// (attempt + 1, same id), CAS on `expectedRev`.
pub fn retry_transition(ledger: &InputLedger, session: &str, id: &str, expected_rev: u64) -> Result<Transition, UserInputExecutionError> {
    retry_transition_checked(ledger, session, id, expected_rev, &|_| Ok(()))
}

/// [`retry_transition`] after `precheck` (the owning store can resend the
/// record; it refuses before the ledger moves).
pub fn retry_transition_checked(
    ledger: &InputLedger,
    session: &str,
    id: &str,
    expected_rev: u64,
    precheck: &dyn Fn(&InputRecord) -> Result<(), UserInputExecutionError>,
) -> Result<Transition, UserInputExecutionError> {
    let record = ledger.get(session, &[id.to_string()])
        .map_err(il::ledger_error)?
        .records.pop()
        .ok_or_else(|| retry_error("input_not_found", format!("no input {id} in {session}"), json!({"inputId": id})))?;
    if record.rev != expected_rev {
        return Err(retry_error("revision_conflict", format!("expected rev {expected_rev}"), json!({"inputId": id, "currentRev": record.rev})));
    }
    if !retryable(&record) {
        return Err(retry_error(
            "input_not_retryable",
            format!("{} {} cannot be resent", record.kind, record.state),
            json!({"inputId": id, "currentRev": record.rev, "state": record.state, "kind": record.kind}),
        ));
    }
    precheck(&record)?;
    let mut expected_rev = expected_rev;
    if il::owner_stuck(&record) {
        // r274.2 §9.4: the owner stops waiting for the lost runtime and
        // sends it again: its first outcome is unknown.
        let lost = ledger
            .transition(session, id, Expect::rev(expected_rev), Next::uncertain(il::stuck_uncertain_reason(&record)).stalled(None))
            .map_err(|error| match error {
                LedgerError::Conflict { current_rev } => retry_error("revision_conflict", error.to_string(), json!({"inputId": id, "currentRev": current_rev})),
                other => il::ledger_error(other),
            })?;
        if let Err(error) = il::publish(&lost) {
            eprintln!("cutex input ledger (settle): publish {session}/{id}: {error:#}");
        }
        expected_rev = lost.record.rev;
    }
    let transition = ledger
        .transition(session, id, Expect::rev(expected_rev), Next::waiting("ownerRetry"))
        .map_err(|error| match error {
            LedgerError::Conflict { current_rev } => retry_error("revision_conflict", error.to_string(), json!({"inputId": id, "currentRev": current_rev})),
            other => il::ledger_error(other),
        })?;
    if transition.record.state!=InputState::Waiting || transition.record.reason.as_deref()!=Some("ownerRetry") {
        return Err(retry_error("input_disposed","this input was disposed while retry admission was checked".into(),json!({"inputId":id})));
    }
    if let Err(error) = il::publish(&transition) {
        eprintln!("cutex input ledger (settle): publish {session}/{id}: {error:#}");
    }
    Ok(transition)
}

/// `cutex/input/retry {inputId, expectedRev}` (owner only): the ledger
/// transition, then the owning block's path sends the same id again
/// (owner: `cutex/userInput/submit` with the same clientUserMessageId and
/// params; delegation request: the ask's retry; bus message: its receipts
/// dropped and the Agent Bus redrive; delegation result / question /
/// status: pending again for a new event). Bus messages and results may
/// reach the recipient twice (`redelivery.mayArriveTwice`).
pub(super) fn retry(
    owner: bool,
    session: &str,
    params: &Value,
    resubmit: &mut dyn FnMut(&Value) -> Result<Value, UserInputExecutionError>,
) -> Result<Value, UserInputExecutionError> {
    if !il::enabled() {
        return Err(retry_error("cutex_method_denied", "the input ledger is disabled (management.inputLedger.enabled)".into(), json!({})));
    }
    if !owner {
        return Err(retry_error("owner_required", "cutex/input/retry is the owner's (Management root credential)".into(), json!({})));
    }
    let (id, expected_rev) = parse_retry(session, params)?;
    let ledger = il::default_ledger().map_err(|error| il::ledger_error(LedgerError::Storage(error)))?;
    retry_on(ledger, session, &id, expected_rev, resubmit)
}

/// [`retry`] on `ledger`, the request parsed and allowed.
fn retry_on(
    ledger: &InputLedger,
    session: &str,
    id: &str,
    expected_rev: u64,
    resubmit: &mut dyn FnMut(&Value) -> Result<Value, UserInputExecutionError>,
) -> Result<Value, UserInputExecutionError> {
    let id = id.to_string();
    let before = ledger.get(session, std::slice::from_ref(&id)).map_err(il::ledger_error)?.records.pop();
    let transition = retry_transition_checked(ledger, session, &id, expected_rev, &|record| redelivery_precheck(session, record))?;
    let redelivery = match transition.record.kind {
        InputKind::Owner => {
            let sent = match ledger.payload(session, &id) {
                Ok(Some(payload)) => {
                    // The owner asked to send it again: an earlier native acceptance
                    // of the same id (C-010) must not answer this resend as a repeat.
                    if forget_owner_acceptance(session, &id) {
                        resend_alone(ledger, session, &transition.record);
                    }
                    resubmit(&payload)
                }
                Ok(None) => Err(retry_error("retry_payload_unavailable", "the original submit is no longer kept".into(), json!({}))),
                Err(error) => Err(il::ledger_error(error)),
            };
            match sent {
                Ok(result) => json!({"path": "ownerResubmit", "ok": true, "error": null, "result": result}),
                Err(error) => {
                    let mut failure = error_json(&error);
                    if before.as_ref().is_some_and(|before| refused_resend_returns(ledger, session, &id, before)) {
                        failure["details"]["nothingSent"] = json!(true);
                    }
                    json!({"path": "ownerResubmit", "ok": false, "error": failure})
                }
            }
        }
        InputKind::DelegationRequest => {
            let result = crate::asks::store::AskStore::open_default().and_then(|store| {
                store.with_state(|state| crate::asks::lifecycle::retry(state, transition.record.ask_id.as_deref().unwrap_or(&id), None, Utc::now()).map(|_| ()))
            });
            match result {
                Ok(Ok(())) => {
                    if let Ok(store)=crate::asks::store::AskStore::open_default() {
                        if let Ok(Some(new_id))=store.project_snapshot(|state|state.asks.get(transition.record.ask_id.as_deref().unwrap_or(&id)).map(|ask|ask.request_input_id())) {
                            if new_id!=id {
                                if let Ok(done)=ledger.transition(session,&id,Expect::rev(transition.record.rev),Next::superseded("revised").superseded_by(new_id)) {let _=il::publish(&done);}
                            }
                        }
                    }
                    json!({"path":"delegationRequeue","ok":true,"error":null})
                },
                Ok(Err(error)) => json!({"path": "delegationRequeue", "ok": false, "error": {"code": error.code, "message": error.detail, "retryable": false, "details": error.extra}}),
                Err(error) => json!({"path": "delegationRequeue", "ok": false, "error": {"code": "asks_unavailable", "message": format!("{error:#}"), "retryable": true, "details": {}}}),
            }
        }
        kind if is_bus(kind) => redeliver_bus(session, &id),
        _ => redeliver_result(session, &id),
    };
    if redelivery["ok"] == false && transition.record.kind != InputKind::Owner {
        if let Some(before)=&before {
            refused_resend_returns(ledger,session,&id,before);
        }
    }
    mark_dirty(session);
    let latest = ledger.get(session, &[id.clone()]).map_err(il::ledger_error)?;
    let record = latest.records.into_iter().next().unwrap_or(transition.record);
    Ok(json!({
        "ledgerEpoch": latest.ledger_epoch,
        "stateRevision": latest.state_revision,
        "record": record,
        "redelivery": redelivery,
    }))
}

/// Clear the C-010 `accepted` delivery record of owner input `id` (CAS on
/// the record read), so the owner's explicit resend goes out instead of
/// being answered `deduplicated` (r274.2 §9.4). `sending` / `unknown`
/// records stay: they still guard an unresolved native request. `true`: no
/// delivery record is left, nothing unresolved guards this id.
fn forget_owner_acceptance(session: &str, id: &str) -> bool {
    let Ok(repository) = super::user_input::user_input_repository() else { return false };
    match repository.delivery(session, id) {
        Ok(None) => true,
        Ok(Some(delivery @ super::user_input::InputDelivery::Accepted { .. })) => match repository.set_delivery(session, id, Some(&delivery), None) {
            Ok(cleared) => cleared,
            Err(error) => {
                eprintln!("cutex input ledger (settle): retry {session}/{id}: clear the earlier acceptance: {error:#}");
                false
            }
        },
        _ => false,
    }
}

/// The owner resends an input whose steer is over, while the bundle it carried
/// stays reserved (057 §6: only a proven-unsent path or the note's own retry
/// releases it). The empty `carries` says this send goes without that bundle
/// (`OwnerCarries::goes_alone`), also when a later flush sends it.
fn resend_alone(ledger: &InputLedger, session: &str, record: &InputRecord) {
    if record.carries.is_empty() {
        return;
    }
    let mut alone = Next::waiting("ownerRetry");
    alone.carries = Some(Vec::new());
    match ledger.transition(session, &record.input_id, Expect::rev(record.rev), alone) {
        Ok(transition) => {
            if let Err(error) = il::publish(&transition) {
                eprintln!("cutex input ledger (settle): publish {session}/{}: {error:#}", record.input_id);
            }
        }
        Err(error) => eprintln!("cutex input ledger (settle): retry {session}/{}: {error}", record.input_id),
    }
}

/// A resubmit refused without a word to the ledger (`error_step`: "nothing was
/// sent, retry later" and the like) would leave `waiting{ownerRetry}` with no
/// queue item, held input, timer or action. Back to what `before` was, with its
/// actions. `false`: the record moved on, its own outcome stands.
fn refused_resend_returns(ledger: &InputLedger, session: &str, id: &str, before: &InputRecord) -> bool {
    let current = match ledger.get(session, &[id.to_string()]) {
        Ok(mut got) => got.records.pop(),
        Err(error) => {
            eprintln!("cutex input ledger (settle): retry {session}/{id}: {error}");
            None
        }
    };
    let Some(current) = current.filter(|record| record.state == InputState::Waiting && record.reason.as_deref() == Some("ownerRetry") && record.attempt == before.attempt + 1) else {
        return false;
    };
    let reason = before.reason.as_deref();
    let mut back = if before.kind != InputKind::Owner {
        Next::uncertain("outcomeUnknown")
    } else if before.state == InputState::Failed {
        let mut failed = Next::failed(reason.unwrap_or("notDelivered"));
        failed.error = before.error.clone();
        failed
    } else if il::owner_stuck(before) {
        Next::uncertain(il::stuck_uncertain_reason(before))
    } else {
        Next::uncertain(reason.unwrap_or("outcomeUnknown"))
    };
    if !il::owner_stuck(before) {
        back = back.details(before.details.clone());
    }
    let mut details=back.details.take().unwrap_or_else(||json!({}));
    if !details.is_object(){details=json!({});}
    details["retryRefused"]=json!(true);
    back=back.details(details);
    match ledger.transition(session, id, Expect::rev(current.rev), back) {
        Ok(transition) => {
            if let Err(error) = il::publish(&transition) {
                eprintln!("cutex input ledger (settle): publish {session}/{id}: {error:#}");
            }
            true
        }
        Err(error) => {
            eprintln!("cutex input ledger (settle): retry {session}/{id}: {error}");
            false
        }
    }
}

fn error_json(error: &UserInputExecutionError) -> Value {
    json!({"code": error.code, "message": error.message, "retryable": error.retryable, "details": error.details})
}

/// bootstrap `cutexState` (§2.7): `inputs` (unsettled + settled in the last
/// 15 min), `delivery`, `stateRevision`, `ledgerEpoch`. Switch on only.
pub fn add_bootstrap_fields(state: &mut serde_json::Map<String, Value>, session: &str) {
    if !il::enabled() {
        return;
    }
    let list = il::default_ledger()
        .map_err(LedgerError::Storage)
        .and_then(|ledger| ledger.list(session, il::ListQuery::default()));
    match list {
        Ok(list) => {
            state.insert("inputs".into(), serde_json::to_value(&list.records).unwrap_or(Value::Array(Vec::new())));
            if let Some(delivery) = list.delivery {
                state.insert("delivery".into(), delivery);
            }
            state.insert("stateRevision".into(), json!(list.state_revision));
            state.insert("ledgerEpoch".into(), json!(list.ledger_epoch));
        }
        Err(error) => eprintln!("cutex input ledger: bootstrap {session}: {error}"),
    }
}

#[cfg(test)]
#[path = "input_ledger_settle_tests.rs"]
mod tests;
