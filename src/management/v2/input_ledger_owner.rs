//! Input ledger hooks owned by block C2 (owner messages, 054 §2.9 owner rows,
//! §2.5, §2.7 result fields, owner decisions 1 and 8): the mapping from the
//! owner input's own store (submit route, held store, queue, C-010 delivery
//! record) to its `InputRecord`, plus the underlying-store side of an owner
//! dismiss. The admission files call the small entry points here so their own
//! edits stay a few lines each.
//!
//! Record id = `clientUserMessageId`; kind `owner`, sender `owner`; surface by
//! §2.5: `queue`, an offline start (decision 8) and `auto` / `interrupt`
//! while the session is held `paused` / `stopHold` go to `nextUp` at
//! creation, everything else is decided by the route's first write (below).
//!
//! `sending` is written right before the native call, after the C-010
//! `sending` reservation (r274 Director decision 3). An explicit refusal that
//! cutex re-queues (`adapterQueued`, `turnFinishing`, `hostBusy`, …) is
//! `sending → waiting{reason}`; the surface never moves back (a timeline
//! record stays on the timeline). Accepted / unknown outcomes follow it.
//!
//! An owner `auto` / `interrupt` not placed at creation is created when the
//! route decides (r274 Director decision 1): right before native IO
//! (`sending`), before an interrupt, or with its first outcome. Owner
//! decision 2026-11-03 (r274.2): an `auto` input that is not yet in the
//! agent's context (held, waiting for any reason, a steer being sent or
//! accepted) is created on `nextUp` and moves to the timeline exactly once,
//! when the agent reads it (`inContext`, or the Claude `accepted[offered]`);
//! a direct `turn/start`, or an idle send held/waiting only for the busy
//! host (owner decision 2026-11-04), is created on the timeline (§9.4: an `interrupt` waits on nextUp until the
//! stop is accepted and the new turn starts with it). On Claude (§9.1) the
//! steer echo or `offered`, whichever comes first, moves it while it stays
//! `accepted`; `inContext` comes only with `contextPersisted`.
//!
//! Every entry point is a no-op while `management.inputLedger.enabled` is off.
//! None of them may be called while holding the host authority fence.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard, OnceLock};

use serde_json::{json, Value};

use super::input_ledger::{
    self as ledger, Evidence, Expect, InputIntent, InputKind, InputRecord, InputSender, InputState,
    NewInput, Next, SenderKind, Stage, Strategy, Surface, Timing,
};
use super::user_input::{
    UserInputDisposition, UserInputExecutionError, UserInputStrategy, UserInputSubmitExecution,
    UserInputSubmitParams,
};

// ---------------------------------------------------------------------------
// Dismiss (C1 calls this before `discarded{ownerDiscarded}`)

/// Settle the owner input's own store before the ledger marks it
/// `discarded{ownerDiscarded}`. Owner inputs are only dismissible while
/// `uncertain` (`input_ledger::dismissible`). An `uncertain` owner input can
/// still sit in the Management queue (a queue steer / flush whose native
/// outcome is unknown keeps the item, and the flush never resends it, so it
/// would block every item behind it): dismissing removes that item, with the
/// usual `cutex/userInput/queueRemoved`. The C-010 delivery record stays: it
/// keeps blocking a blind resend of the same id until history settles it.
pub(crate) fn dismiss(record: &InputRecord) -> Result<(), String> {
    if record.kind != InputKind::Owner {
        return Ok(());
    }
    let session = &record.cutex_session_id;
    let fail = |error: anyhow::Error| format!("{error:#}");
    let lane = super::user_input::session_input_lock(session).map_err(fail)?;
    let _lane = lane.lock().map_err(|_| "session input lock poisoned".to_string())?;
    let repository = super::user_input::user_input_repository().map_err(fail)?;
    let (_, items) = repository.list(session).map_err(fail)?;
    let Some(item) = items.into_iter().find(|item| item.client_user_message_id == record.input_id) else {
        return Ok(());
    };
    let Some(removed) = repository.remove(session, &item.queue_id, item.revision).map_err(fail)? else {
        return Ok(());
    };
    let (queue_revision, _) = repository.list(session).map_err(fail)?;
    super::repository::management_v2_repository()
        .and_then(|events| {
            events.append(super::model::PendingEvent {
                cutex_session_id: session.clone(),
                host_id: crate::platform::host::current_host_name(),
                source: super::model::EventSource::Cutex,
                schema: None,
                correlation: super::model::EventCorrelation {
                    client_user_message_id: Some(removed.client_user_message_id.clone()),
                    queue_id: Some(removed.queue_id.clone()),
                    ..Default::default()
                },
                native: None,
                cutex: Some(super::model::CutexMessage {
                    method: "cutex/userInput/queueRemoved".into(),
                    params: json!({
                        "queueId": removed.queue_id,
                        "clientUserMessageId": removed.client_user_message_id,
                        "revision": removed.revision,
                        "queueRevision": queue_revision,
                        "reason": "cancelled",
                        "removedAt": chrono::Utc::now().to_rfc3339(),
                    }),
                }),
            })
        })
        .map(|_| ())
        .map_err(fail)
}

// ---------------------------------------------------------------------------
// Pure mapping

