//! Input ledger writers for delegations (054 §2.9 rows "委托请求" and
//! "委托结果 / 提问 / 状态", §2.10 for results, §2.12 lease rows, §2.14
//! lease release, owner decisions 3 and 9). Block C4.
//!
//! One hook, in [`super::store::AskStore`]'s write path: after a write
//! commits, the asks and deliveries it changed are mapped here
//! ([`store_jobs`]) to ledger transitions. Facts the store does not record
//! — that the native submit of a results event was really made (invariant
//! 7), that native said `Absent` after it — are written by the scheduler
//! code that knows them ([`results_submitted`], [`results_returned`],
//! [`results_unknown`]).
//!
//! Store writes can run under the host authority fence, where no ledger
//! write (event append = IO) may happen (051, 054 §2.13). The mapping is
//! pure and cheap; the transitions are queued under the AskStore lock (so
//! they keep commit order) and applied by one writer thread
//! (`cutex-ledger-asks`). Every write is behind `management.inputLedger.enabled`:
//! with the switch off nothing is computed or queued.
//!
//! Record ids (invariant 2): a delegation request is its `askId` in the
//! target's ledger; a result / question / status is its `deliveryId` in the
//! recipient's ledger. A new event id (another `aev-…` batch) only changes
//! `batchId`.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use super::model::*;
use super::store::{AskState, AskStore};
use crate::management::v2::input_ledger::{
    self as il, Evidence, Expect, InputIntent, InputKind, InputLedger, InputRecord, InputRemote, InputSender,
    InputState, LedgerError, NewInput, Next, SenderKind, Timing, Transition,
};

// ---------------------------------------------------------------------------
// Jobs: ordered transitions of one record, each step guarded by the state it
// may start from (the mapping never reads the ledger itself).

/// `None` in `from` matches a record that does not exist yet.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// Tried in order; a forbidden edge tries the next one.
    pub alts: Vec<Next>,
    /// Apply only when the record's current state is one of these.
    pub from: Option<Vec<Option<InputState>>>,
    /// And only when the record is carried by this turn.
    pub turn: Option<String>,
    /// The results event (`batchId`) a scheduler fact is about. Such a fact
    /// is not ordered by the store: a record in flight in another batch (a
    /// newer attempt: any state but `waiting` between attempts) is skipped,
    /// so a stale fact never moves it or rebinds its `timelineItemId`.
    pub batch: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    pub session: String,
    pub input_id: String,
    pub steps: Vec<Step>,
    /// The AskStore `revision` of the state this job was derived from
    /// (`None`: a scheduler fact, not a store state). The store is written by
    /// several processes (management, Agent Bus), each with its own writer
    /// thread; `revision` is bumped under the store's cross-process exclusive
    /// lock, so it orders their states. A job older than the one last applied
    /// to the record ([`SOURCE_VERSION`]) is skipped: a record never moves
    /// back to an older store state.
    pub source_version: Option<u64>,
}

/// `details` key of the AskStore revision last applied to a record.
pub const SOURCE_VERSION: &str = "askSourceVersion";

fn applied_version(record: Option<&InputRecord>) -> Option<u64> {
    record.and_then(|record| record.details.get(SOURCE_VERSION)).and_then(Value::as_u64)
}

/// `next` carrying the newest source version (the job's, else the one the
/// record already has: a scheduler fact must not erase it), in the details
/// the transition would leave on the record.
fn with_source_version(next: &Next, current: Option<&InputRecord>, version: Option<u64>) -> Next {
    let mut next = next.clone();
    // Private asks job convention: waiting without a reason is a hold-action
    // metadata update, not a scheduler decision. Resolve it under the same
    // revision CAS so a late store projection preserves `returned` (and any
    // other waiting reason), its since timestamp and unrelated details.
    if next.state == InputState::Waiting && next.reason.is_none() {
        if let Some(record) = current.filter(|record| record.state == InputState::Waiting) {
            next.reason = record.reason.clone();
            let owner_hold = next.details.as_ref().and_then(|details| details.get("ownerHold")).cloned();
            let mut details = record.details.clone();
            if !details.is_object() { details = json!({}); }
            details.as_object_mut().expect("details object").remove("ownerHold");
            if let Some(held) = owner_hold { details["ownerHold"] = held; }
            next.details = Some(details);
        }
    }
    let Some(version) = version.max(applied_version(current)) else {
        return next;
    };
    let mut details = next.details.take().unwrap_or_else(|| match current {
        // Same state and no new details: the ledger keeps the stored ones.
        Some(record) if record.state == next.state => record.details.clone(),
        _ => json!({}),
    });
    if !details.is_object() {
        details = json!({});
    }
    details[SOURCE_VERSION] = json!(version);
    next.details = Some(details);
    next
}

/// Re-reads after a revision conflict (another process wrote the record
/// between the read and the write) before the job gives up.
const CONFLICT_RETRIES: usize = 16;

const ABSENT: Option<InputState> = None;
const WAITING: Option<InputState> = Some(InputState::Waiting);
const SENDING: Option<InputState> = Some(InputState::Sending);
const ACCEPTED: Option<InputState> = Some(InputState::Accepted);
const UNCERTAIN: Option<InputState> = Some(InputState::Uncertain);

fn step(next: Next, from: &[Option<InputState>]) -> Step {
    Step { alts: vec![next], from: Some(from.to_vec()), turn: None, batch: None }
}

/// Update hold actions on an existing waiting record without deciding why
/// it is waiting. `with_source_version` resolves the reason under its CAS.
fn waiting_metadata(mut next: Next) -> Next {
    next.reason = None;
    next
}

/// [`step`] of a scheduler fact about results event `batch` (see
/// [`Step::batch`]).
fn batch_step(next: Next, from: &[Option<InputState>], batch: &str) -> Step {
    Step { batch: Some(batch.to_string()), ..step(next, from) }
}

/// Whether `record` is in flight in another batch than `batch`.
fn other_batch(record: Option<&InputRecord>, batch: Option<&str>) -> bool {
    let Some(batch) = batch else { return false };
    record.is_some_and(|record| {
        record.state != InputState::Waiting && record.batch_id.as_deref().is_some_and(|own| own != batch)
    })
}

/// Apply one job on `ledger`. Steps whose guard does not match, and
/// forbidden edges (a record already further along, a late duplicate), are
/// skipped silently; other ledger errors are logged and end the job. A job
/// derived from an older AskStore state than the record's last one is
/// skipped whole. Every step is a compare-and-set on the `rev` read with
/// its guard: a concurrent write (another process's writer) makes it read
/// again, never wait.
pub fn apply_job(ledger: &InputLedger, job: &Job) -> Vec<Transition> {
    apply_job_with(ledger, job, &mut |_| {})
}

/// [`apply_job`], calling `committed` right after each transition commits
/// (the writers publish there: a revision handed out must reach the stream
/// before this thread does more ledger IO, or a later writer's revision waits
/// behind it in the publish queue).
fn apply_job_with(ledger: &InputLedger, job: &Job, committed: &mut dyn FnMut(&Transition)) -> Vec<Transition> {
    let mut out = Vec::new();
    'steps: for step in &job.steps {
        'read: for _ in 0..=CONFLICT_RETRIES {
            let current = match ledger.get(&job.session, std::slice::from_ref(&job.input_id)) {
                Ok(mut result) => result.records.pop(),
                Err(error) => {
                    eprintln!("cutex input ledger (asks): {}/{}: {error}", job.session, job.input_id);
                    return out;
                }
            };
            let applied = applied_version(current.as_ref());
            if job.source_version.zip(applied).is_some_and(|(version, applied)| version < applied) {
                return out;
            }
            // Fence a newer source snapshot while an earlier request is uncertain,
            // even when the new attempt has no applicable state edge yet.
            // Otherwise a delayed older positive job could still rebind it.
            if let (Some(record), Some(version)) = (&current, job.source_version) {
                if record.kind == InputKind::DelegationRequest && record.state == InputState::Uncertain
                    && applied.is_none_or(|old| old < version) {
                    match ledger.note_ask_source_version(&job.session, &job.input_id, Expect::rev(record.rev), version) {
                        Ok(change) => { committed(&change); out.push(change); continue 'read; }
                        Err(LedgerError::Conflict { .. }) => continue 'read,
                        Err(error) => { eprintln!("cutex asks source fence: {error}"); return out; }
                    }
                }
            }
            if step.from.as_ref().is_some_and(|from| !from.contains(&current.as_ref().map(|record| record.state))) {
                continue 'steps;
            }
            if step.turn.is_some() && current.as_ref().and_then(|record| record.turn_id.as_ref()) != step.turn.as_ref() {
                continue 'steps;
            }
            if other_batch(current.as_ref(), step.batch.as_deref()) {
                continue 'steps;
            }
            let expect = Expect::rev(current.as_ref().map_or(0, |record| record.rev));
            for next in &step.alts {
                let next = with_source_version(next, current.as_ref(), job.source_version);
                match ledger.transition(&job.session, &job.input_id, expect, next) {
                    Ok(transition) => {
                        committed(&transition);
                        out.push(transition);
                        continue 'steps;
                    }
                    Err(LedgerError::Conflict { .. }) => continue 'read,
                    Err(LedgerError::ForbiddenEdge { .. } | LedgerError::SurfaceRegression { .. } | LedgerError::NotFound) => {}
                    Err(error) => {
                        eprintln!("cutex input ledger (asks): {}/{}: {error}", job.session, job.input_id);
                        return out;
                    }
                }
            }
            continue 'steps;
        }
        eprintln!("cutex input ledger (asks): {}/{}: still conflicting after {CONFLICT_RETRIES} re-reads", job.session, job.input_id);
        return out;
    }
    out
}

