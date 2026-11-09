//! Runtime half of `cutex/input/interruptSend` (r274 interrupt-send
//! contract; the route and the pure checks are in
//! `management/v2/input_interrupt_send.rs`).
//!
//! Everything runs under the session input lane (no submit, flush, queue
//! steer or held admission of this session interleaves) and never waits for
//! the host authority fence: the C-010 reservation only TRIES it.
//!
//! Exactly once, same id, never a text copy:
//! - a steered owner input (`accepted` into turn T) is in the runtime's
//!   pending input of T. Both runtimes drop a steer T never read when T is
//!   interrupted (cute-codex `abort_all_tasks` / `finish_turn_abort` →
//!   `input_queue.clear_pending`; the Claude adapter, claude-r18,
//!   `Store.drop_unread_steers` at the `turn/interrupt` request). A steer T
//!   read before the abort is a `userMessage` item of T with the clientId
//!   (Codex records drained pending input before it samples again; the
//!   Claude adapter writes the hook-read steer as a `steered` item of T).
//!   So after T stopped, ONE `thread/items/list` of T decides: found → it is
//!   in context, nothing is sent; absent → it was dropped and goes out once
//!   as `turn/start` with the same clientUserMessageId.
//! - a queued (`waiting`) or held owner input never reached the runtime: its
//!   own store keeps it until the `turn/start` is accepted, then forgets it.
//! - the C-010 delivery record is reserved (`sending`) before the
//!   `turn/start`, so no flush, retry or held admission sends the id at the
//!   same time; the runtimes do not deduplicate `turn/start` by
//!   clientUserMessageId (the Claude adapter gives every `turn/start` a new
//!   inbox message id), so cutex's single send is the guarantee.

use cutex::management::v2::input_interrupt_send::{self as isend, InterruptSendRequest, Outcome};
use cutex::management::v2::input_ledger::{
    self as il, Evidence, Expect, InputLedger, InputRecord, InputState, LedgerError, Next, Stage,
};
use cutex::management::v2::owner_held::{self, HeldOwnerInput};
use cutex::management::v2::user_input::{
    parse_user_input_submit_params, InputDelivery, QueueEnqueueDecision, UserInputQueueItem,
    UserInputRepository, UserInputSubmitParams,
};

use super::*;

/// What the live runtime says about the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LiveTurn {
    pub thread_id: String,
    pub generation: u64,
    pub active_turn: Option<String>,
}