/// One fact about an owner input, as its own store reports it.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// `routeAccepted`: create (surface decided here, §2.5).
    Received { surface: Surface },
    /// Held for the owner admission worker (`allowHeld`, busy host fence).
    Held,
    /// Queued in the Management queue (reason already mapped to §2.4).
    Waiting(&'static str),
    /// `turn/start` accepted: the input is the turn's input.
    Started { turn_id: Option<String> },
    /// Native IO is about to run (after the C-010 `sending` reservation).
    /// `steer`: the call is `turn/steer` (the input joins a running turn
    /// and is not read yet), else `turn/start`.
    Sending { steer: bool },
    /// `turn/steer` accepted into the running turn. `stage`: the Claude
    /// adapter's steer reply means the input is registered in its queue
    /// (§2.4 `accepted.stage`); Codex steers have no stage.
    /// `generation`: the runtime generation that accepted it (r274.2 §9.4:
    /// a later generation cannot read it; `None`: unknown).
    Steered { turn_id: Option<String>, stage: Option<Stage>, generation: Option<u64> },
    /// Codex echoed the user item carrying the clientId.
    ClientIdEcho,
    /// Claude echoed the steer's own user item (`steer-<clientId>`, or a
    /// slipped steer's first user item of the next turn). r274.2 §9.1: the
    /// agent was shown it, which is not a context receipt: an `accepted`
    /// record moves to the timeline, still `accepted`, bound to `item`
    /// (`details.readSignal: "steerEcho"`); `inContext` waits for
    /// `contextPersisted`. Already moved (by `offered`): only the id binds.
    SteerEcho { turn_id: Option<String>, item: Option<String> },
    /// Native outcome unknown (C-010 `unknown`).
    Uncertain(&'static str),
    /// Failed without (or with a refused) native write.
    Failed(&'static str),
    /// `cutex/userInput/queue/remove`.
    Removed,
    /// A repeated identity the native history / C-010 receipt already shows.
    AlreadyInContext,
}

/// The transitions that bring `current` to `step` (§2.3 edges only); empty
/// when nothing changes. `create`: the record to create when there is none
/// (`None` for steps that must never create one: an echo, a removal, a
/// repeated identity).
pub fn plan(current: Option<&InputRecord>, step: &Step, create: Option<&NewInput>) -> Vec<Next> {
    use InputState::*;
    let mut out = Vec::new();
    let state = match (current.map(|record| record.state), step) {
        (None, Step::Received { surface }) => {
            if let Some(create) = create {
                out.push(Next::received().create(create.clone()).surface(*surface));
            }
            return out;
        }
        (None, Step::ClientIdEcho | Step::SteerEcho { .. } | Step::Removed | Step::AlreadyInContext) => return out,
        (None, _) => match create {
            Some(create) => {
                out.push(Next::received().create(create.clone()));
                Received
            }
            None => return out,
        },
        (Some(state), _) => state,
    };
    // A failed / uncertain owner input that goes out again (same id: the
    // owner resubmitted it, or its queue item is flushed again) is reopened
    // the way §2.3 allows: `waiting{ownerRetry}`, attempt + 1.
    let reopened = |out: &mut Vec<Next>| -> InputState {
        if matches!(state, Failed | Uncertain) {
            out.push(Next::waiting("ownerRetry"));
            Waiting
        } else {
            state
        }
    };
    // An interruptSend that waited for the host fence (`held{hostBusy}`
    // with `details.interruptSend`) goes out as `sending{interruptSend}`.
    let resumed = current.and_then(interrupt_send_resumed);
    let sent = |out: &mut Vec<Next>, state: InputState| -> bool {
        match state {
            Received | Held | Waiting => {
                out.push(resumed.clone().unwrap_or_else(Next::sending));
                true
            }
            Sending => true,
            _ => false,
        }
    };
    match step {
        Step::Received { .. } => {}
        Step::Held => match reopened(&mut out) {
            Received | Held => out.push(Next::held("hostBusy")),
            // A refused native call handed to the admission worker
            // (decision 3): `sending → waiting`, never back to `held`.
            Waiting | Sending => out.push(Next::waiting("hostBusy")),
            _ => {}
        },
        Step::Waiting(reason) => {
            // `Sending`: an explicit refusal cutex re-queued (decision 3).
            if let Received | Held | Waiting | Sending = reopened(&mut out) {
                out.push(Next::waiting(reason));
            }
        }
        Step::Sending { .. } => {
            let state = reopened(&mut out);
            sent(&mut out, state);
        }
        Step::Started { turn_id } => {
            let state = reopened(&mut out);
            if sent(&mut out, state) || state == Accepted {
                out.push(with_turn(Next::in_context(Evidence::TurnStartInput), turn_id));
            }
        }
        Step::Steered { turn_id, stage, generation } => {
            let state = reopened(&mut out);
            if sent(&mut out, state) || state == Accepted {
                // A stage the adapter already reported is kept (the ledger
                // keeps the old stage when none is given).
                let accepted = match stage {
                    Some(stage) if current.and_then(|record| record.stage).is_none() => Next::accepted().stage(*stage),
                    _ => Next::accepted(),
                };
                let accepted = Next { runtime_generation: *generation, ..accepted };
                out.push(with_turn(accepted, turn_id));
            }
        }
        Step::ClientIdEcho => {
            if matches!(state, Accepted | Uncertain) {
                out.push(Next::in_context(Evidence::ClientIdEcho));
            }
        }
        Step::SteerEcho { turn_id, item } => {
            if let Some(record) = current.filter(|_| state == Accepted) {
                let mut next = Next::accepted();
                if record.surface == Surface::NextUp {
                    next = next.surface(Surface::Timeline)
                        .details(super::input_ledger_bus::with_read_signal(record, "steerEcho"));
                }
                if let Some(item) = item.as_deref().filter(|item| !item.is_empty()) {
                    // Never rebinds a record bound to another item.
                    if record.timeline_item_id.as_deref().is_none_or(|bound| bound == item) {
                        next = next.timeline_item(item);
                    }
                }
                // A steer that slipped to the next turn: that turn has it,
                // the receipt grace of the earlier one no longer applies.
                // The adapter started that turn with it as its prompt, so it
                // is the turn's start input (r275, as `Step::Started`); it is
                // never left `registered` until that turn's receipt.
                if let Some(turn) = turn_id.as_deref().filter(|turn| !turn.is_empty() && record.turn_id.as_deref() != Some(*turn)) {
                    out.push(next.turn(turn).deadline(None));
                    out.push(Next::in_context(Evidence::TurnStartInput).turn(turn));
                } else {
                    out.push(next);
                }
            }
        }
        Step::Uncertain(reason) => {
            let state = reopened(&mut out);
            if sent(&mut out, state) || state == Accepted {
                out.push(Next::uncertain(reason));
            }
        }
        Step::Failed(reason) => {
            // r274.2 §9.4: the owner's resend refused before native IO
            // fails again (keeps its actions).
            let resend = state == Waiting && current.and_then(|record| record.reason.as_deref()) == Some("ownerRetry");
            if matches!(state, Received | Held | Sending | Accepted) || resend {
                out.push(Next::failed(reason));
            }
        }
        Step::Removed => {
            if state == Waiting {
                out.push(Next::discarded("ownerRemoved"));
            }
        }
        Step::AlreadyInContext => match state {
            Received | Held | Waiting | Sending => {
                sent(&mut out, state);
                out.push(Next::in_context(Evidence::HistoryRead));
            }
            Uncertain => out.push(Next::in_context(Evidence::HistoryRead)),
            _ => {}
        },
    }
    out
}

/// `cutex/input/interruptSend` held for the host authority fence: the
/// `sending{interruptSend}` its admission writes (`None`: not one).
pub fn interrupt_send_resumed(record: &InputRecord) -> Option<Next> {
    let mark = record.details.get("interruptSend").filter(|mark| mark.is_object())?;
    (record.state == InputState::Held).then(|| Next::sending().reason("interruptSend").details(mark.clone()))
}

/// [`plan`] for an owner `auto` / `interrupt` record `route_accepted` left
/// pending: it is created by the route's first step. Owner rule 2026-11-03
/// (r274.2 §9.4, "the timeline shows only what has happened or is
/// running"): only a submission that itself starts the agent's turn at
/// once, as its first input, is created on the timeline: a direct
/// `turn/start` (`sending` before a start, or `started`). Every other first
/// step (held, waiting for any reason, a steer being sent or accepted, a
/// failure or an unknown outcome before any turn started) creates it on
/// nextUp; it moves to the timeline once, with its read evidence (the
/// ledger's `moved_surface`). Exception (owner 2026-11-04): the route
/// observed idle and hostBusy, with either allowHeld setting, stays timeline.
pub fn pending_plan(create: &NewInput, step: &Step, idle_host_busy: bool) -> Vec<Next> {
    let mut nexts = plan(None, step, Some(create));
    let starts_turn = matches!(step, Step::Sending { steer: false } | Step::Started { .. });
    // Owner decision 2026-11-04: a busy host cannot move an idle send out
    // of the timeline. Only the route's observed turn state proves idle.
    let idle_busy = idle_host_busy && matches!(step, Step::Held | Step::Waiting("hostBusy"))
        && matches!(create.intent.strategy, Some(Strategy::Auto | Strategy::Interrupt));
    let surface = if starts_turn || idle_busy { Surface::Timeline } else { Surface::NextUp };
    if let Some(first) = nexts.first_mut().filter(|first| first.create.is_some()) {
        first.surface = Some(surface);
    }
    nexts
}

fn with_turn(next: Next, turn_id: &Option<String>) -> Next {
    match turn_id {
        Some(turn) if !turn.is_empty() => next.turn(turn.clone()),
        _ => next,
    }
}

/// The owner record a submit creates.
pub fn new_input(strategy: UserInputStrategy, input: &[Value]) -> NewInput {
    let (strategy, timing) = match strategy {
        UserInputStrategy::Auto => (Strategy::Auto, Timing::NextStep),
        UserInputStrategy::Queue => (Strategy::Queue, Timing::AfterTurn),
        UserInputStrategy::Interrupt => (Strategy::Interrupt, Timing::NextStep),
    };
    NewInput {
        kind: InputKind::Owner,
        sender: InputSender {
            kind: SenderKind::Owner,
            id: "owner".into(),
            name: "owner".into(),
            host: None,
            cutex_session_id: None,
        },
        intent: InputIntent { strategy: Some(strategy), delivery_mode: None, timing },
        preview: preview_text(input),
        ask_id: None,
        remote: None,
    }
}

/// Text of the user input items (the ledger keeps at most 80 characters).
fn preview_text(input: &[Value]) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for item in input {
        match item.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    parts.push(text);
                }
            }
            Some("image" | "localImage") => parts.push("[image]"),
            _ => {}
        }
        if parts.iter().map(|part| part.len()).sum::<usize>() > 400 {
            break;
        }
    }
    parts.join(" ")
}