struct Queue {
    jobs: VecDeque<Job>,
    busy: bool,
    started: bool,
}

static QUEUE: Mutex<Queue> = Mutex::new(Queue { jobs: VecDeque::new(), busy: false, started: false });
static WAKE: Condvar = Condvar::new();
static IDLE: Condvar = Condvar::new();

/// Queue jobs for the writer thread (cheap: never IO, safe under the
/// AskStore lock and the host fence).
pub fn enqueue(jobs: Vec<Job>) {
    if jobs.is_empty() {
        return;
    }
    let mut queue = QUEUE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    queue.jobs.extend(jobs);
    if !queue.started {
        queue.started = true;
        if let Err(error) = std::thread::Builder::new().name("cutex-ledger-asks".into()).spawn(writer) {
            queue.started = false;
            eprintln!("cutex input ledger (asks): writer thread: {error}");
        }
    }
    WAKE.notify_one();
}

fn writer() {
    loop {
        let batch: Vec<Job> = {
            let mut queue = QUEUE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            while queue.jobs.is_empty() {
                queue.busy = false;
                IDLE.notify_all();
                queue = WAKE.wait(queue).unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            queue.busy = true;
            queue.jobs.drain(..).collect()
        };
        let ledger = match il::default_ledger() {
            Ok(ledger) => ledger,
            Err(error) => {
                eprintln!("cutex input ledger (asks): {error:#}");
                continue;
            }
        };
        for job in &batch {
            apply_job_with(ledger, job, &mut |transition| publish_one(job, transition));
        }
    }
}

/// Wait until every queued job is applied (owner routes that answer with
/// the resulting records). `false` on timeout.
pub fn flush(timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    let mut queue = QUEUE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    while !queue.jobs.is_empty() || queue.busy {
        let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) else {
            return false;
        };
        queue = IDLE.wait_timeout(queue, left).unwrap_or_else(std::sync::PoisonError::into_inner).0;
    }
    true
}

/// Apply jobs now on this thread (after [`flush`], so earlier queued jobs of
/// the same records come first) and publish them.
fn apply_now(jobs: &[Job]) -> Result<(), String> {
    let ledger = il::default_ledger().map_err(|error| format!("{error:#}"))?;
    for job in jobs {
        apply_job_with(ledger, job, &mut |transition| publish_one(job, transition));
    }
    Ok(())
}

fn publish_one(job: &Job, transition: &Transition) {
    if let Err(error) = il::publish(transition) {
        eprintln!("cutex input ledger (asks): publish {}/{}: {error:#}", job.session, job.input_id);
    }
}

// ---------------------------------------------------------------------------
// Who / what (NewInput)

fn requester_sender(requester: &Requester) -> (InputSender, Option<InputRemote>) {
    match requester {
        Requester::Owner => (
            InputSender { kind: SenderKind::Owner, id: "owner".into(), name: "owner".into(), host: None, cutex_session_id: None },
            None,
        ),
        Requester::Agent { cutex_session_id, name } => (
            InputSender {
                kind: SenderKind::Agent,
                id: cutex_session_id.clone(),
                name: name.clone().unwrap_or_else(|| cutex_session_id.clone()),
                host: None,
                cutex_session_id: Some(cutex_session_id.clone()),
            },
            None,
        ),
        Requester::Peer { peer_id, cutex_session_id, name, label } => (
            InputSender {
                kind: SenderKind::Peer,
                id: cutex_session_id.clone(),
                name: name.clone().unwrap_or_else(|| cutex_session_id.clone()),
                host: Some(label.clone()),
                cutex_session_id: Some(cutex_session_id.clone()),
            },
            Some(InputRemote { host: label.clone(), peer_id: peer_id.clone(), sender_id: cutex_session_id.clone() }),
        ),
    }
}

/// The target answers (results, questions and status come from it).
fn target_sender(ask: &AskRecord) -> (InputSender, Option<InputRemote>) {
    match &ask.remote {
        Some(remote) => (
            InputSender {
                kind: SenderKind::Peer,
                id: ask.target_cutex_session_id.clone(),
                name: remote.target_name.clone(),
                host: Some(remote.label.clone()),
                cutex_session_id: Some(ask.target_cutex_session_id.clone()),
            },
            Some(InputRemote {
                host: remote.label.clone(),
                peer_id: remote.peer_id.clone(),
                sender_id: ask.target_cutex_session_id.clone(),
            }),
        ),
        None => (
            InputSender {
                kind: SenderKind::Agent,
                id: ask.target_cutex_session_id.clone(),
                name: ask.target_name.clone().unwrap_or_else(|| ask.target_cutex_session_id.clone()),
                host: None,
                cutex_session_id: Some(ask.target_cutex_session_id.clone()),
            },
            None,
        ),
    }
}

fn request_input(ask: &AskRecord) -> NewInput {
    let (sender, remote) = requester_sender(&ask.requester);
    NewInput {
        kind: InputKind::DelegationRequest,
        sender,
        intent: InputIntent { strategy: None, delivery_mode: None, timing: Timing::AfterTurn },
        preview: ask.title.clone(),
        ask_id: Some(ask.ask_id.clone()),
        remote,
    }
}

pub fn delivery_kind(kind: DeliveryKind) -> InputKind {
    match kind {
        DeliveryKind::Answer => InputKind::DelegationResult,
        DeliveryKind::Question => InputKind::DelegationQuestion,
        DeliveryKind::State => InputKind::DelegationStatus,
    }
}

fn delivery_input(state: &AskState, delivery: &DeliveryRecord) -> Option<NewInput> {
    let ask = state.asks.get(&delivery.ask_id)?;
    let (sender, remote) = target_sender(ask);
    let preview = match delivery.kind {
        DeliveryKind::Answer => state.answers.get(&delivery.ref_id).and_then(|answer| answer.preview.clone()),
        DeliveryKind::Question => state
            .questions
            .get(&delivery.ref_id)
            .and_then(|question| question.questions.pointer("/0/question").and_then(Value::as_str).map(str::to_string)),
        DeliveryKind::State => Some(format!("{}: {}", delivery.ref_id, ask.title)),
    }
    .unwrap_or_else(|| ask.title.clone());
    Some(NewInput {
        kind: delivery_kind(delivery.kind),
        sender,
        intent: InputIntent {
            strategy: None,
            delivery_mode: Some(if delivery.passive { il::DeliveryMode::Passive } else { il::DeliveryMode::AfterTurn }),
            timing: if delivery.passive { Timing::Passive } else { Timing::AfterTurn },
        },
        preview,
        ask_id: Some(delivery.ask_id.clone()),
        remote,
    })
}

/// The session whose ledger holds the item: where it went (event, wait),
/// else its requester. Never the handover resolution of a pending item
/// (`delivery::recipient` may read the Director sidecar: IO under the store
/// lock); an item a handover reroutes gets its record in the new
/// controller's ledger when it is sent there.
pub fn delivery_session(state: &AskState, delivery: &DeliveryRecord) -> String {
    if delivery.state == DeliveryState::QueuedAsEvent {
        if let Some(event) = delivery.event_id.as_deref().and_then(|id| state.events.get(id)) {
            return event.cutex_session_id.clone();
        }
    }
    delivery.routed_to.clone().unwrap_or_else(|| delivery.requester_cutex_session_id.clone())
}

// ---------------------------------------------------------------------------
// Delegation request (target side)