/// One `thread/items/list` of a turn: does it hold the owner's item?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TurnCheck {
    Found,
    Absent,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum InterruptFailure {
    /// The runtime answered with an error (refused).
    Refused(String),
    /// Transport / timeout: the interrupt may or may not have happened.
    Timeout(String),
    /// No runtime / connection gone.
    Offline(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Reservation {
    Reserved,
    /// The host authority fence is busy (or the agent is paused): the held
    /// admission worker sends it later.
    Busy,
    /// The delivery record changed underneath (another sender).
    Changed,
}

/// The runtime and the stores (live, or a test fake).
pub(super) trait Ops {
    fn ledger(&self) -> anyhow::Result<&InputLedger>;
    fn repository(&self) -> &UserInputRepository;
    /// The session's §2.6 delivery now.
    fn delivery(&self, session: &str) -> Option<Value>;
    /// The connected runtime (`None`: not connected).
    fn live(&self, session: &str) -> Option<LiveTurn>;
    fn claude(&self, session: &str) -> bool;
    fn turn_has(&self, session: &str, thread: &str, turn: &str, client_id: &str) -> TurnCheck;
    fn interrupt(&self, session: &str, thread: &str, turn: &str) -> Result<(), InterruptFailure>;
    fn wait_stopped(&self, session: &str, turn: &str) -> Result<(), InterruptFailure>;
    /// C-010: `expected` → `sending` under a TRY of the host fence.
    fn reserve(&self, session: &str, id: &str, expected: Option<&InputDelivery>, sending: InputDelivery) -> anyhow::Result<Reservation>;
    fn send_start(&self, command: &UserInputSubmitCommand) -> Result<UserInputSubmitExecution, UserInputExecutionError>;
    fn held_find(&self, session: &str, id: &str) -> anyhow::Result<Option<HeldOwnerInput>>;
    fn held_finish(&self, session: &str, id: &str) -> anyhow::Result<()>;
    /// Hold `params` for the owner admission worker (replacing the params of
    /// an input already held).
    fn held_park(&self, session: &str, thread: &str, request_id: &str, params: Value) -> anyhow::Result<()>;
    fn publish(&self, transition: &il::Transition);
    fn publish_delivery(&self, session: &str, change: &il::DeliveryChange);
    fn append_event(&self, session: &str, correlation: EventCorrelation, method: &str, params: Value) -> anyhow::Result<()>;
}

/// Where the owner input is kept, and what goes out.
#[derive(Debug, Clone)]
enum Source {
    /// Steered into the running turn (`accepted`).
    Steered,
    Queued(UserInputQueueItem),
    Held,
}

#[derive(Debug, Clone)]
struct Input {
    source: Source,
    params: UserInputSubmitParams,
    request_id: String,
}

fn ledger_failure(error: LedgerError) -> UserInputExecutionError {
    execution_error_message("route", "event_persistence_unavailable", &error.to_string(), true, false)
}

fn submit_params(value: &Value) -> Option<UserInputSubmitParams> {
    let mut value = value.clone();
    if let Some(object) = value.as_object_mut() {
        object.remove("allowHeld");
        object.remove("startIfOffline");
    }
    parse_user_input_submit_params(&value).ok()
}

/// The input of `record` from its own store (the original submit params of
/// a steer, kept by C5 for `cutex/input/retry`). Missing → not interruptible
/// (nothing is done).
fn locate(ops: &dyn Ops, ledger: &InputLedger, session: &str, record: &InputRecord) -> Result<Input, UserInputExecutionError> {
    let id = &record.input_id;
    let missing = || isend::not_interruptible(record);
    let new_request = || format!("input-interrupt-send-{}", Uuid::new_v4());
    match record.state {
        InputState::Accepted => {
            let payload = ledger.payload(session, id).map_err(ledger_failure)?.ok_or_else(missing)?;
            let params = submit_params(&payload).filter(|params| params.client_user_message_id == *id).ok_or_else(missing)?;
            Ok(Input { source: Source::Steered, params, request_id: new_request() })
        }
        InputState::Waiting => {
            let (_, items) = ops.repository().list(session).map_err(queue_persistence_error)?;
            let item = items.into_iter().find(|item| item.client_user_message_id == *id).ok_or_else(missing)?;
            let params = UserInputSubmitParams {
                join_delegation: None,
                client_user_message_id: id.clone(),
                origin: item.origin.clone(),
                strategy: UserInputStrategy::Auto,
                input: item.input.clone(),
                expected_turn_id: None,
            };
            Ok(Input { source: Source::Queued(item), params, request_id: new_request() })
        }
        InputState::Held => {
            let held = ops.held_find(session, id).map_err(queue_persistence_error)?.ok_or_else(missing)?;
            let params = submit_params(&held.params).ok_or_else(missing)?;
            Ok(Input { source: Source::Held, params, request_id: held.management_request_id })
        }
        _ => Err(missing()),
    }
}

/// One ledger transition on the observed rev; returns the new rev.
fn step(ops: &dyn Ops, session: &str, id: &str, rev: u64, next: Next) -> Option<u64> {
    let ledger = ops.ledger().ok()?;
    match ledger.transition(session, id, Expect::rev(rev), next) {
        Ok(transition) => {
            ops.publish(&transition);
            Some(transition.record.rev)
        }
        Err(error) => {
            eprintln!("cutex/input/interruptSend: {session}/{id}: {error}");
            None
        }
    }
}

/// The runtime half (see the module docs). Errors only before any side
/// effect; once the first transition is written, every failure is a
/// record state (contract §5).
pub(super) fn run(ops: &dyn Ops, session: &str, request: &InterruptSendRequest) -> Result<Outcome, UserInputExecutionError> {
    let ledger = ops.ledger().map_err(|error| execution_error("route", "event_persistence_unavailable", error, true, false))?;
    let id = request.input_id.clone();
    let current = ledger.get(session, std::slice::from_ref(&id)).map_err(ledger_failure)?.records.pop();
    let delivery = ops.delivery(session);
    let record = isend::precheck(current.as_ref(), delivery.as_ref(), request)?.clone();
    let delivery = delivery.expect("precheck saw a running turn");
    let turn = request.expected_turn_id.clone();
    // The connected runtime must agree with the delivery the CAS used.
    let live = ops.live(session)
        .filter(|live| live.active_turn.as_deref() == Some(turn.as_str()) && live.generation == request.expected_runtime_generation)
        .ok_or_else(|| isend::conflict(Some(&record), Some(&delivery)))?;
    let thread = live.thread_id.clone();
    let input = locate(ops, ledger, session, &record)?;
    let steered = matches!(input.source, Source::Steered);

    // 2. Already in context: no interrupt, nothing resent.
    if steered {
        let offered = ops.claude(session) && record.stage == Some(Stage::Offered);
        if offered || ops.turn_has(session, &thread, &turn, &id) == TurnCheck::Found {
            let transition = ledger
                .transition(session, &id, Expect::rev(record.rev), Next::in_context(Evidence::HistoryRead))
                .map_err(|error| match error {
                    LedgerError::Conflict { .. } => isend::conflict(Some(&record), Some(&delivery)),
                    other => ledger_failure(other),
                })?;
            ops.publish(&transition);
            return Ok(Outcome::AlreadyInContext);
        }
    }

    // 3. The first transition and the delivery change of the interrupt,
    // one revision.
    let mark = json!({"interruptedTurnId": turn});
    let first = Next::sending().reason(isend::SENDING_REASON).details(mark.clone());
    let (transitions, change) = ledger
        .transition_batch_with_delivery(session, vec![(id.clone(), Expect::rev(record.rev), first)], &isend::finishing(&delivery))
        .map_err(|error| match error {
            LedgerError::Conflict { .. } => isend::conflict(Some(&record), Some(&delivery)),
            other => ledger_failure(other),
        })?;
    ops.publish_delivery(session, &change);
    for transition in &transitions {
        ops.publish(transition);
    }
    let mut rev = transitions.first().map_or(record.rev, |transition| transition.record.rev);
    let _interrupting = isend::Interrupting::start(session);
    let flow = Flow { ops, session, id: &id, thread: &thread, turn: &turn, input: &input, mark: &mark };

    // 4. Interrupt the turn and wait until it stopped.
    let stopped = ops.interrupt(session, &thread, &turn).and_then(|()| ops.wait_stopped(session, &turn));
    match stopped {
        Ok(()) => {}
        Err(InterruptFailure::Offline(_)) => return Ok(flow.offline(&mut rev, steered, false)),
        Err(InterruptFailure::Refused(_)) => return Ok(flow.failed(rev, "runtimeRefused", steered, true)),
        Err(InterruptFailure::Timeout(_)) => return Ok(flow.failed(rev, "interrupt_timeout", steered, false)),
    }

    // 5. A steer the stopped turn read stays read; one it did not read was
    // dropped by the runtime.
    if steered {
        match ops.turn_has(session, &thread, &turn, &id) {
            TurnCheck::Found => {
                step(ops, session, &id, rev, Next::in_context(Evidence::HistoryRead).turn(turn.clone()));
                return Ok(Outcome::Interrupted { turn_id: turn, started: false });
            }
            // Dropped, and the bundle it carried with it. That bundle stays
            // reserved (057 §6): this goes out alone (`OwnerCarries::goes_alone`).
            TurnCheck::Absent if !record.carries.is_empty() => {
                let mut alone = Next::sending().reason(isend::SENDING_REASON).details(mark.clone());
                alone.carries = Some(Vec::new());
                if let Some(next) = step(ops, session, &id, rev, alone) {
                    rev = next;
                }
            }
            TurnCheck::Absent => {}
            // Unprovable either way: never resent blindly (owner decision 3).
            TurnCheck::Unavailable => {
                flow.unknown_delivery();
                step(ops, session, &id, rev, Next::uncertain("turnInterrupted"));
                return Ok(Outcome::Interrupted { turn_id: turn, started: false });
            }
        }
    }

    // 6. Reserve the C-010 delivery (a TRY of the host fence).
    let Some(live) = ops.live(session) else { return Ok(flow.offline(&mut rev, steered, true)) };
    let prior = ops.repository().delivery(session, &id).map_err(queue_persistence_error)?;
    let sending = InputDelivery::Sending { thread_id: live.thread_id.clone(), since: Utc::now().to_rfc3339() };
    let reservation = ops.reserve(session, &id, prior.as_ref(), sending.clone());
    match reservation {
        Ok(Reservation::Reserved) => {}
        Ok(Reservation::Busy) => return Ok(flow.held(rev, prior.as_ref())),
        Ok(Reservation::Changed) | Err(_) => {
            // Someone else holds the id's delivery now (it resolves it); the
            // record stays `sending` until that sender's outcome.
            return Ok(Outcome::Interrupted { turn_id: turn, started: false });
        }
    }

    // 7. The same clientUserMessageId as the next turn's first input.
    let command = UserInputSubmitCommand {
        management_request_id: input.request_id.clone(),
        cutex_session_id: session.to_string(),
        thread_id: live.thread_id.clone(),
        params: input.params.clone(),
    };
    match ops.send_start(&command) {
        Ok(execution) => {
            let accepted = InputDelivery::Accepted {
                thread_id: live.thread_id.clone(),
                turn_id: execution.turn_id.clone(),
                accepted_at: Utc::now().to_rfc3339(),
            };
            if let Err(error) = ops.repository().set_delivery(session, &id, Some(&sending), Some(accepted)) {
                eprintln!("cutex/input/interruptSend: {session}/{id}: delivery record: {error:#}");
            }
            let mut next = Next::in_context(Evidence::TurnStartInput);
            if let Some(new_turn) = execution.turn_id.as_deref().filter(|turn| !turn.is_empty()) {
                next = next.turn(new_turn.to_string());
            }
            step(ops, session, &id, rev, next);
            flow.sent(&command, &execution);
            Ok(Outcome::Interrupted { turn_id: turn, started: true })
        }
        Err(error) if error.outcome_unknown => {
            let unknown = InputDelivery::Unknown { thread_id: live.thread_id.clone(), since: Utc::now().to_rfc3339() };
            let _ = ops.repository().set_delivery(session, &id, Some(&sending), Some(unknown));
            step(ops, session, &id, rev, Next::uncertain("outcomeUnknown"));
            Ok(Outcome::Interrupted { turn_id: turn, started: false })
        }
        Err(error) => {
            let _ = ops.repository().set_delivery(session, &id, Some(&sending), None);
            if matches!(error.code.as_str(), "app_server_unavailable" | "session_offline") {
                return Ok(flow.offline(&mut rev, false, true));
            }
            Ok(flow.failed(rev, "runtimeRefused", false, false))
        }
    }
}

struct Flow<'a> {
    ops: &'a dyn Ops,
    session: &'a str,
    id: &'a str,
    thread: &'a str,
    turn: &'a str,
    input: &'a Input,
    mark: &'a Value,
}

impl Flow<'_> {
    /// The steer's delivery after an interrupt that may have dropped it:
    /// the C-010 rules decide a later resend (history first).
    fn unknown_delivery(&self) {
        let repository = self.ops.repository();
        let Ok(prior) = repository.delivery(self.session, self.id) else { return };
        let unknown = InputDelivery::Unknown { thread_id: self.thread.to_string(), since: Utc::now().to_rfc3339() };
        let _ = repository.set_delivery(self.session, self.id, prior.as_ref(), Some(unknown));
    }

    /// Remove the queue item of a queued input (`reason` on the wire).
    fn remove_queued(&self, reason: &str) {
        let Source::Queued(item) = &self.input.source else { return };
        let repository = self.ops.repository();
        match repository.remove(self.session, &item.queue_id, item.revision) {
            Ok(Some(removed)) => {
                let queue_revision = repository.list(self.session).map(|(revision, _)| revision).unwrap_or(0);
                if let Err(error) = append_queue_removed_event(
                    &|session: &str, correlation: EventCorrelation, method: &str, params: Value| {
                        self.ops.append_event(session, correlation, method, params)
                    },
                    self.session,
                    &self.input.request_id,
                    &removed,
                    reason,
                    queue_revision,
                ) {
                    eprintln!("cutex/input/interruptSend: {}: queueRemoved: {error:#}", self.session);
                }
            }
            Ok(None) => {}
            Err(error) => eprintln!("cutex/input/interruptSend: {}: queue item: {error:#}", self.session),
        }
    }

    /// Interrupt refused or timed out: `failed{reason}` (retry resends the
    /// same id). A refused interrupt leaves the turn running with the steer
    /// in it (the C-010 receipt stays: a retry finds it delivered); a
    /// timed-out one may have dropped it (C-010 `unknown`: history decides).
    fn failed(&self, rev: u64, reason: &str, steered: bool, refused: bool) -> Outcome {
        if steered && !refused {
            self.unknown_delivery();
        }
        self.remove_queued("failed");
        if matches!(self.input.source, Source::Held) {
            let _ = self.ops.held_finish(self.session, self.id);
        }
        step(self.ops, self.session, self.id, rev, Next::failed(reason));
        Outcome::Interrupted { turn_id: self.turn.to_string(), started: false }
    }

    /// The runtime went away: `waiting{agentOffline}` first in the queue
    /// (owner decision 8: the route starts the agent). `proven`: the
    /// stopped turn showed the steer absent (else the history decides at
    /// the flush, C-010).
    fn offline(&self, rev: &mut u64, steered: bool, proven: bool) -> Outcome {
        let repository = self.ops.repository();
        let item = match &self.input.source {
            Source::Queued(item) => Some(item.clone()),
            source => {
                if matches!(source, Source::Held) {
                    let _ = self.ops.held_finish(self.session, self.id);
                }
                let mut params = self.input.params.clone();
                params.strategy = UserInputStrategy::Queue;
                // The steer's receipt must not deduplicate the flush.
                let prior = repository.delivery(self.session, self.id).ok().flatten();
                let _ = repository.set_delivery(self.session, self.id, prior.as_ref(), None);
                match repository.enqueue(self.session, self.thread, &self.input.request_id, &params) {
                    Ok(QueueEnqueueDecision::Queued(item) | QueueEnqueueDecision::Deduplicated(item)) => Some(item),
                    _ => None,
                }
            }
        };
        if let Some(item) = &item {
            let _ = repository.mark_bypass_pause(self.session, &item.queue_id);
            first_in_queue(repository, self.session, &item.queue_id);
            if steered && !proven {
                // Read the history once before sending: a turn id the thread
                // does not list makes C-010 resend it unless it is visible.
                let unproven = InputDelivery::Accepted {
                    thread_id: self.thread.to_string(),
                    turn_id: Some(format!("interruptSend:{}", self.turn)),
                    accepted_at: Utc::now().to_rfc3339(),
                };
                let _ = repository.set_delivery(self.session, self.id, None, Some(unproven));
            }
        }
        if let Some(next) = step(self.ops, self.session, self.id, *rev, Next::waiting("agentOffline")) {
            *rev = next;
        }
        Outcome::AgentOffline { turn_id: Some(self.turn.to_string()) }
    }

    /// The host authority fence is busy: `held{hostBusy}` with
    /// `details.interruptSend`; the owner admission worker runs it with
    /// strategy `interrupt` (a turn started meanwhile is interrupted too; an
    /// idle agent gets it as `turn/start`, never behind the queue).
    fn held(&self, rev: u64, prior: Option<&InputDelivery>) -> Outcome {
        let params = json!({
            "clientUserMessageId": self.id,
            "origin": self.input.params.origin,
            "strategy": UserInputStrategy::Interrupt,
            "input": self.input.params.input,
        });
        if matches!(self.input.source, Source::Steered) {
            // Proven absent from the interrupted turn: the admission sends it.
            let _ = self.ops.repository().set_delivery(self.session, self.id, prior, None);
        }
        if let Err(error) = self.ops.held_park(self.session, self.thread, &self.input.request_id, params) {
            eprintln!("cutex/input/interruptSend: {}: hold: {error:#}", self.session);
        }
        self.remove_queued("submitted");
        step(self.ops, self.session, self.id, rev,
            Next::held("hostBusy").details(json!({"interruptSend": self.mark})));
        Outcome::Held { turn_id: Some(self.turn.to_string()) }
    }

    /// `turn/start` accepted: the input's own store forgets it, the usual
    /// `cutex/userInput/submitted` (and `queueRemoved`) go out.
    fn sent(&self, command: &UserInputSubmitCommand, execution: &UserInputSubmitExecution) {
        if matches!(self.input.source, Source::Held) {
            let _ = self.ops.held_finish(self.session, self.id);
        }
        let queue_id = match &self.input.source {
            Source::Queued(item) => Some(item.queue_id.clone()),
            _ => None,
        };
        let submitted = self.ops.append_event(
            self.session,
            EventCorrelation {
                thread_id: Some(command.thread_id.clone()),
                turn_id: execution.turn_id.clone(),
                client_user_message_id: Some(self.id.to_string()),
                management_request_id: Some(command.management_request_id.clone()),
                queue_id,
                native_request_id: execution.native_request_id.clone(),
                ..Default::default()
            },
            "cutex/userInput/submitted",
            json!({
                "managementRequestId": command.management_request_id,
                "clientUserMessageId": self.id,
                "origin": command.params.origin,
                "input": command.params.input,
                "disposition": execution.disposition,
                "nativeRequestId": execution.native_request_id,
                "nativeMethod": execution.native_method,
                "turnId": execution.turn_id,
                "appServerAccepted": true,
                "submittedAt": Utc::now().to_rfc3339(),
            }),
        );
        if let Err(error) = submitted {
            eprintln!("cutex/input/interruptSend: {}: submitted event: {error:#}", self.session);
        }
        self.remove_queued("submitted");
    }
}