/// §2.5 creation surface of an owner message.
pub fn creation_surface(strategy: UserInputStrategy, offline: bool, held_paused: bool) -> Surface {
    if offline || strategy == UserInputStrategy::Queue || held_paused {
        Surface::NextUp
    } else {
        Surface::Timeline
    }
}

/// `queuedReason` (wire, unchanged) → §2.4 waiting reason. `paused` on the
/// wire is either the queue pause after `cutex/turn/stop` (`stopHold`) or an
/// agent / project pause (`paused`). `imageBusy` / `joinRefused` wait for the
/// running turn.
pub fn waiting_reason(queued_reason: Option<&str>, queue_paused: bool) -> &'static str {
    match queued_reason {
        Some("paused") if queue_paused => "stopHold",
        Some("paused") => "paused",
        Some("hostBusy") => "hostBusy",
        Some("delegationLease") => "delegationLease",
        Some("turnRunning" | "imageBusy" | "joinRefused") => "turnRunning",
        Some("adapterQueued") => "adapterQueued",
        Some("ownerQueueAhead") => "ownerQueueAhead",
        Some("turnFinishing") => "turnFinishing",
        _ => "explicitQueue",
    }
}

/// An owner submit / queue error → the record's step. Errors that belong to
/// an earlier submit of the same id (`idempotency_conflict`,
/// `user_input_in_progress`) change nothing. §2.4 `failed` mapping:
/// - `admission_timeout` → `admission_timeout`;
/// - `interrupt_timeout` and every `interrupt`-stage error (the interrupt
///   did not complete; the input was not sent) → `interrupt_timeout`;
/// - `session_offline`, `app_server_unavailable` (no connected runtime, no
///   start requested) → `session_offline`;
/// - `native_request_failed`, `steer_unavailable`, `turn_conflict`,
///   `fork_history_signature_rejected`, `invalid_input` and any other
///   refusal of the input itself → `runtimeRefused`;
/// - `event_persistence_unavailable`, `internal_error`, `admission_busy`,
///   `archive_blocks_input` (the route did not complete) → `routeLost`.
///
/// An unknown native outcome (`outcome_unknown`) is `uncertain{outcomeUnknown}`.
pub fn error_step(error: &UserInputExecutionError) -> Option<Step> {
    match error.code.as_str() {
        "idempotency_conflict" | "user_input_in_progress" | "invalid_request" => return None,
        _ => {}
    }
    if error.stage == "interrupt" || error.code == "interrupt_timeout" {
        return Some(Step::Failed("interrupt_timeout"));
    }
    if error.outcome_unknown {
        return Some(Step::Uncertain("outcomeUnknown"));
    }
    Some(Step::Failed(match error.code.as_str() {
        "admission_timeout" => "admission_timeout",
        "session_offline" | "app_server_unavailable" => "session_offline",
        "event_persistence_unavailable" | "internal_error" | "admission_busy" | "archive_blocks_input" => "routeLost",
        _ => "runtimeRefused",
    }))
}