/// `waiting` reason (and details) of a queued request: what the scheduler's
/// hold means (§2.4). An admission lease is named by its holder.
pub fn queued_reason(state: &AskState, ask: &AskRecord) -> (&'static str, Option<Value>) {
    let target = ask.target_cutex_session_id.as_str();
    let by_lease = || match state.leases.get(target).map(|lease| &lease.holder) {
        Some(LeaseHolder::Ask { .. }) => "delegationLease",
        Some(LeaseHolder::Event { .. }) => "eventLease",
        Some(LeaseHolder::Owner) => "turnRunning",
        None if state.presence.get(target).is_some_and(|presence| presence.busy) => "turnRunning",
        None => "targetBusy",
    };
    let held = |value: &Hold| Some(json!({"hold": value}));
    match &ask.hold {
        None | Some(Hold::TargetBusy { .. }) => (by_lease(), None),
        Some(Hold::WaitingForApproval) => ("reviewPending", None),
        Some(hold @ Hold::Paused { by: PausedBy::Owner }) => {
            let stop = state.pauses.get(target).is_some_and(|pause| pause.stop_hold);
            (if stop { "stopHold" } else { "paused" }, held(hold))
        }
        Some(hold @ Hold::Paused { by: PausedBy::PlanMode }) => {
            ("nativeHeld", Some(json!({"nativeReason": "PlanMode", "hold": hold})))
        }
        Some(hold @ Hold::HeldByRuntime { native }) if native.as_deref() == Some(HOLD_RECONNECTING) => {
            ("runtimeDisconnected", held(hold))
        }
        Some(hold @ Hold::HeldByRuntime { native }) => {
            ("nativeHeld", Some(json!({"nativeReason": native, "hold": hold})))
        }
        Some(hold @ Hold::RuntimeUnsupported { .. }) => {
            ("nativeHeld", Some(json!({"nativeReason": "runtimeUnsupported", "hold": hold})))
        }
        Some(hold @ Hold::Blocked { by: BlockedReason::RuntimeDown }) => ("agentOffline", held(hold)),
        Some(hold @ Hold::Blocked { .. }) => ("autoRetry", held(hold)),
        Some(
            hold @ (Hold::PeerUnreachable | Hold::PeerIdentityChanged | Hold::PeerVersionMismatch | Hold::PeerUnknown { .. }),
        ) => ("runtimeDisconnected", held(hold)),
        Some(hold @ (Hold::ProfileSwitchPending | Hold::WaitingForInput | Hold::WaitingForEvent { .. })) => {
            ("targetBusy", held(hold))
        }
    }
}

/// Native IO for the current attempt happened: a receipt (or a later
/// state), or a submit whose outcome is unknown (`submit_retry`).
fn submitted(ask: &AskRecord) -> bool {
    ask.current_attempt().is_some_and(|attempt| {
        !matches!(attempt.state, AttemptState::Submitting | AttemptState::Refused)
    }) || ask.submit_retry.is_some()
}

/// The ASK reached a turn (the turn started with it as its input).
fn ran(before: Option<&AskRecord>, ask: &AskRecord) -> bool {
    before.is_some_and(|before| before.execution == ExecutionState::Running
            && before.current_attempt().zip(ask.current_attempt()).is_some_and(|(old, new)|
                old.n == new.n && old.native_message_id == new.native_message_id && old.turn_id == new.turn_id))
        || ask.current_attempt().is_some_and(|attempt| {
            attempt.started_at.is_some() || matches!(attempt.state, AttemptState::Running | AttemptState::Answered | AttemptState::Yielded)
        })
}

type RequestSignature = (ExecutionState, &'static str, bool, bool, Option<InterruptReason>);

fn request_signature(state: &AskState, ask: &AskRecord) -> RequestSignature {
    let reason = if ask.execution == ExecutionState::Queued { queued_reason(state, ask).0 } else { "" };
    let interrupt = ask.current_attempt().and_then(|attempt| attempt.interrupt_reason);
    (ask.execution, reason, submitted(ask), ask.current_attempt().is_some_and(|a| a.started_at.is_some()), interrupt)
}

/// The native timeline item of a request attempt on its target: the
/// attempt's `native_message_id` (`cutex-ask-msg-*`, chosen before the
/// submit). The runtimes project the external input as an item whose id is
/// the submitted message id (Codex receipt `responseItemId == messageId`,
/// the Claude adapter's inbox entry). Never the `askId`.
pub fn attempt_item(attempt: &AttemptRecord) -> Option<String> {
    (!attempt.native_message_id.is_empty()).then(|| attempt.native_message_id.clone())
}

/// §2.9 delegation request → the target's record (`askId`). `None`: nothing
/// a client sees changed.
pub fn request_job(before_state: &AskState, after_state: &AskState, before: Option<&AskRecord>, ask: &AskRecord) -> Option<Job> {
    // A request to another host is recorded there.
    if ask.remote.is_some() {
        return None;
    }
    if before.is_some_and(|before| request_signature(before_state, before) == request_signature(after_state, ask)
        && before.current_attempt().map(|a| (a.n, &a.native_message_id, &a.turn_id, a.runtime_generation))
            == ask.current_attempt().map(|a| (a.n, &a.native_message_id, &a.turn_id, a.runtime_generation))) {
        return None;
    }
    let input = request_input(ask);
    let n = |next: Next| next.create(input.clone());
    let turn = ask.current_attempt().and_then(|attempt| attempt.turn_id.clone());
    let started = |next: Next| match &turn {
        Some(turn) => n(next).turn(turn.clone()),
        None => n(next),
    };
    // The native item the request is in: its attempt's message (see
    // [`attempt_item`]). Bound together with that attempt's turn, from the
    // same store state as the rest of the job (ordered by `source_version`,
    // and `inContext` is entered from `sending` only): a stale attempt never
    // rebinds a newer one.
    let in_context = || {
        let next = started(Next::in_context(Evidence::TurnStartInput));
        match ask.current_attempt().and_then(attempt_item) {
            Some(item) if turn.is_some() => next.timeline_item(item),
            _ => next,
        }
    };
    let was_terminal = before.is_some_and(|before| before.execution.is_terminal());
    let mut steps = Vec::new();
    match ask.execution {
        ExecutionState::PendingReview => steps.push(step(n(Next::waiting("reviewPending")), &[ABSENT, WAITING])),
        ExecutionState::Queued => {
            let (reason, details) = queued_reason(after_state, ask);
            let mut next = n(Next::waiting(reason));
            if let Some(details) = details {
                next = next.details(details);
            }
            // A native refusal (nothing entered) puts a sent request back.
            steps.push(step(next, &[ABSENT, WAITING, SENDING]));
        }
        ExecutionState::Admitted if submitted(ask) => steps.push(step(started(Next::sending()), &[ABSENT, WAITING, SENDING])),
        // Lease and attempt are written before the native submit: no IO yet,
        // nothing changes (invariant 7).
        ExecutionState::Admitted => steps.push(step(n(Next::waiting("targetBusy")), &[ABSENT])),
        ExecutionState::Running if before.is_none_or(|before| before.execution != ExecutionState::Running) => {
            steps.push(step(started(Next::sending()), &[ABSENT, WAITING]));
            steps.push(step(in_context(), &[SENDING]));
        }
        ExecutionState::Running => {}
        ExecutionState::Answered | ExecutionState::Failed | ExecutionState::Interrupted if !was_terminal => {
            let interrupt = ask.current_attempt().and_then(|attempt| attempt.interrupt_reason);
            if ran(before, ask) {
                steps.push(step(started(Next::sending()), &[ABSENT, WAITING]));
                steps.push(step(in_context(), &[SENDING]));
            } else if interrupt == Some(InterruptReason::OutcomeUnknown) {
                // Decision 3 / 9: unprovable, kept until the owner resends or
                // dismisses (de84ad).
                steps.push(step(started(Next::sending()), &[ABSENT, WAITING]));
                steps.push(step(n(Next::uncertain("outcomeUnknown")), &[SENDING, ACCEPTED]));
            } else if ask.execution == ExecutionState::Failed {
                steps.push(step(n(Next::rejected("invalid")).details(json!({"execution": "failed"})), &[ABSENT, WAITING]));
                steps.push(step(n(Next::uncertain("turnFailed")), &[SENDING, ACCEPTED]));
            } else if ask.execution == ExecutionState::Interrupted {
                steps.push(step(n(Next::discarded("askCancelled")), &[WAITING]));
                steps.push(step(n(Next::uncertain("turnInterrupted")), &[SENDING, ACCEPTED]));
            } else {
                steps.push(step(started(Next::sending()), &[ABSENT, WAITING]));
                steps.push(step(in_context(), &[SENDING]));
            }
        }
        ExecutionState::Answered | ExecutionState::Failed | ExecutionState::Interrupted => {}
        ExecutionState::Rejected => steps.push(step(n(Next::rejected("askRejected")), &[ABSENT, WAITING])),
        ExecutionState::Cancelled => steps.push(step(n(Next::discarded("askCancelled")), &[WAITING])),
    }
    // Positive facts of the current attempt also heal a timed-out send. An
    // old row/attempt is never proof for the current one; the job carries the
    // same store revision as this binding and apply_job rejects older jobs.
    if matches!(ask.execution, ExecutionState::Running | ExecutionState::Answered | ExecutionState::Failed | ExecutionState::Interrupted)
        && ran(before, ask)
        && ask.current_attempt().is_some_and(|attempt| attempt.turn_id.as_ref().is_some_and(|turn| !turn.is_empty()) && !attempt.native_message_id.is_empty()) {
        steps.push(step(in_context(), &[UNCERTAIN]));
    }
    (!steps.is_empty()).then(|| Job {
        session: ask.target_cutex_session_id.clone(),
        input_id: ask.ask_id.clone(),
        steps,
        source_version: Some(after_state.revision),
    })
}

// ---------------------------------------------------------------------------
// Delegation result / question / status (initiator side)

/// `discarded` reason of a result / question / status record that is not
/// certain to reach the agent's context (r274.2, owner 2026-11-03): its item
/// went passive (read only by `delegation_wait` / `delegation_read`), or a
/// passive status notice left from before r274.2. The asks item itself is
/// kept (a later tool read still returns it); only the panel row goes.
pub const PASSIVE_DISCARDED: &str = "superseded";

/// A record of the passive kind r274.2 no longer creates: a delegation
/// result / question / status `waiting{passive}` (dismiss only, never
/// delivered by cutex itself).
pub fn passive_record(record: &InputRecord) -> bool {
    matches!(record.kind, InputKind::DelegationResult | InputKind::DelegationQuestion | InputKind::DelegationStatus)
        && record.state == InputState::Waiting
        && record.reason.as_deref() == Some("passive")
}

/// `waiting` reason of a new item: passive notes wait for a read; an idle
/// requester's results go through the coalescing window; a busy one waits
/// for its turn to end.
pub fn pending_reason(state: &AskState, delivery: &DeliveryRecord, session: &str) -> &'static str {
    if delivery.owner_hold {
        return "ownerPause";
    }
    if delivery.passive {
        return "passive";
    }
    match state.presence.get(session) {
        Some(presence) if !presence.online => "agentOffline",
        Some(presence) if presence.busy => "afterTurn",
        _ if delivery.kind == DeliveryKind::Answer => "resultCoalescing",
        _ => "afterTurn",
    }
}