/// Move a queued owner item before every other owner item (best effort).
fn first_in_queue(repository: &UserInputRepository, session: &str, queue_id: &str) {
    let Ok(snapshot) = repository.admission(session) else { return };
    let Some(head) = snapshot.items.iter().find(|item| item.kind == "owner" && item.id != queue_id) else { return };
    let ours = snapshot.items.iter().position(|item| item.kind == "owner" && item.id == queue_id);
    let theirs = snapshot.items.iter().position(|item| item.kind == "owner" && item.id == head.id);
    if ours.is_some_and(|ours| theirs.is_some_and(|theirs| ours < theirs)) {
        return;
    }
    let before = cutex::management::v2::admission::AdmissionRef { kind: "owner".into(), id: head.id.clone() };
    if let Err(error) = repository.move_admission_item(session, queue_id, snapshot.order_revision, Some(&before), false) {
        eprintln!("cutex/input/interruptSend: {session}: could not move {queue_id} first: {error:#}");
    }
}

// ---------------------------------------------------------------------------
// Live

struct Live {
    repository: &'static UserInputRepository,
}

impl Ops for Live {
    fn ledger(&self) -> anyhow::Result<&InputLedger> {
        il::default_ledger()
    }

    fn repository(&self) -> &UserInputRepository {
        self.repository
    }