// ---------------------------------------------------------------------------
// Per-thread submit context (offline start, precise wait reason)

#[derive(Default)]
struct SubmitContext {
    /// Routing observed a busy host with no running turn, for this identity.
    idle_host_busy: Option<(String, String)>,
    /// Decision 8: this submit is the offline start's queued input; the
    /// original strategy and whether the agent is paused (no start).
    offline: Option<(UserInputStrategy, bool)>,
    /// Ledger-only waiting reason of `(session, clientUserMessageId)`
    /// (`rideAlongAhead`, `eventLease`, … where the wire says
    /// `delegationLease`, §4.3).
    wait_reason: Option<(String, String, &'static str)>,
    /// A timeline owner record not created yet (`route_accepted`): created
    /// by the route's first write for `(session, clientUserMessageId)`.
    pending: Option<(String, String, NewInput)>,
    /// The waiting reason a record had when `sending` was written
    /// (`native_sending`), restored when native refuses it and the caller
    /// keeps its queue item.
    sending_from: Option<(String, String, Option<String>)>,
}

thread_local! {
    static CONTEXT: RefCell<SubmitContext> = RefCell::new(SubmitContext::default());
}

/// Run an offline-start submit (decision 8): its record is created on
/// `nextUp` with the caller's original strategy and waits `agentOffline`
/// (`paused` while the agent is paused: no start is requested).
pub fn with_offline_start<T>(original: UserInputStrategy, agent_paused: bool, run: impl FnOnce() -> T) -> T {
    CONTEXT.with(|context| context.borrow_mut().offline = Some((original, agent_paused)));
    let result = run();
    CONTEXT.with(|context| context.borrow_mut().offline = None);
    result
}

fn offline_start() -> Option<(UserInputStrategy, bool)> {
    CONTEXT.with(|context| context.borrow().offline)
}

/// Carry the route's own active-turn observation to its first ledger write.
/// Unknown state must not claim idle. No native lookup or host fence here.
pub fn note_host_busy(session: &str, id: &str, idle: bool) {
    if ledger::enabled() {
        CONTEXT.with(|context| context.borrow_mut().idle_host_busy = idle.then(|| (session.into(), id.into())));
    }
}

/// The admission said "a delegation goes first"; record why precisely
/// (ledger only). Taken by the submit's outcome on this thread.
pub fn note_wait_reason(session: &str, client_user_message_id: &str, reason: &'static str) {
    if !ledger::enabled() {
        return;
    }
    CONTEXT.with(|context| {
        context.borrow_mut().wait_reason = Some((session.into(), client_user_message_id.into(), reason));
    });
}

fn take_wait_reason(session: &str, client_user_message_id: &str) -> Option<&'static str> {
    CONTEXT.with(|context| {
        let mut context = context.borrow_mut();
        match &context.wait_reason {
            Some((s, c, reason)) if s == session && c == client_user_message_id => {
                let reason = *reason;
                context.wait_reason = None;
                Some(reason)
            }
            _ => None,
        }
    })
}

/// Ledger reason of the admission lease `session` holds (`None`: none).
/// Reads the shared AskState snapshot; no fence.
pub fn lease_reason(session: &str) -> Option<&'static str> {
    if !ledger::enabled() {
        return None;
    }
    let state = crate::asks::store::AskStore::open_default().ok()?.peek_shared().ok()??;
    state.leases.get(session).map(|lease| match &lease.holder {
        crate::asks::model::LeaseHolder::Ask { .. } => "delegationLease",
        crate::asks::model::LeaseHolder::Event { .. } => "eventLease",
        _ => "turnRunning",
    })
}

// ---------------------------------------------------------------------------
// Writers

/// Apply `step` to the record of `client_user_message_id` and publish every
/// change. Returns the record afterwards (`None`: switch off or no record).
fn run(session: &str, client_user_message_id: &str, step: Step, create: Option<NewInput>) -> Option<InputRecord> {
    run_bound(session, client_user_message_id, step, create, None)
}

/// `nexts` with their `inContext` transition naming native user item `item`
/// (054 §3.2 `timelineItemId`).
pub fn with_item(nexts: Vec<Next>, item: Option<&str>) -> Vec<Next> {
    nexts
        .into_iter()
        .map(|next| match item {
            Some(item) if next.state == InputState::InContext => next.timeline_item(item),
            _ => next,
        })
        .collect()
}

/// [`run`]; an `inContext` transition names native user item `item`.
fn run_bound(session: &str, client_user_message_id: &str, step: Step, create: Option<NewInput>, item: Option<&str>) -> Option<InputRecord> {
    if !ledger::enabled() {
        return None;
    }
    let current = current_record(session, client_user_message_id);
    let mut latest = current.clone();
    let creates = !matches!(step, Step::ClientIdEcho | Step::SteerEcho { .. } | Step::Removed | Step::AlreadyInContext);
    let pending = if current.is_none() && creates { take_pending(session, client_user_message_id) } else { None };
    let nexts = match pending {
        Some(pending) => {
            let idle = CONTEXT.with(|context| context.borrow().idle_host_busy.as_ref()
                .is_some_and(|(s, id)| s == session && id == client_user_message_id));
            pending_plan(&pending, &step, idle)
        },
        None => plan(current.as_ref(), &step, create.as_ref()),
    };
    for next in with_item(nexts, item) {
        match ledger::record(session, client_user_message_id, Expect::ANY, next) {
            Some(transition) => latest = Some(transition.record),
            None => break,
        }
    }
    latest
}