fn event_evidence(state: &AskState, event_id: Option<&str>) -> (Evidence, Option<String>) {
    match event_id.and_then(|id| state.events.get(id)) {
        Some(event) if super::returns::is_next_step_event(&event.native_message_id) || super::returns::is_owner_note(&event.native_message_id) => {
            (Evidence::ExternalInputPersisted, event.turn_id.clone())
        }
        Some(event) => (Evidence::TurnStartInput, event.turn_id.clone()),
        None => (Evidence::HistoryRead, None),
    }
}

/// The native timeline item of delivery `delivery_id` in `session`: the
/// results event that carried it is the native input item, its id the
/// event's `native_message_id` (the runtimes project an external input as a
/// `functionCallOutput` whose item id is the submitted message id: Codex
/// `Envelope::response_item`, the Claude adapter's `external_event`). Only
/// an event of that session that names the delivery; `None` otherwise.
pub fn event_item(state: &AskState, event_id: Option<&str>, delivery_id: &str, session: &str) -> Option<String> {
    let event = state.events.get(event_id?)?;
    (event.cutex_session_id == session && event.delivery_ids.iter().any(|id| id == delivery_id) && !event.native_message_id.is_empty())
        .then(|| event.native_message_id.clone())
}

/// §2.9 result / question / status → the recipient's record (`deliveryId`).
pub fn delivery_job(
    before_state: &AskState,
    after_state: &AskState,
    before: Option<&DeliveryRecord>,
    delivery: &DeliveryRecord,
) -> Option<Job> {
    if before.is_some_and(|before| before.state == delivery.state && before.passive == delivery.passive && before.owner_hold == delivery.owner_hold && before.owner_input_id == delivery.owner_input_id) {
        return None;
    }
    let input = delivery_input(after_state, delivery).or_else(|| delivery_input(before_state, delivery))?;
    let session = delivery_session(after_state, delivery);
    let n = |next: Next| next.create(input.clone());
    let created = n(Next::waiting(pending_reason(after_state, delivery, &session)))
        .details(if delivery.state == DeliveryState::Pending {
            json!({"ownerHold": delivery.owner_hold})
        } else { json!({}) });
    // r274.2 (owner 2026-11-03): only an input certain to reach the agent's
    // context enters nextUp / pendingNotes. A passive item reaches it only
    // if the agent calls `delegation_wait` / `delegation_read` (the
    // delegation panel shows the delegation's state instead): no record
    // while it is pending, and the record a tool read creates is off the
    // panels from its first write.
    let read_by_tool = if delivery.passive { created.clone().surface(il::Surface::None) } else { created.clone() };
    let mut steps = Vec::new();
    match delivery.state {
        // An abandoned owner reservation: unreconcilable native outcome or
        // explicit lease release. Normal carries remain QueuedAsEvent; a
        // proven-unsent release/retry clears owner_input_id. This source fact
        // survives a crash before the ledger writer runs. It says unknown,
        // never that a runtime was lost or that the content was read.
        DeliveryState::Pending if delivery.passive && delivery.owner_input_id.is_some()
            && delivery.event_id.is_none() => {
            steps.push(step(n(Next::uncertain("outcomeUnknown")), &[SENDING, ACCEPTED]));
        }
        DeliveryState::Pending if before.is_none() && delivery.passive => {}
        DeliveryState::Pending if before.is_none() => steps.push(step(created, &[ABSENT])),
        // Over budget / unfit / unreconcilable: read with delegation_wait or
        // agent_list only (an unreconcilable send is written explicitly).
        // No longer certain to reach the agent: a `waiting` record leaves
        // the panel (r274.2), nothing in flight is touched.
        DeliveryState::Pending if delivery.passive && before.is_some_and(|before| !before.passive) => {
            steps.push(step(n(Next::discarded(PASSIVE_DISCARDED)), &[WAITING]));
        }
        // Released (refused, absent, unknown): the scheduler that knows
        // whether native IO happened writes it (invariant 7).
        DeliveryState::Pending if before.is_some_and(|before| before.owner_hold != delivery.owner_hold) => {
            steps.push(step(created, &[WAITING]));
        }
        DeliveryState::Pending if before.is_some_and(|before| before.state == DeliveryState::QueuedAsEvent
            || (before.owner_input_id.is_some() && delivery.owner_input_id.is_none())) => {
            steps.push(step(waiting_metadata(created), &[WAITING]));
        }
        DeliveryState::Pending => {}
        // Frozen into an event: `sending` only once the native submit is made.
        DeliveryState::QueuedAsEvent => {
            steps.push(step(created.clone(), &[ABSENT]));
            steps.push(step(waiting_metadata(created), &[WAITING]));
        }
        DeliveryState::ClaimedByWait => {
            steps.push(step(read_by_tool, &[ABSENT]));
            steps.push(step(n(Next::in_context(Evidence::ClaimedByWait)), &[WAITING, UNCERTAIN, SENDING, ACCEPTED]));
        }
        DeliveryState::Acknowledged => match delivery.settled_by.as_deref() {
            None => {
                let event = delivery.event_id.as_deref().or(before.and_then(|before| before.event_id.as_deref()));
                let (evidence, turn) = event_evidence(after_state, event);
                let with_turn = |next: Next| match &turn {
                    Some(turn) => n(next).turn(turn.clone()),
                    None => n(next),
                };
                // The acknowledged event is the native item it is in (every
                // delivery coalesced into it binds the same item). A store
                // state, ordered by its revision: never stale.
                let mut in_context = match (event, event_item(after_state, event, &delivery.delivery_id, &session)) {
                    (Some(event), Some(item)) => {
                        let mut next = with_turn(Next::in_context(evidence)).timeline_item(item);
                        next.batch_id = Some(event.to_string());
                        next
                    }
                    _ => with_turn(Next::in_context(evidence)),
                };
                if delivery.owner_input_id.is_some() {
                    in_context = in_context.reason("carriedByOwner");
                }
                steps.push(step(created, &[ABSENT]));
                steps.push(step(with_turn(Next::sending()), &[WAITING]));
                steps.push(step(in_context, &[SENDING, ACCEPTED, UNCERTAIN]));
            }
            Some("read") => {
                steps.push(step(read_by_tool, &[ABSENT]));
                steps.push(step(n(Next::in_context(Evidence::ReadByTool)), &[WAITING, UNCERTAIN, SENDING, ACCEPTED]));
            }
            Some("closed") => steps.push(step(n(Next::discarded("settlementClosed")), &[WAITING, UNCERTAIN])),
            Some("notices_read") => {
                // A pending source can outpace a prior native projection in
                // another process. Retire it without claiming model context.
                steps.push(step(n(Next::uncertain("outcomeUnknown")), &[SENDING, ACCEPTED]));
                steps.push(step(n(Next::discarded("settlementClosed")), &[WAITING, UNCERTAIN]));
            }
            Some(super::delivery::SUPERSEDED) => {
                // Another process may project this settlement before the
                // creation writer. Establish its newer source version off
                // all surfaces so a late waiting job cannot resurrect it.
                steps.push(step(created.surface(il::Surface::None), &[ABSENT]));
                steps.push(step(n(Next::discarded("superseded")), &[WAITING]));
            }
            Some(super::delivery::HANDOVER_UNKNOWN) => {
                steps.push(step(n(Next::uncertain("outcomeUnknown")), &[SENDING, ACCEPTED]));
            }
            // Source settlement is durable before the owner's ledger write.
            // Recover that decision after a competing projection or a crash;
            // the source version also fences older queued retry metadata.
            Some(DISMISSED) => steps.push(step(n(Next::discarded("ownerDiscarded")), &[WAITING, UNCERTAIN])),
            Some(_) => {}
        },
    }
    (!steps.is_empty()).then(|| Job { session, input_id: delivery.delivery_id.clone(), steps, source_version: Some(after_state.revision) })
}