    fn delivery(&self, session: &str) -> Option<Value> {
        cutex::management::v2::input_ledger_settle::current_delivery(session)
    }

    fn live(&self, session: &str) -> Option<LiveTurn> {
        let status = session_manager(session).status(session).ok().flatten()?;
        status.connected.then(|| LiveTurn {
            thread_id: status.thread_id.clone(),
            generation: status.runtime_generation,
            active_turn: status.active_turn_id.clone(),
        })
    }

    fn claude(&self, session: &str) -> bool {
        is_claude_session(session)
    }

    fn turn_has(&self, session: &str, thread: &str, turn: &str, client_id: &str) -> TurnCheck {
        let Ok(handle) = session_manager(session).handle(session) else { return TurnCheck::Unavailable };
        let request = json!({
            "id": format!("cutex-interrupt-send-{}", Uuid::new_v4()),
            "method": "thread/items/list",
            "params": {"threadId": thread, "turnId": turn, "limit": 200, "sortDirection": "asc"},
        });
        let Ok(response) = handle.request_raw_message(request) else { return TurnCheck::Unavailable };
        let Some(result) = response.get("result") else { return TurnCheck::Unavailable };
        match cutex::management::v2::input_ledger_settle::parse_turn_items(result, client_id) {
            cutex::management::v2::input_ledger_settle::Check::Persisted(_) => TurnCheck::Found,
            cutex::management::v2::input_ledger_settle::Check::Absent => TurnCheck::Absent,
            _ => TurnCheck::Unavailable,
        }
    }