fn current_record(session: &str, client_user_message_id: &str) -> Option<InputRecord> {
    let ledger = ledger::default_ledger().ok()?;
    match ledger.get(session, &[client_user_message_id.to_string()]) {
        Ok(mut result) => result.records.pop(),
        Err(error) => {
            eprintln!("cutex input ledger: {session}/{client_user_message_id}: {error}");
            None
        }
    }
}

/// The record as a result field (`input`, §2.7); `None` with the switch off.
pub fn result_input(session: &str, client_user_message_id: &str) -> Option<Value> {
    if !ledger::enabled() {
        return None;
    }
    current_record(session, client_user_message_id)
        .and_then(|record| serde_json::to_value(record).ok())
}

/// Add `input` to a route result when the switch is on.
pub fn add_result_input(result: &mut Value, session: &str, client_user_message_id: &str) {
    if let Some(input) = result_input(session, client_user_message_id) {
        if let Some(object) = result.as_object_mut() {
            object.insert("input".into(), input);
        }
    }
}

/// `cutex/userInput/routeAccepted` → `received` (§2.9). A repeated identity
/// (a ledger record or a C-010 delivery record exists) creates nothing.
pub fn route_accepted(session: &str, params: &UserInputSubmitParams) {
    if !ledger::enabled() {
        return;
    }
    CONTEXT.with(|context| context.borrow_mut().wait_reason = None);
    let id = &params.client_user_message_id;
    if current_record(session, id).is_some() {
        return;
    }
    if super::user_input::user_input_repository()
        .and_then(|repository| repository.delivery(session, id))
        .ok()
        .flatten()
        .is_some()
    {
        return;
    }
    let (strategy, offline) = match offline_start() {
        Some((original, _)) => (original, true),
        None => (params.strategy, false),
    };
    let held_paused = !offline && strategy != UserInputStrategy::Queue && session_held_paused(session);
    let surface = creation_surface(strategy, offline, held_paused);
    let create = new_input(strategy, &params.input);
    if surface == Surface::Timeline {
        // Created by the route's first write (`run`), using its active-turn
        // observation for a busy host (owner decision 2026-11-04).
        CONTEXT.with(|context| context.borrow_mut().pending = Some((session.into(), id.clone(), create)));
        return;
    }
    run(session, id, Step::Received { surface }, Some(create));
}

fn take_pending(session: &str, client_user_message_id: &str) -> Option<NewInput> {
    CONTEXT.with(|context| {
        let mut context = context.borrow_mut();
        match &context.pending {
            Some((s, c, _)) if s == session && c == client_user_message_id => context.pending.take().map(|(_, _, create)| create),
            _ => None,
        }
    })
}

/// Create the record `route_accepted` left pending: the route is about to
/// interrupt the running turn and wait for it to stop before its first
/// write. Owner rule 2026-11-03 (r274.2 §9.4): on nextUp, since a turn is
/// still running; it moves to the timeline once the stop is accepted and
/// the new turn starts with it (`inContext{turnStartInput}`). No-op when
/// created already or the switch is off.
pub fn materialize(session: &str, client_user_message_id: &str) {
    if !ledger::enabled() {
        return;
    }
    if let Some(create) = take_pending(session, client_user_message_id) {
        if current_record(session, client_user_message_id).is_none() {
            run(session, client_user_message_id, Step::Received { surface: Surface::NextUp }, Some(create));
        }
    }
}

/// §2.6 `held{paused | stopHold}` without `held{hostBusy}` (read without the
/// fence): the session's queue pause, or an agent / project pause.
fn session_held_paused(session: &str) -> bool {
    if super::owner_held::has_session(session).unwrap_or(false) {
        return false;
    }
    let queue_paused = super::user_input::user_input_repository()
        .and_then(|repository| repository.pause_state(session))
        .ok()
        .flatten()
        .is_some();
    queue_paused
        || crate::asks::store::AskStore::open_default()
            .ok()
            .and_then(|store| store.peek_shared().ok().flatten())
            .is_some_and(|state| crate::asks::agent_pause::is_paused(&state, session))
}

/// The submit's outcome (HTTP submit and the held input's later admission):
/// held, queued, a repeated identity or an error. `started` / `steered` are
/// written by [`native_accepted`] when native accepted them; repeating them
/// here changes nothing.
pub fn submit_outcome(
    session: &str,
    params: &UserInputSubmitParams,
    execution: &Result<UserInputSubmitExecution, UserInputExecutionError>,
) {
    if ledger::enabled() {
        write_submit_outcome(session, params, execution);
    }
    // The route is over: nothing it left pending outlives it on this thread.
    clear_route_context();
}

fn write_submit_outcome(
    session: &str,
    params: &UserInputSubmitParams,
    execution: &Result<UserInputSubmitExecution, UserInputExecutionError>,
) {
    let id = &params.client_user_message_id;
    let override_reason = take_wait_reason(session, id);
    let create = Some(new_input(offline_start().map_or(params.strategy, |(original, _)| original), &params.input));
    let step = match execution {
        Ok(execution) => match execution.disposition {
            UserInputDisposition::Held => Step::Held,
            UserInputDisposition::Queued => Step::Waiting(match offline_start() {
                Some((_, true)) => "paused",
                Some((_, false)) => "agentOffline",
                None => match (execution.queued_reason, override_reason) {
                    (Some("delegationLease"), Some(reason)) => reason,
                    // An explicit queue waits for the turn end anyway; a busy
                    // host only delayed the routing decision (its retry
                    // flushes an idle agent's queue), so the owner's own
                    // reason stays.
                    (Some("hostBusy"), _) if params.strategy == UserInputStrategy::Queue => "explicitQueue",
                    (wire, _) => waiting_reason(wire, execution.queue_paused),
                },
            }),
            UserInputDisposition::Started => Step::Started { turn_id: execution.turn_id.clone() },
            UserInputDisposition::Steered => Step::Steered { turn_id: execution.turn_id.clone(), stage: None, generation: None },
            UserInputDisposition::Deduplicated if execution.queue.is_some() => {
                // Already waiting in the queue: a record created just now
                // (switch turned on since) learns that; nothing else changes.
                if current_record(session, id).is_some_and(|record| record.state == InputState::Received) {
                    Step::Waiting(waiting_reason(None, execution.queue_paused))
                } else {
                    return;
                }
            }
            UserInputDisposition::Deduplicated => Step::AlreadyInContext,
        },
        Err(error) => match error_step(error) {
            Some(step) => step,
            None => return,
        },
    };
    run(session, id, step, create);
}