// ---------------------------------------------------------------------------
// The store hook

/// Jobs for one committed AskStore write: only the asks and deliveries in
/// its row changes are looked at (no scan, no history read).
pub fn store_jobs(before: &AskState, after: &AskState, changes: &[crate::management::v2::hot_store::RowChange]) -> Vec<Job> {
    use crate::management::v2::hot_store::RowChange;
    // 054 §2.6 (C5): a lease taken, moved or released changes the session's
    // SessionDelivery. Only marks (no IO; safe under this lock and the fence).
    if before.leases != after.leases {
        for session in before.leases.keys().chain(after.leases.keys()) {
            if before.leases.get(session) != after.leases.get(session) {
                crate::management::v2::input_ledger_settle::mark_dirty(session);
            }
        }
    }
    let mut jobs = Vec::new();
    for change in changes {
        let RowChange::Put { collection, key, .. } = change else { continue };
        match collection.as_str() {
            "asks" => {
                if let Some(ask) = after.asks.get(key) {
                    jobs.extend(request_job(before, after, before.asks.get(key).map(|ask| &**ask), ask));
                }
            }
            "deliveries" => {
                if let Some(delivery) = after.deliveries.get(key) {
                    jobs.extend(delivery_job(before, after, before.deliveries.get(key), delivery));
                }
            }
            _ => {}
        }
    }
    jobs
}

// ---------------------------------------------------------------------------
// Scheduler facts (returns.rs)

fn results_jobs(state: &AskState, session: &str, ids: &[String], steps: impl Fn(&NewInput) -> Vec<Step>) -> Vec<Job> {
    ids.iter()
        .filter_map(|id| {
            let delivery = state.deliveries.get(id)?;
            let input = delivery_input(state, delivery)?;
            Some(Job { session: session.to_string(), input_id: id.clone(), steps: steps(&input), source_version: None })
        })
        .collect()
}

/// The native submit of results event `event_id` was made: `sending`, and
/// `accepted{turnId}` when the runtime took it into a turn. Not called for
/// a refusal before native IO (authority busy, paused): nothing changes then
/// (invariant 7, RC5). `native_message_id` is the event's native item id
/// (`timelineItemId` of an accepted record). Owner rule 2026-11-03 (r274.2
/// §9.4): `starts_turn` (an exclusive submit: the event is the new turn's
/// first input) puts the records on the timeline with that turn; a
/// next-step event steered into a running turn (`starts_turn` false) is
/// only queued there: it stays in nextUp / pendingNotes until its read
/// evidence (the native item, or the receipt).
pub fn results_submitted(state: &AskState, session: &str, ids: &[String], event_id: &str, native_message_id: &str, turn_id: Option<&str>, starts_turn: bool) {
    if il::enabled() {
        enqueue(submitted_jobs_in(state, session, ids, event_id, Some(native_message_id), turn_id, starts_turn));
    }
}

/// [`submitted_jobs_with`] for an event already in `state`.
pub fn submitted_jobs(state: &AskState, session: &str, ids: &[String], event_id: &str, turn_id: Option<&str>) -> Vec<Job> {
    let item = state.events.get(event_id).map(|event| event.native_message_id.clone());
    submitted_jobs_with(state, session, ids, event_id, item.as_deref(), turn_id)
}

/// [`submitted_jobs_in`] of an event that started turn `turn_id`.
pub fn submitted_jobs_with(
    state: &AskState,
    session: &str,
    ids: &[String],
    event_id: &str,
    item: Option<&str>,
    turn_id: Option<&str>,
) -> Vec<Job> {
    submitted_jobs_in(state, session, ids, event_id, item, turn_id, true)
}

pub fn submitted_jobs_in(
    state: &AskState,
    session: &str,
    ids: &[String],
    event_id: &str,
    item: Option<&str>,
    turn_id: Option<&str>,
    starts_turn: bool,
) -> Vec<Job> {
    let carried = ids.iter().any(|id| state.deliveries.get(id).is_some_and(|d| d.owner_input_id.is_some()));
    results_jobs(state, session, ids, |input| {
        let mut sending = Next::sending().create(input.clone());
        sending.batch_id = Some(event_id.to_string());
        let mut steps = vec![batch_step(sending, &[ABSENT, WAITING, SENDING], event_id)];
        if let Some(turn) = turn_id {
            let mut accepted = Next::accepted().turn(turn).create(input.clone());
            if carried { accepted = accepted.reason("carriedByOwner"); }
            // The turn-start input is read evidence (r274.2 §9.4); a queued
            // steer is not: it moves at its native item or receipt.
            let accepted = if starts_turn { accepted.details(json!({il::READ_SIGNAL: "turnStart"})) } else { accepted };
            let accepted = match item.filter(|item| !item.is_empty()) {
                Some(item) => accepted.timeline_item(item),
                None => accepted,
            };
            steps.push(batch_step(accepted, &[SENDING], event_id));
        }
        steps
    })
}

/// After native IO the runtime said it does not have the event (`Absent`):
/// back to `waiting{returned}`, resent automatically (decision 3). An
/// accepted item cannot go back (invariant 8): `uncertain{runtimeLost}`
/// until a later receipt proves it.
pub fn results_returned(state: &AskState, session: &str, ids: &[String]) {
    if il::enabled() {
        enqueue(returned_jobs(state, session, ids));
    }
}

pub fn returned_jobs(state: &AskState, session: &str, ids: &[String]) -> Vec<Job> {
    results_jobs(state, session, ids, |input| {
        vec![
            step(Next::waiting("returned").create(input.clone()), &[SENDING]),
            step(Next::uncertain("runtimeLost").create(input.clone()), &[ACCEPTED]),
        ]
    })
}

/// 057 §6: the runtime proved the steer that carried these never read and
/// gave them back: `waiting{returned}`, also from `accepted` / `uncertain`
/// (invariant 8's one exception), and sent again as ordinary results.
pub fn results_returned_unread(state: &AskState, session: &str, ids: &[String]) {
    if il::enabled() {
        enqueue(results_jobs(state, session, ids, |input| {
            let back = Next::waiting("returned").details(json!({il::CARRIER_UNREAD: true})).create(input.clone());
            vec![step(back, &[SENDING, ACCEPTED, UNCERTAIN])]
        }));
    }
}

/// The event's outcome cannot be reconciled (decision 3): `uncertain`.
pub fn results_unknown(state: &AskState, session: &str, ids: &[String]) {
    if il::enabled() {
        enqueue(unknown_jobs(state, session, ids));
    }
}