    fn interrupt(&self, session: &str, thread: &str, turn: &str) -> Result<(), InterruptFailure> {
        use cutex::app_server::client::AppServerClientError;
        let commands = session_manager(session).commands(session)
            .map_err(|error| InterruptFailure::Offline(format!("{error:#}")))?;
        let result = timed_input(TimingPath::InputNative, TimingStage::Dispatch, || commands.turn_interrupt(&TurnInterruptParams {
            thread_id: thread.to_string(),
            turn_id: turn.to_string(),
        }));
        match result {
            Ok(_) => Ok(()),
            Err(AppServerClientError::Rpc(error)) => Err(InterruptFailure::Refused(format!("{error:?}"))),
            Err(error @ (AppServerClientError::Connect(_) | AppServerClientError::Disconnected(_) | AppServerClientError::Shutdown)) => {
                Err(InterruptFailure::Offline(error.to_string()))
            }
            Err(error) => Err(InterruptFailure::Timeout(error.to_string())),
        }
    }

    fn wait_stopped(&self, session: &str, turn: &str) -> Result<(), InterruptFailure> {
        timed_input(TimingPath::InputNative, TimingStage::Dispatch, || wait_for_turn_to_stop(session, turn)).map_err(|error| {
            if self.live(session).is_none() {
                InterruptFailure::Offline(format!("{error:#}"))
            } else {
                InterruptFailure::Timeout(format!("{error:#}"))
            }
        })?;
        // A disconnect also clears the active turn: that is not a stop.
        if self.live(session).is_none() {
            return Err(InterruptFailure::Offline("the runtime disconnected".into()));
        }
        Ok(())
    }