fn clear_route_context() {
    CONTEXT.with(|context| {
        let mut context = context.borrow_mut();
        context.pending = None;
        context.idle_host_busy = None;
        context.sending_from = None;
    });
}

/// Native IO is about to run (decision 3): `sending`, written after the
/// C-010 `sending` reservation and before the request leaves (so a Claude
/// stage that overtakes the reply finds `sending`, §2.11). Remembers the
/// waiting reason it replaced for [`native_refused`].
pub fn native_sending(session: &str, params: &UserInputSubmitParams, steer: bool) {
    if !ledger::enabled() {
        return;
    }
    let id = &params.client_user_message_id;
    let before = current_record(session, id).filter(|record| record.state == InputState::Waiting).and_then(|record| record.reason);
    CONTEXT.with(|context| context.borrow_mut().sending_from = Some((session.into(), id.clone(), before)));
    run(session, id, Step::Sending { steer }, Some(new_input(params.strategy, &params.input)));
}

/// Native explicitly refused an input that stays queued (a flush or queue
/// steer keeps its queue item): `sending → waiting{reason}`; without a
/// reason, the waiting reason it had before `sending` (else
/// `explicitQueue`). No-op unless the record is `sending`.
pub fn native_refused(session: &str, client_user_message_id: &str, reason: Option<&'static str>) {
    if !ledger::enabled() {
        return;
    }
    let before = CONTEXT.with(|context| {
        let mut context = context.borrow_mut();
        match context.sending_from.take() {
            Some((s, c, before)) if s == session && c == client_user_message_id => before,
            other => {
                context.sending_from = other;
                None
            }
        }
    });
    if !current_record(session, client_user_message_id).is_some_and(|record| record.state == InputState::Sending) {
        return;
    }
    let reason = reason
        .or_else(|| before.as_deref().and_then(|before| ledger::WAITING_REASONS.iter().copied().find(|known| *known == before)))
        .unwrap_or("explicitQueue");
    run(session, client_user_message_id, Step::Waiting(reason), None);
}

/// The §2.4 waiting reason of a refusal that re-queues an owner input
/// (`steer_unavailable{reason}` / a busy native thread); `None`: not one
/// of those (the caller decides).
pub fn refusal_reason(error: &UserInputExecutionError) -> Option<&'static str> {
    if error.outcome_unknown {
        return None;
    }
    match error.details.get("reason").and_then(Value::as_str) {
        Some("turnFinishing") => Some("turnFinishing"),
        Some("admissionBusy") => Some("hostBusy"),
        Some("agentPaused") => Some("paused"),
        Some("adapterQueued") => Some("adapterQueued"),
        _ => None,
    }
}

/// Native accepted the input (`send_recorded`, every owner native request):
/// `sending`, then `inContext{turnStartInput}` (turn/start) or
/// `accepted{turnId}` (turn/steer; `accepted[registered]` on Claude, whose
/// steer reply means the adapter registered it), and a Codex echo that
/// arrived first.
pub fn native_accepted(
    session: &str,
    params: &UserInputSubmitParams,
    disposition: UserInputDisposition,
    turn_id: Option<&str>,
    claude: bool,
) {
    if !ledger::enabled() {
        return;
    }
    let id = &params.client_user_message_id;
    let turn_id = turn_id.map(str::to_string);
    let steered = disposition == UserInputDisposition::Steered;
    let step = if steered {
        Step::Steered { turn_id, stage: claude.then_some(Stage::Registered), generation: session_generation(session) }
    } else {
        Step::Started { turn_id }
    };
    // Serialized with `observe_user_item`, so an echo is either seen after
    // the acceptance or remembered before it, never lost in between.
    let mut early = echoes();
    let echoed = take_echo(&mut early, session, id);
    // The echo is the native user item: its id is the record's timeline
    // item (054 §3.2), for a turn/start input too.
    let (item, echo_turn) = echoed.clone().unwrap_or_default();
    run_bound(session, id, step, Some(new_input(params.strategy, &params.input)), item.as_deref());
    if echoed.is_some() && steered {
        let echo = if claude { Step::SteerEcho { turn_id: echo_turn, item: item.clone() } } else { Step::ClientIdEcho };
        run_bound(session, id, echo, None, item.as_deref());
    }
}

/// The session's current runtime generation (the session record's; the
/// settle engine's runtime view uses the same one), `None` when unknown.
fn session_generation(session: &str) -> Option<u64> {
    let store = crate::session::store::load_cutex_session_store_shared().ok()?;
    store.sessions.get(session).map(|record| record.runtime_generation)
}

/// Native IO ran but its outcome is unknown (C-010 `unknown`).
pub fn native_unknown(session: &str, params: &UserInputSubmitParams) {
    if !ledger::enabled() {
        return;
    }
    run(session, &params.client_user_message_id, Step::Uncertain("outcomeUnknown"), Some(new_input(params.strategy, &params.input)));
}

/// A queued item native already has (history / receipt): only its queue item
/// is removed (flush cleanup).
pub fn already_in_context(session: &str, client_user_message_id: &str) {
    run(session, client_user_message_id, Step::AlreadyInContext, None);
}

/// A queued input whose flush native refused outright (`cutex/userInput/failed`;
/// the queue item stays): `sending → failed{code}` (§2.4 mapping of
/// [`error_step`]). An unknown outcome was written by [`native_unknown`].
pub fn native_failed(session: &str, client_user_message_id: &str, error: &UserInputExecutionError) {
    if error.outcome_unknown {
        return;
    }
    if let Some(step) = error_step(error) {
        run(session, client_user_message_id, step, None);
    }
}

/// A queued owner input whose agent is being started for it (decision 8,
/// an offline `queue/steer`): `waiting{agentOffline}`.
pub fn offline_waiting(session: &str, client_user_message_id: &str) {
    run(session, client_user_message_id, Step::Waiting("agentOffline"), None);
}

/// A held input passed `owner_held::HELD_BUDGET` without admission.
pub fn held_expired(session: &str, client_user_message_id: &str) {
    run(session, client_user_message_id, Step::Failed("admission_timeout"), None);
}