pub fn unknown_jobs(state: &AskState, session: &str, ids: &[String]) -> Vec<Job> {
    results_jobs(state, session, ids, |input| {
        vec![step(Next::uncertain("outcomeUnknown").create(input.clone()), &[SENDING, ACCEPTED])]
    })
}

/// §2.10 for results: turn `turn_id` of `session` ended `interrupted` or
/// `failed`. Items it accepted (a next-step event steered into it) that no
/// receipt proved read become `uncertain{turnInterrupted|turnFailed}`; the
/// scheduler's one status check per event still upgrades them to
/// `inContext` when native shows them persisted (the only way back).
pub fn results_turn_ended(store: &AskStore, session: &str, turn_id: &str, status: &str) {
    let reason = match status {
        "interrupted" => "turnInterrupted",
        "failed" => "turnFailed",
        _ => return,
    };
    if !il::enabled() {
        return;
    }
    let jobs = store.project_snapshot(|state| turn_ended_jobs(state, session, turn_id, reason));
    match jobs {
        Ok(jobs) => enqueue(jobs),
        Err(error) => eprintln!("cutex input ledger (asks): {session}: {error:#}"),
    }
}

/// r274.2 §9.1 for results: the Claude adapter showed results event item
/// `native_message_id` in `turn_id` of `session` (claude-r35 emits a busy
/// `soon` result's `external_event` item when its hook claims it). Its
/// deliveries that were sent but not yet taken into a turn become
/// `accepted{turnId, timelineItemId}` (moved to the timeline,
/// `details.readSignal: "nativeItem"`); `inContext` still waits for the
/// receipt (the scheduler's status check), and an interrupted turn makes
/// them `uncertain` ([`results_turn_ended`]).
pub fn results_item_shown(session: &str, native_message_id: &str, turn_id: &str) {
    if !il::enabled() || native_message_id.is_empty() || turn_id.is_empty() {
        return;
    }
    let jobs = AskStore::open_default().and_then(|store| store.project_snapshot(|state| shown_jobs(state, session, native_message_id, turn_id)));
    match jobs {
        Ok(jobs) => enqueue(jobs),
        Err(error) => eprintln!("cutex input ledger (asks): {session}: {error:#}"),
    }
}

pub fn shown_jobs(state: &AskState, session: &str, native_message_id: &str, turn_id: &str) -> Vec<Job> {
    let events: Vec<&EventOutboxRecord> = state
        .events
        .values()
        .filter(|event| event.cutex_session_id == session && event.native_message_id == native_message_id && !event.acknowledged)
        .collect();
    let mut jobs = Vec::new();
    for event in events {
        jobs.extend(results_jobs(state, session, &event.delivery_ids, |input| {
            let mut shown = Next::accepted().turn(turn_id).timeline_item(native_message_id)
                .details(json!({il::READ_SIGNAL: "nativeItem"}))
                .create(input.clone());
            if event.delivery_ids.iter().any(|id| state.deliveries.get(id).is_some_and(|d| d.owner_input_id.is_some())) {
                shown = shown.reason("carriedByOwner");
            }
            vec![batch_step(shown, &[SENDING], &event.event_id)]
        }));
    }
    jobs
}

pub fn turn_ended_jobs(state: &AskState, session: &str, turn_id: &str, reason: &str) -> Vec<Job> {
    let ids: Vec<String> = state
        .events
        .values()
        .filter(|event| event.cutex_session_id == session && !event.acknowledged)
        .flat_map(|event| event.delivery_ids.iter().cloned())
        .collect();
    let mut jobs = results_jobs(state, session, &ids, |input| {
        vec![Step {
            alts: vec![Next::uncertain(reason).create(input.clone())],
            from: Some(vec![ACCEPTED]),
            turn: Some(turn_id.to_string()),
            batch: None,
        }]
    });
    jobs.retain(|job| !job.steps.is_empty());
    jobs
}

// ---------------------------------------------------------------------------
// Lease view (SessionDelivery.lease, §2.6) and release (§2.14)

fn holder_name(holder: &LeaseHolder) -> &'static str {
    match holder {
        LeaseHolder::Ask { .. } => "ask",
        LeaseHolder::Event { .. } => "event",
        LeaseHolder::Owner => "owner",
    }
}

fn lease_input_ids(state: &AskState, lease: &AdmissionLease) -> Vec<String> {
    match &lease.holder {
        LeaseHolder::Ask { ask_id, .. } => vec![ask_id.clone()],
        LeaseHolder::Event { event_id } => state.events.get(event_id).map(|event| event.delivery_ids.clone()).unwrap_or_default(),
        // Owner inputs are block C2's records (clientUserMessageId).
        LeaseHolder::Owner => Vec::new(),
    }
}

fn time(at: Option<DateTime<Utc>>) -> Value {
    at.map_or(Value::Null, |at| json!(super::now_rfc3339(at)))
}

/// The session's admission lease as 054 §2.6 `lease`, or `None`.
pub fn lease_view_in(store: &AskStore, state: &AskState, session: &str) -> Option<Value> {
    let lease = state.leases.get(session)?;
    let reconcile = super::lease_reconcile::view(store.dir(), lease);
    let reconcile = match reconcile {
        Some(entry) => json!({
            "state": entry.state.as_str(),
            "lastCheckedAt": time(entry.last_checked_at),
            "nextCheckAt": time(entry.next_check_at),
            "lastResult": entry.last_result,
        }),
        None => json!({"state": "waitingTurn", "lastCheckedAt": null, "nextCheckAt": null, "lastResult": null}),
    };
    Some(json!({
        "holder": holder_name(&lease.holder),
        "turnId": lease.turn_id,
        "since": lease.since,
        "runtimeGeneration": lease.runtime_generation,
        "inputIds": lease_input_ids(state, lease),
        "reconcile": reconcile,
    }))
}

pub fn lease_view(store: &AskStore, session: &str) -> anyhow::Result<Option<Value>> {
    store.project_snapshot(|state| lease_view_in(store, state, session))
}

/// A refused release: `code` is `lease_not_stale` or `lease_changed`.
#[derive(Debug, Clone, PartialEq)]
pub struct ReleaseError {
    pub code: &'static str,
    pub message: String,
    pub details: Value,
}

/// §2.14 records of a released event lease: acknowledged (read in its
/// turn) → `inContext`, bound to the event's native item (`native`:
/// `(event id, native message id)`); the rest → `uncertain{leaseReleased}`.
pub fn release_jobs(
    session: &str,
    items: Vec<(String, bool, Option<NewInput>)>,
    evidence: Evidence,
    turn: Option<String>,
    native: Option<(&str, &str)>,
) -> Vec<Job> {
    items
        .into_iter()
        .filter_map(|(id, acknowledged, input)| {
            let input = input?;
            let steps = if acknowledged {
                let next = Next::in_context(evidence).create(input);
                let next = match &turn {
                    Some(turn) => next.turn(turn.clone()),
                    None => next,
                };
                match native {
                    Some((event, item)) => vec![batch_step(next.timeline_item(item), &[SENDING, ACCEPTED], event)],
                    None => vec![step(next, &[SENDING, ACCEPTED])],
                }
            } else {
                vec![step(Next::uncertain("leaseReleased").create(input), &[SENDING, ACCEPTED])]
            };
            Some(Job { session: session.to_string(), input_id: id, steps, source_version: None })
        })
        .collect()
}

fn same_lease(view: &Value, expected: &Value) -> bool {
    ["holder", "turnId", "since", "runtimeGeneration"].iter().all(|key| view.get(*key) == expected.get(*key))
}