    fn reserve(&self, session: &str, id: &str, expected: Option<&InputDelivery>, sending: InputDelivery) -> anyhow::Result<Reservation> {
        let repository = self.repository;
        let admission = cutex::asks::pause_admission::try_with_state(&repository.ask_store(), |state, _| {
            if cutex::asks::agent_pause::is_paused(state, session) {
                None
            } else {
                Some(repository.set_delivery(session, id, expected, Some(sending)))
            }
        })?;
        Ok(match admission {
            cutex::asks::pause_admission::Admission::Deferred => Reservation::Busy,
            cutex::asks::pause_admission::Admission::Ready(None) => Reservation::Busy,
            cutex::asks::pause_admission::Admission::Ready(Some(result)) => {
                if result? { Reservation::Reserved } else { Reservation::Changed }
            }
        })
    }

    fn send_start(&self, command: &UserInputSubmitCommand) -> Result<UserInputSubmitExecution, UserInputExecutionError> {
        // Never native IO under the fence (the reservation only tried it).
        cutex::asks::pause_admission::release_worker_fence();
        let result = send_user_input_native_request(
            command,
            "turn/start",
            json!({
                "threadId": command.thread_id,
                "input": command.params.input,
                "clientUserMessageId": command.params.client_user_message_id,
            }),
            UserInputDisposition::Started,
        );
        if result.is_ok() {
            // Owner input ends the agent's `agent_wait` at once (asks §5.8).
            cutex::asks::waits::cancel_waits_default(&command.cutex_session_id);
        }
        result
    }