/// `cutex/userInput/queue/remove` removed the owner's queue item.
pub fn queue_removed(session: &str, client_user_message_id: &str) -> Option<InputRecord> {
    run(session, client_user_message_id, Step::Removed, None)
}

/// `cutex/userInput/queue/merge`: the merged-away inputs become
/// `superseded{mergedInsert, supersededBy: survivor}` (surface none), and the
/// survivor's record (same state and reason) names what it absorbed in
/// `details.mergedFrom`. ONE `record_batch` (Director decision 4): one
/// transaction, one `stateRevision` for every changed record, so a client
/// never sees an intermediate. Returns every affected record, survivor first
/// (those the ledger knows).
pub fn queue_merged(session: &str, survivor: &str, merged_away: &[String]) -> Vec<InputRecord> {
    if !ledger::enabled() {
        return Vec::new();
    }
    let mut ids = vec![survivor.to_string()];
    ids.extend(merged_away.iter().cloned());
    let read = || ledger::default_ledger().ok().and_then(|ledger| ledger.get(session, &ids).ok()).map(|result| result.records);
    let Some(before) = read() else { return Vec::new() };
    let batch = merge_batch(&before, survivor, merged_away);
    if batch.is_empty() {
        return before;
    }
    match ledger::record_batch(session, batch) {
        Some(_) => read().unwrap_or_default(),
        None => before,
    }
}

/// The transitions of one merge over the records the ledger has (pure):
/// the survivor first, then each merged-away record still open. Records the
/// ledger does not know (from before the switch) are left out.
pub fn merge_batch(records: &[InputRecord], survivor: &str, merged_away: &[String]) -> Vec<(String, Expect, Next)> {
    let mut batch = Vec::new();
    let find = |id: &str| records.iter().find(|record| record.input_id == id);
    let absorbed: Vec<&String> = merged_away.iter().filter(|id| find(id).is_some_and(|record| record.state == InputState::Waiting)).collect();
    if let Some(record) = find(survivor).filter(|record| record.state == InputState::Waiting && !absorbed.is_empty()) {
        let mut merged: Vec<Value> = record.details.get("mergedFrom").and_then(Value::as_array).cloned().unwrap_or_default();
        merged.extend(absorbed.iter().map(|id| json!(id)));
        let mut details = if record.details.is_object() { record.details.clone() } else { json!({}) };
        details["mergedFrom"] = Value::Array(merged);
        let mut next = Next::to(record.state).details(details);
        if let Some(reason) = &record.reason {
            next = next.reason(reason);
        }
        batch.push((survivor.to_string(), Expect::ANY, next));
    }
    for id in absorbed {
        batch.push((id.clone(), Expect::ANY, Next::superseded("mergedInsert").superseded_by(survivor.to_string())));
    }
    batch
}

// ---------------------------------------------------------------------------
// Codex clientId echo (native user item)

/// Echoes seen before their `turn/steer` response was recorded (bounded;
/// the oldest is dropped first).
const MAX_EARLY_ECHOES: usize = 256;

/// (session, clientId, native item id, native turn id).
type Echo = (String, String, Option<String>, Option<String>);

fn echoes() -> MutexGuard<'static, VecDeque<Echo>> {
    static ECHOES: OnceLock<Mutex<VecDeque<Echo>>> = OnceLock::new();
    ECHOES.get_or_init(Default::default).lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The remembered echo of `id`: (its native item id, its turn), when known.
fn take_echo(early: &mut VecDeque<Echo>, session: &str, id: &str) -> Option<(Option<String>, Option<String>)> {
    let index = early.iter().position(|(s, c, _, _)| s == session && c == id)?;
    early.remove(index).map(|(_, _, item, turn)| (item, turn))
}

/// Whether `raw` is a native user item (`item/started` / `item/completed`
/// with `item.type == "userMessage"`) of either runtime: a Codex echo, or
/// (r274.2 §9.1) a Claude steer echo / turn input.
pub fn is_owner_user_item(raw: &Value) -> bool {
    matches!(raw.get("method").and_then(Value::as_str), Some("item/started" | "item/completed"))
        && raw.pointer("/params/item/type").and_then(Value::as_str) == Some("userMessage")
}

/// A native user item carrying `client_id` (054 §2.9).
///
/// Codex: an `accepted` owner input is now `inContext{clientIdEcho}` (an
/// `uncertain` one too: late evidence).
///
/// Claude (`claude`, r274.2 §9.1): the item is shown, not a context
/// receipt: an `accepted` input moves to the timeline and stays `accepted`
/// ([`Step::SteerEcho`]); `inContext` waits for `contextPersisted`. An
/// echo is ignored while the record is `sending{interruptSend}` (the steer
/// of the turn being interrupted; its resend is another native item).
///
/// Both: an echo that overtakes its `turn/steer` response is remembered and
/// applied right after the acceptance is recorded; a `turn/start` input
/// becomes `inContext{turnStartInput}` and drops it.
///
/// `item_id`: the native user item's id, the record's timeline item (054
/// §3.2; the desktop also joins owner items on the clientId). A record the
/// turn/start already put in context is bound to it once. `turn_id`: the
/// item's turn (a Claude steer that slipped to the next turn).
pub fn observe_user_item(session: &str, client_id: &str, item_id: Option<&str>, claude: bool, turn_id: Option<&str>) {
    if !ledger::enabled() {
        return;
    }
    let item_id = item_id.filter(|item| !item.is_empty());
    let mut early = echoes();
    let Some(record) = current_record(session, client_id).filter(|record| record.kind == InputKind::Owner) else {
        return;
    };
    match record.state {
        InputState::Accepted if claude => {
            let step = Step::SteerEcho { turn_id: turn_id.map(str::to_string), item: item_id.map(str::to_string) };
            run_bound(session, client_id, step, None, None);
        }
        // Not a receipt on Claude: an uncertain record stays uncertain.
        InputState::Uncertain if claude => {}
        InputState::Sending if claude && record.reason.as_deref() == Some(super::input_interrupt_send::SENDING_REASON) => {}
        InputState::Accepted | InputState::Uncertain => {
            run_bound(session, client_id, Step::ClientIdEcho, None, item_id);
        }
        InputState::Received | InputState::Held | InputState::Waiting | InputState::Sending => {
            if !early.iter().any(|(s, c, _, _)| s == session && c == client_id) {
                if early.len() >= MAX_EARLY_ECHOES {
                    early.pop_front();
                }
                early.push_back((session.to_string(), client_id.to_string(), item_id.map(str::to_string), turn_id.map(str::to_string)));
            }
        }
        InputState::InContext if record.timeline_item_id.is_none() => {
            if let Some(item) = item_id {
                bind(&record, item);
            }
        }
        _ => {}
    }
}