/// `cutex/input/lease/release` (§2.14): only a `stale` lease, only the one
/// the owner saw (compare-and-delete on holder, turn, since and generation,
/// like `model_overload_retry`'s release). Acknowledged items → `inContext`,
/// the others → `uncertain{leaseReleased}` (never resent).
pub fn lease_release(store: &AskStore, session: &str, expected: &Value, now: DateTime<Utc>) -> Result<Value, ReleaseError> {
    let unavailable = |error: anyhow::Error| ReleaseError { code: "asks_unavailable", message: format!("{error:#}"), details: json!({}) };
    // Only what the release needs; never an owned copy of the whole state.
    struct Item {
        id: String,
        acknowledged: bool,
        input: Option<NewInput>,
    }
    let snapshot = store
        .project_snapshot(|state| {
            let lease = state.leases.get(session)?.clone();
            let view = lease_view_in(store, state, session)?;
            let ids = lease_input_ids(state, &lease);
            let request = match &lease.holder {
                LeaseHolder::Ask { ask_id, .. } => state.asks.get(ask_id).map(|ask| request_input(ask)),
                _ => None,
            };
            let event = match &lease.holder {
                LeaseHolder::Event { event_id } => Some(event_evidence(state, Some(event_id))),
                _ => None,
            };
            // The event is the native item its acknowledged items are in.
            let native = match &lease.holder {
                LeaseHolder::Event { event_id } => state.events.get(event_id)
                    .filter(|event| event.cutex_session_id == session && !event.native_message_id.is_empty())
                    .map(|event| (event_id.clone(), event.native_message_id.clone())),
                _ => None,
            };
            let items: Vec<Item> = match &lease.holder {
                LeaseHolder::Event { .. } => ids
                    .iter()
                    .filter_map(|id| state.deliveries.get(id))
                    .map(|delivery| Item {
                        id: delivery.delivery_id.clone(),
                        acknowledged: delivery.state == DeliveryState::Acknowledged,
                        input: delivery_input(state, delivery),
                    })
                    .collect(),
                _ => Vec::new(),
            };
            Some((lease, view, ids, request, event, native, items))
        })
        .map_err(unavailable)?;
    let Some((lease, view, ids, request, event, native, items)) = snapshot else {
        return Err(ReleaseError {
            code: "lease_changed",
            message: format!("{session} holds no admission lease now"),
            details: json!({"lease": null}),
        });
    };
    if !same_lease(&view, expected) {
        return Err(ReleaseError { code: "lease_changed", message: "the lease changed since it was read".into(), details: json!({"lease": view}) });
    }
    if !super::lease_reconcile::is_stale(store.dir(), &lease) {
        return Err(ReleaseError {
            code: "lease_not_stale",
            message: "only a stale lease (reconcile.state = stale) can be released".into(),
            details: json!({"lease": view}),
        });
    }
    let persistence = |message: String| ReleaseError { code: "event_persistence_unavailable", message, details: json!({}) };
    let ledger_on = il::enabled();
    match &lease.holder {
        LeaseHolder::Ask { ask_id, attempt } => {
            if let (true, Some(input)) = (ledger_on, request) {
                // Before the store write, so the request ends
                // `uncertain{leaseReleased}`, not the generic outcome.
                flush(Duration::from_secs(5));
                apply_now(&[Job {
                    session: session.to_string(),
                    input_id: ask_id.clone(),
                    steps: vec![step(Next::uncertain("leaseReleased").create(input), &[SENDING, ACCEPTED])],
                    source_version: None,
                }])
                .map_err(persistence)?;
            }
            // The same compare as reconciliation: the attempt ends
            // interrupted (outcome unknown, never re-run), the lease is freed.
            super::observe::finalize_fenced(
                store,
                session,
                ask_id,
                *attempt,
                &json!({"id": lease.turn_id, "status": "interrupted"}),
                Some(InterruptReason::OutcomeUnknown),
                now,
                &lease,
            )
            .map_err(unavailable)?;
            // An attempt that had already ended leaves the lease: remove
            // exactly that lease (compare-and-delete).
            store
                .with_state(|state| {
                    if state.leases.get(session) == Some(&lease) {
                        state.leases.remove(session);
                    }
                })
                .map_err(unavailable)?;
        }
        holder => {
            let event_id = match holder {
                LeaseHolder::Event { event_id } => Some(event_id.clone()),
                _ => None,
            };
            let released = store
                .with_state(|state| {
                    if state.leases.get(session) != Some(&lease) {
                        return false;
                    }
                    state.leases.remove(session);
                    if let Some(event_id) = &event_id {
                        let acknowledged = state.events.get(event_id).is_some_and(|event| event.acknowledged);
                        for id in &ids {
                            if let Some(delivery) = state.deliveries.get_mut(id) {
                                // Not proven read: never sent again (decision 3);
                                // still readable with delegation_wait / agent_list.
                                if delivery.state == DeliveryState::QueuedAsEvent {
                                    delivery.state = DeliveryState::Pending;
                                    delivery.event_id = None;
                                    delivery.passive = true;
                                    delivery.updated_at = super::now_rfc3339(now);
                                }
                            }
                        }
                        if !acknowledged {
                            state.events.remove(event_id);
                        }
                    }
                    true
                })
                .map_err(unavailable)?;
            if !released {
                return Err(ReleaseError { code: "lease_changed", message: "the lease changed while it was released".into(), details: json!({}) });
            }
            if ledger_on {
                let (evidence, turn) = event.unwrap_or((Evidence::HistoryRead, None));
                let items = items.into_iter().map(|item| (item.id, item.acknowledged, item.input)).collect();
                let jobs = release_jobs(session, items, evidence, turn.or(lease.turn_id.clone()),
                    native.as_ref().map(|(event, item)| (event.as_str(), item.as_str())));
                flush(Duration::from_secs(5));
                apply_now(&jobs).map_err(persistence)?;
            }
        }
    }
    super::lease_reconcile::forget(store.dir(), session);
    let records = if ledger_on && !ids.is_empty() {
        flush(Duration::from_secs(5));
        il::default_ledger()
            .map_err(|error| error.to_string())
            .and_then(|ledger| ledger.get(session, &ids).map_err(|error| error.to_string()))
            .map(|result| serde_json::to_value(result.records).unwrap_or(json!([])))
            .unwrap_or(json!([]))
    } else {
        json!([])
    };
    Ok(json!({"released": true, "lease": view, "records": records}))
}

// ---------------------------------------------------------------------------
// Dismiss (owner decision 9)

/// The owner dismisses an `uncertain` (or waiting) delegation record: settle
/// the store side WITHOUT a retry and without native IO, before the ledger
/// marks it `discarded{ownerDiscarded}`.
/// - request (`askId`): an interrupted / failed delegation still open
///   (e.g. `interrupted{outcomeUnknown}`, delegation-de84ad) is closed
///   `stopped`, exactly as `cutex/ask/stop` closes a finished one; a
///   running one is left alone (stop it first);
/// - result / question / status (`deliveryId`): a pending item is settled
///   (`acknowledged`, `settledBy: dismissed`, never sent); its delegation
///   is closed like above when it ended `outcomeUnknown`.
pub fn dismiss(store: &AskStore, record: &InputRecord, now: DateTime<Utc>) -> Result<(), String> {
    let ask_id = match record.kind {
        InputKind::DelegationRequest => Some(record.ask_id.clone().unwrap_or_else(|| record.input_id.clone())),
        _ => record.ask_id.clone(),
    };
    let delivery_id = (record.kind != InputKind::DelegationRequest).then(|| record.input_id.clone());
    let ids: Vec<&str> = ask_id.iter().map(String::as_str).collect();
    store
        .with_records(&ids, |state| {
            if let Some(id) = &delivery_id {
                if let Some(delivery) = state.deliveries.get_mut(id) {
                    if delivery.state == DeliveryState::Pending {
                        delivery.state = DeliveryState::Acknowledged;
                        delivery.settled_by = Some(DISMISSED.to_string());
                        delivery.updated_at = super::now_rfc3339(now);
                    }
                }
            }
            let Some(ask_id) = &ask_id else { return };
            let Some(ask) = state.asks.get(ask_id) else { return };
            let outcome_unknown = ask.current_attempt().and_then(|attempt| attempt.interrupt_reason) == Some(InterruptReason::OutcomeUnknown);
            let close = ask.is_open()
                && ask.execution.is_terminal()
                && ask.remote.is_none()
                && (record.kind == InputKind::DelegationRequest || outcome_unknown);
            if close {
                state.update_ask(ask_id, now, |ask| {
                    ask.closed = Some(CloseOutcome::Stopped);
                    ask.closed_at = Some(super::now_rfc3339(now));
                });
            }
        })
        .map_err(|error| format!("{error:#}"))
}

/// `settledBy` of a delegation item the owner dismissed.
pub const DISMISSED: &str = "dismissed";

// ---------------------------------------------------------------------------
// Retry (054 §2.10 / §2.14, block C5)