    fn held_find(&self, session: &str, id: &str) -> anyhow::Result<Option<HeldOwnerInput>> {
        owner_held::find(session, id)
    }

    fn held_finish(&self, session: &str, id: &str) -> anyhow::Result<()> {
        owner_held::finish(session, id)
    }

    fn held_park(&self, session: &str, thread: &str, request_id: &str, params: Value) -> anyhow::Result<()> {
        let id = params.get("clientUserMessageId").and_then(Value::as_str).unwrap_or_default().to_string();
        if owner_held::find(session, &id)?.is_some() {
            return owner_held::update_params(session, &id, params);
        }
        owner_held::hold(HeldOwnerInput::new(
            session,
            &id,
            request_id,
            thread,
            params,
            is_claude_session(session),
            Utc::now(),
        ))
        .map(|_| ())
    }

    fn publish(&self, transition: &il::Transition) {
        if let Err(error) = il::publish(transition) {
            eprintln!("cutex/input/interruptSend: publish: {error:#}");
        }
    }

    fn publish_delivery(&self, session: &str, change: &il::DeliveryChange) {
        if let Err(error) = il::publish_delivery(session, change) {
            eprintln!("cutex/input/interruptSend: publish delivery {session}: {error:#}");
        }
    }

    fn append_event(&self, session: &str, correlation: EventCorrelation, method: &str, params: Value) -> anyhow::Result<()> {
        append_input_event(session, correlation, method, params)
    }
}

/// The installed handler ([`isend::install`]): under the session input lane.
pub(crate) fn interrupt_send_v2(
    session: &str,
    _thread: &str,
    request: &InterruptSendRequest,
) -> Result<Outcome, UserInputExecutionError> {
    let lane = session_input_lock(session)
        .map_err(|error| execution_error("route", "internal_error", error, true, false))?;
    let _lane = lane.lock().map_err(|_| {
        execution_error_message("route", "internal_error", "app-server session input lock was poisoned", true, false)
    })?;
    super::super::management_archive::api::ensure_input_allowed(session)
        .map_err(|error| execution_error("route", "archive_blocks_input", error, false, false))?;
    let repository = user_input_repository()
        .map_err(|error| execution_error("route", "event_persistence_unavailable", error, true, false))?;
    run(&Live { repository }, session, request)
}

#[cfg(test)]
#[path = "app_server_interrupt_send_tests.rs"]
mod tests;