/// Bind an `inContext` owner record to its native user item (metadata
/// only; never rebinds a bound record).
fn bind(record: &InputRecord, item: &str) {
    let Ok(store) = ledger::default_ledger() else { return };
    match store.bind_timeline_item(&record.cutex_session_id, &record.input_id, Expect::rev(record.rev), item, None) {
        Ok(transition) => {
            if let Err(error) = ledger::publish(&transition) {
                eprintln!("cutex input ledger: publish {}/{}: {error:#}", record.cutex_session_id, record.input_id);
            }
        }
        Err(ledger::LedgerError::Conflict { .. }) => {}
        Err(error) => eprintln!("cutex input ledger: bind {}/{}: {error}", record.cutex_session_id, record.input_id),
    }
}

// ---------------------------------------------------------------------------
// `cutex/userInput/queue/list` fields (§2.7)

/// With the switch on: `stateRevision`, `ledgerEpoch`, `inputs` (this
/// session's unsettled records on `nextUp` / `pendingNotes`), `delivery`
/// (when one is stored), and `admission.stale: true` for a committed
/// (`admissionFresh: false`) admission. Read without the fence.
pub fn add_queue_list_fields(result: &mut Value, session: &str, admission_fresh: bool) {
    if !ledger::enabled() {
        return;
    }
    if !admission_fresh {
        if let Some(admission) = result.get_mut("admission").and_then(Value::as_object_mut) {
            admission.insert("stale".into(), Value::Bool(true));
        }
    }
    let list = ledger::default_ledger()
        .map_err(ledger::LedgerError::Storage)
        .and_then(|ledger| ledger.list(session, ledger::ListQuery { since_revision: None, include_settled_secs: 0 }));
    let list = match list {
        Ok(list) => list,
        Err(error) => {
            eprintln!("cutex input ledger: queue/list {session}: {error}");
            return;
        }
    };
    let inputs: Vec<Value> = list.records.into_iter()
        .filter(|record| !record.state.is_settled() && matches!(record.surface, Surface::NextUp | Surface::PendingNotes))
        .filter_map(|record| serde_json::to_value(record).ok())
        .collect();
    let Some(object) = result.as_object_mut() else { return };
    object.insert("stateRevision".into(), json!(list.state_revision));
    object.insert("ledgerEpoch".into(), json!(list.ledger_epoch));
    object.insert("inputs".into(), Value::Array(inputs));
    if let Some(delivery) = list.delivery {
        object.insert("delivery".into(), delivery);
    }
}

// ---------------------------------------------------------------------------
// S7 test hook (debug builds only)

/// `POST /v2/test/hostFence/hold {"seconds": N}` (root credential; debug
/// builds only, and only while `CUTEX_TEST_PRIVATE_HOME` is set): a
/// background thread takes the host authority fence (the agent management
/// mutation lock the owner admission worker uses), answers once it holds it,
/// and releases it after N seconds (1..=600). Owner submits meanwhile see a
/// busy host (`held{hostBusy}` with `allowHeld`, else `waiting{hostBusy}`).
#[cfg(debug_assertions)]
pub const TEST_HOLD_FENCE_PATH: &str = "/v2/test/hostFence/hold";

#[cfg(debug_assertions)]
pub fn handle_test_hold_fence(
    stream: &mut std::net::TcpStream,
    request: &crate::http::server::SimpleHttpRequest,
) -> anyhow::Result<()> {
    use super::server::write_v2_error;
    if std::env::var_os("CUTEX_TEST_PRIVATE_HOME").is_none_or(|home| home.is_empty()) {
        return write_v2_error(stream, 404, "Not Found", "not_found", "test hooks need CUTEX_TEST_PRIVATE_HOME", false, json!({}));
    }
    if request.method != "POST" {
        return write_v2_error(stream, 405, "Method Not Allowed", "method_not_allowed", "use POST", false, json!({}));
    }
    let seconds = serde_json::from_slice::<Value>(&request.body).ok()
        .filter(|body| body.as_object().is_some_and(|object| object.len() == 1))
        .and_then(|body| body.get("seconds").and_then(Value::as_u64))
        .filter(|seconds| (1..=600).contains(seconds));
    let Some(seconds) = seconds else {
        return write_v2_error(stream, 400, "Bad Request", "invalid_request", "body must be exactly {\"seconds\": 1..=600}", false, json!({}));
    };
    let (acquired, wait) = std::sync::mpsc::channel::<Result<(), String>>();
    std::thread::Builder::new().name("cutex-test-host-fence".into()).spawn(move || {
        let fence = crate::agent_management::AgentManagementStore::open_default()
            .map_err(|error| format!("{error:#}"))
            .and_then(|store| store.lock_mutations().map_err(|error| format!("{error}")));
        match fence {
            Ok(fence) => {
                let _ = acquired.send(Ok(()));
                std::thread::sleep(std::time::Duration::from_secs(seconds));
                drop(fence);
            }
            Err(error) => {
                let _ = acquired.send(Err(error));
            }
        }
    })?;
    match wait.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(Ok(())) => crate::http::server::write_json_response(stream, 200, "OK", &json!({
            "held": true,
            "seconds": seconds,
            "until": (chrono::Utc::now() + chrono::Duration::seconds(seconds as i64)).to_rfc3339(),
        })),
        Ok(Err(error)) => write_v2_error(stream, 503, "Service Unavailable", "fence_unavailable", &error, true, json!({})),
        Err(_) => write_v2_error(stream, 503, "Service Unavailable", "fence_busy", "the host fence was not free within 30 s", true, json!({})),
    }
}

#[cfg(test)]
#[path = "input_ledger_owner_tests.rs"]
mod tests;