/// Whether `cutex/input/retry` can resend result / question / status
/// `delivery_id` of `session`: it was not read (no wait claimed it, no
/// event was acknowledged), so it is either still queued in an event whose
/// outcome is unknown or back to pending. `Err((code, message))` says why not.
pub fn result_redeliverable(state: &AskState, session: &str, delivery_id: &str) -> Result<(), (&'static str, String)> {
    let Some(delivery) = state.deliveries.get(delivery_id) else {
        return Err(("delivery_not_found", format!("no delegation delivery {delivery_id}")));
    };
    if delivery_session(state, delivery) != session {
        return Err(("delivery_not_found", format!("delegation delivery {delivery_id} is not for {session}")));
    }
    match delivery.state {
        DeliveryState::Pending | DeliveryState::QueuedAsEvent => Ok(()),
        DeliveryState::ClaimedByWait | DeliveryState::Acknowledged => Err((
            "delivery_not_redeliverable",
            format!("delegation delivery {delivery_id} was already read or settled"),
        )),
    }
}

/// The store half of a result retry: the item leaves the event that
/// carried it (whose outcome stays unknown; acknowledging that event later
/// still proves it read) and is pending again, not passive, so the
/// scheduler sends it in a new event under the same `deliveryId`. The retry
/// already wrote `waiting{ownerRetry}`; the store projection may restore hold
/// actions without replacing that delivery state or its reason.
pub fn retry_result(state: &mut AskState, session: &str, delivery_id: &str, now: DateTime<Utc>) -> Result<(), (&'static str, String)> {
    result_redeliverable(state, session, delivery_id)?;
    let event_id = state.deliveries.get(delivery_id).and_then(|delivery| delivery.event_id.clone());
    if let Some(event) = event_id.as_deref().and_then(|event_id| state.events.get_mut(event_id)) {
        event.delivery_ids.retain(|id| id != delivery_id);
    }
    // Explicit retry authorizes a fresh native attempt despite this event's
    // unknown outcome. Its lease must not block that attempt, even when other
    // unknown deliveries remain. They stay queued in the old event for late
    // receipts; retrying one item never resends its siblings.
    if let Some(event_id) = event_id.as_deref() {
        super::returns::remove_event_lease(state, session, event_id);
        if state.events.get(event_id).is_some_and(|event| event.delivery_ids.is_empty()) {
            state.events.remove(event_id);
        }
    }
    let delivery = state.deliveries.get_mut(delivery_id).expect("checked above");
    delivery.state = DeliveryState::Pending;
    delivery.event_id = None;
    delivery.owner_input_id = None;
    delivery.routed_to = None;
    delivery.passive = false;
    delivery.updated_at = super::now_rfc3339(now);
    Ok(())
}

// ---------------------------------------------------------------------------
// Drift and rebuild (054 §2.12, block C5): the store is the source; the
// mapping above is replayed from "no record" with every step guarded by the
// record's current state, so an up-to-date record is left alone.

/// The job that brings `record` (a delegation request / result / question /
/// status) in line with the store, or `None` (unknown here, remote, or
/// nothing to do).
pub fn drift_job(state: &AskState, record: &InputRecord) -> Option<Job> {
    match record.kind {
        InputKind::DelegationRequest => {
            let ask = state.asks.get(&record.input_id)?;
            (ask.target_cutex_session_id == record.cutex_session_id).then(|| request_job(state, state, None, ask)).flatten()
        }
        InputKind::DelegationResult | InputKind::DelegationQuestion | InputKind::DelegationStatus => {
            let delivery = state.deliveries.get(&record.input_id)?;
            (delivery_session(state, delivery) == record.cutex_session_id).then(|| delivery_job(state, state, None, delivery)).flatten()
        }
        _ => None,
    }
}

/// The native timeline item of an `inContext` result / question / status
/// record that has none (r274.1 backfill of records written before the
/// binding): `(item, event id)` from the store only. The delivery must be
/// settled by its event (`acknowledged`, not read by a tool, dismissed or
/// closed), for the record's session, its event acknowledged and naming it,
/// and the record's `batchId` (when set) that very event: another batch is
/// another attempt and is never bound. `None`: not derivable.
pub fn backfill_item(state: &AskState, record: &InputRecord) -> Option<(String, String)> {
    if !matches!(record.kind, InputKind::DelegationResult | InputKind::DelegationQuestion | InputKind::DelegationStatus)
        || record.state != InputState::InContext
        || matches!(record.evidence, Some(Evidence::ClaimedByWait | Evidence::ReadByTool))
    {
        return None;
    }
    let delivery = state.deliveries.get(&record.input_id)?;
    if delivery.state != DeliveryState::Acknowledged || delivery.settled_by.is_some() {
        return None;
    }
    if delivery_session(state, delivery) != record.cutex_session_id {
        return None;
    }
    let event_id = delivery.event_id.as_deref()?;
    if !state.events.get(event_id).is_some_and(|event| event.acknowledged) {
        return None;
    }
    if record.batch_id.as_deref().is_some_and(|batch| batch != event_id) {
        return None;
    }
    let item = event_item(state, Some(event_id), &delivery.delivery_id, &record.cutex_session_id)?;
    Some((item, event_id.to_string()))
}

/// The native timeline item of an `inContext` delegation request record on
/// its target that has none (r274.1 backfill): the message of the attempt
/// that ran in the record's turn, from the store only. The ask must be
/// local, target the record's session, and exactly one of its attempts must
/// have started in that very turn (a record without a turn, or a turn no
/// attempt or several attempts name, is not derivable). `None`: not derivable.
pub fn backfill_request_item(state: &AskState, record: &InputRecord) -> Option<String> {
    if record.kind != InputKind::DelegationRequest || record.state != InputState::InContext {
        return None;
    }
    let turn = record.turn_id.as_deref()?;
    let ask = state.asks.get(record.ask_id.as_deref().unwrap_or(&record.input_id))?;
    if ask.ask_id != record.input_id || ask.remote.is_some() || ask.target_cutex_session_id != record.cutex_session_id {
        return None;
    }
    let mut ran = ask.attempts.iter().filter(|attempt| {
        attempt.turn_id.as_deref() == Some(turn)
            && (attempt.started_at.is_some()
                || matches!(attempt.state, AttemptState::Running | AttemptState::Answered | AttemptState::Yielded))
    });
    let attempt = ran.next()?;
    if ran.next().is_some() {
        return None;
    }
    attempt_item(attempt)
}

/// A lost ledger (§2.12): records for the open delegations of this host
/// (requests not yet ended, results / questions / status not yet read).
/// Queued for the writer; returns how many jobs. One pass over the hot asks,
/// only after a loss.
pub fn rebuild_open(store: &AskStore) -> anyhow::Result<usize> {
    if !il::enabled() {
        return Ok(0);
    }
    let jobs = store.project_snapshot(|state| {
        let mut jobs: Vec<Job> = state
            .asks
            .values()
            .filter(|ask| ask.is_open() && ask.remote.is_none() && !ask.execution.is_terminal())
            .filter_map(|ask| request_job(state, state, None, ask))
            .collect();
        jobs.extend(
            state
                .deliveries
                .values()
                .filter(|delivery| matches!(delivery.state, DeliveryState::Pending | DeliveryState::QueuedAsEvent))
                .filter_map(|delivery| delivery_job(state, state, None, delivery)),
        );
        jobs
    })?;
    let count = jobs.len();
    enqueue(jobs);
    Ok(count)
}

// ---------------------------------------------------------------------------
// Backfill (decision 9 for delegations that ended before the ledger existed)

/// Open delegations that ended `interrupted{outcomeUnknown}` before the
/// switch was on have no record, so the owner could not dismiss them
/// (delegation-de84ad): give each one `uncertain{outcomeUnknown}` once. One
/// pass over the hot asks at scheduler start; existing records are kept.
pub fn backfill_outcome_unknown(store: &AskStore) -> anyhow::Result<usize> {
    if !il::enabled() {
        return Ok(0);
    }
    let jobs = store.project_snapshot(|state| {
        state
            .asks
            .values()
            .filter(|ask| ask.is_open() && ask.remote.is_none() && ask.execution == ExecutionState::Interrupted)
            .filter(|ask| ask.current_attempt().and_then(|attempt| attempt.interrupt_reason) == Some(InterruptReason::OutcomeUnknown))
            .filter(|ask| !ran(None, ask))
            .map(|ask| {
                let input = request_input(ask);
                Job {
                    session: ask.target_cutex_session_id.clone(),
                    input_id: ask.ask_id.clone(),
                    steps: vec![
                        step(Next::sending().create(input.clone()), &[ABSENT]),
                        step(Next::uncertain("outcomeUnknown").create(input), &[SENDING]),
                    ],
                    source_version: None,
                }
            })
            .collect::<Vec<_>>()
    })?;
    let count = jobs.len();
    enqueue(jobs);
    Ok(count)
}

#[cfg(test)]
#[path = "ledger_tests.rs"]
mod tests;
