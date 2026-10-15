//! Returning results to agent requesters (asks design §5.7): wait results,
//! and the one coalesced ANSWER/QUESTION event admitted through the
//! requester's own scheduler (same lease, same exclusive admission) when it
//! is idle and not inside an `delegation_wait` on those asks.
//!
//! r217h2: a busy requester still waits for its turn to end; once its oldest
//! result has been pending `agent_bus.wake_busy_wait_secs` (default 120 s,
//! the wake setting), a Codex runtime that takes native `soon` reads the same
//! event at its next step instead (no turn of its own). The event is
//! acknowledged once the native receipt shows it read, exactly like an
//! exclusive one. Claude adapters advertising durable next-step results use
//! authenticated PostToolUse context; older runtimes keep waiting.
//!
//! The same holds while the requester's running turn holds its admission
//! lease (a turn a results event or a delegation it handles started): the
//! next-step event does not need that slot, so it is tracked beside it in
//! `next_step_leases` (same record shape) and takes the slot once the
//! running turn's lease is released, if it has not been read by then.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use super::answer;
use super::model::*;
use super::render::{self, ReturnItem};
use super::scheduler::{AskRuntime, NativeMessageStatus, SchedulerWorld, Submission, SubmitOutcome, TickReport};
use super::store::{AskState, AskStore};
use super::{new_id, now_rfc3339};

fn answer_text(store: &AskStore, answer: &AnswerRecord) -> Option<String> {
    let artifact = answer.artifact_id.as_deref()?;
    store
        .read_artifact(artifact, None)
        .ok()
        .flatten()
        .map(|(_, bytes)| String::from_utf8_lossy(&bytes).into_owned())
}

fn flags(ask: &AskRecord, answer: &AnswerRecord) -> Vec<&'static str> {
    let mut flags = Vec::new();
    if answer.metadata.shared_turn {
        flags.push("sharedTurn");
    }
    if answer.metadata.owner_steered {
        flags.push("ownerSteered");
    }
    if answer.metadata.compacted {
        flags.push("compacted");
    }
    if ask.target_reverted {
        flags.push("targetReverted");
    }
    flags
}

fn target_label(ask: &AskRecord) -> String {
    ask.target_name.clone().unwrap_or_else(|| ask.target_cutex_session_id.clone())
}

/// One delivery as a model-facing return item.
pub fn return_item(store: &AskStore, state: &AskState, delivery: &DeliveryRecord) -> Option<ReturnItem> {
    let ask = state.asks.get(&delivery.ask_id)?;
    Some(match delivery.kind {
        DeliveryKind::Answer => {
            let answer = state.answers.get(&delivery.ref_id)?;
            let full = answer_text(store, answer);
            let answer_bytes = full.as_ref().map_or(0, String::len);
            let inline = full.map(|text| answer::inline_default(&text, &ask.ask_id, &answer.answer_id, answer.revision).0);
            ReturnItem::Answer {
                ask_id: ask.ask_id.clone(),
                title: ask.title.clone(),
                target: target_label(ask),
                status: answer.status,
                answer_id: answer.answer_id.clone(),
                revision: answer.revision,
                inline,
                answer_bytes,
                review: ask.review == Some(ReviewState::AwaitingAcceptance),
                flags: flags(ask, answer),
            }
        }
        DeliveryKind::Question => {
            let question = state.questions.get(&delivery.ref_id)?;
            ReturnItem::Question {
                ask_id: ask.ask_id.clone(),
                title: ask.title.clone(),
                target: target_label(ask),
                question_id: question.question_id.clone(),
                questions: question.questions.clone(),
            }
        }
        DeliveryKind::State => ReturnItem::State {
            ask_id: ask.ask_id.clone(),
            title: ask.title.clone(),
            target: target_label(ask),
            state: delivery.ref_id.split('@').next().unwrap_or("changed").to_string(),
        },
    })
}

/// JSON of a return item for tool results.
pub fn item_json(item: &ReturnItem) -> Value {
    match item {
        ReturnItem::Answer { ask_id, title, target, status, answer_id, revision, inline, review, flags, .. } => json!({
            "kind": "answer",
            "ask_id": ask_id,
            "title": title,
            "from": target,
            "status": status,
            "answer_id": answer_id,
            "revision": revision,
            "answer": inline,
            "awaiting_acceptance": review,
            "flags": flags,
        }),
        ReturnItem::Question { ask_id, title, target, question_id, questions } => json!({
            "kind": "question",
            "ask_id": ask_id,
            "title": title,
            "from": target,
            "question_id": question_id,
            "questions": questions,
        }),
        ReturnItem::State { ask_id, title, target, state } => json!({
            "kind": "state",
            "ask_id": ask_id,
            "title": title,
            "target": target,
            "state": state,
        }),
    }
}

/// r217h2: native message id prefix of a results event read at a busy
/// requester's next step (native `soon`). The runtime builds the `soon`
/// envelope from it, so submit and status rebuild the same key.
pub const NEXT_STEP_EVENT_PREFIX: &str = "cutex-ask-soon";

pub fn is_next_step_event(native_message_id: &str) -> bool {
    native_message_id
        .strip_prefix(NEXT_STEP_EVENT_PREFIX)
        .is_some_and(|rest| rest.starts_with('-'))
}

/// When the requester's oldest actionable result became pending (the busy
/// wait counts from it; the admission snapshot shows the same deadline).
pub(crate) fn oldest_pending(state: &AskState, requester: &str) -> Option<DateTime<Utc>> {
    state
        .deliveries
        .values()
        .filter(|delivery| {
            delivery.requester_cutex_session_id == requester && !delivery.passive && super::delivery::actionable(state, delivery)
        })
        .filter_map(|delivery| super::parse_time(&delivery.created_at))
        .min()
}

/// In-memory facts for the result read log only (never for decisions).
#[derive(Default)]
struct ReadLog {
    /// Requesters whose pending results waited for a busy turn.
    waited_busy: HashSet<String>,
    /// Next-step event id → the busy turn it was sent into.
    sent_into: HashMap<String, String>,
}

fn read_log() -> &'static Mutex<ReadLog> {
    static LOG: OnceLock<Mutex<ReadLog>> = OnceLock::new();
    LOG.get_or_init(Default::default)
}

fn note_waited_busy(requester: &str) {
    if let Ok(mut log) = read_log().lock() {
        if log.waited_busy.len() > 4096 {
            log.waited_busy.clear();
        }
        log.waited_busy.insert(requester.to_string());
    }
}

/// `turn_end` when the requester's results waited for a busy turn.
fn exclusive_path(requester: &str) -> &'static str {
    let waited = read_log().lock().is_ok_and(|mut log| log.waited_busy.remove(requester));
    if waited {
        "turn_end"
    } else {
        "idle_exclusive"
    }
}

fn note_sent_into(event_id: &str, busy_turn: Option<&str>, requester: &str) {
    if let Ok(mut log) = read_log().lock() {
        log.waited_busy.remove(requester);
        if log.sent_into.len() > 4096 {
            log.sent_into.clear();
        }
        if let Some(turn) = busy_turn {
            log.sent_into.insert(event_id.to_string(), turn.to_string());
        }
    }
}

/// `early_next_step` when read in the busy turn it was sent into (or that
/// turn is unknown); `turn_end` when that turn ended without reading it and
/// the runtime read it in a turn of its own afterwards.
fn next_step_path(event_id: &str, turn_id: &str) -> &'static str {
    match read_log().lock().ok().and_then(|mut log| log.sent_into.remove(event_id)) {
        Some(busy) if busy != turn_id => "turn_end",
        _ => "early_next_step",
    }
}

fn forget_next_step(event_id: &str) {
    if let Ok(mut log) = read_log().lock() {
        log.sent_into.remove(event_id);
    }
    forget_unavailable(event_id);
}

type UnavailableLog = std::collections::HashMap<String, DateTime<Utc>>;

/// When each event lease was first seen unreconcilable (process-local: a
/// restart starts the grace over, which only delays the release).
fn unavailable_log() -> &'static std::sync::Mutex<UnavailableLog> {
    static LOG: std::sync::OnceLock<std::sync::Mutex<UnavailableLog>> = std::sync::OnceLock::new();
    LOG.get_or_init(Default::default)
}

/// Whether `event_id` has now been unreconcilable for the whole grace.
fn unavailable_past_grace(event_id: &str, now: DateTime<Utc>) -> bool {
    let Ok(mut log) = unavailable_log().lock() else {
        return false;
    };
    if log.len() > 4096 {
        log.clear();
    }
    let since = *log.entry(event_id.to_string()).or_insert(now);
    now - since >= chrono::Duration::seconds(super::EVENT_UNAVAILABLE_GRACE_SECS)
}

fn forget_unavailable(event_id: &str) {
    if let Ok(mut log) = unavailable_log().lock() {
        log.remove(event_id);
    }
}

/// `(ask id, kind, pending since ms)` of an event's results, for the log.
fn result_rows(state: &AskState, delivery_ids: &[String], now: DateTime<Utc>) -> Vec<(String, &'static str, i64)> {
    delivery_ids
        .iter()
        .filter_map(|id| state.deliveries.get(id))
        .map(|delivery| {
            let kind = match delivery.kind {
                DeliveryKind::Answer => "answer",
                DeliveryKind::Question => "question",
                DeliveryKind::State => "state",
            };
            let since = super::parse_time(&delivery.created_at).unwrap_or(now);
            (delivery.ask_id.clone(), kind, since.timestamp_millis())
        })
        .collect()
}

/// One `cutex-wake-read` line per delegation result that reached its
/// requester (wake log, r217h1).
#[allow(clippy::too_many_arguments)]
fn log_results(
    rows: &[(String, &'static str, i64)],
    message_id: &str,
    requester: &str,
    wait: Option<std::time::Duration>,
    path: &str,
    turn_id: &str,
    now: DateTime<Utc>,
) {
    for (ask_id, kind, since) in rows {
        crate::agent_bus::wake_log::record_result_read(
            ask_id,
            kind,
            message_id,
            requester,
            wait,
            *since,
            now.timestamp_millis(),
            path,
            turn_id,
        );
    }
}

fn held_by_event(lease: &AdmissionLease, event_id: &str) -> bool {
    matches!(&lease.holder, LeaseHolder::Event { event_id: id } if id == event_id)
}

/// Remove what `event_id` holds for `session`: its admission lease, or its
/// next-step record beside the running turn's lease (never that lease).
fn remove_event_lease(state: &mut AskState, session: &str, event_id: &str) {
    if state.leases.get(session).is_some_and(|lease| held_by_event(lease, event_id)) {
        state.leases.remove(session);
    }
    if state.next_step_leases.get(session).is_some_and(|lease| held_by_event(lease, event_id)) {
        state.next_step_leases.remove(session);
    }
}

/// A next-step event in flight beside a running turn's lease takes the
/// requester's admission slot as soon as that lease is gone (turn end,
/// interrupt, yield, any release path), exactly as if it had been sent
/// without one: the slot is then held until the runtime shows it read (or
/// lost). Records of events that no longer wait are dropped. Run on every
/// store write ([`AskStore::with_state`]).
pub fn promote_next_step_leases(state: &mut AskState) {
    if state.next_step_leases.is_empty() {
        return;
    }
    let sessions: Vec<String> = state.next_step_leases.keys().cloned().collect();
    for session in sessions {
        let live = state.next_step_leases.get(&session).is_some_and(|lease| match &lease.holder {
            LeaseHolder::Event { event_id } => state.events.get(event_id).is_some_and(|event| !event.acknowledged),
            _ => false,
        });
        if !live {
            state.next_step_leases.remove(&session);
        } else if !state.leases.contains_key(&session) {
            if let Some(lease) = state.next_step_leases.remove(&session) {
                state.leases.insert(session, lease);
            }
        }
    }
}

/// Whether new results may be sent to `requester` now as far as the store
/// goes, and the running turn whose lease they would go beside: no pause, no
/// delegation_wait, no next-step event in flight, and either no lease or a
/// lease bound to a turn (the running turn: the runtime status must show it
/// active). A lease without a turn (a submit not yet bound, a next-step event
/// not read yet) still waits.
pub(crate) fn results_admissible(state: &AskState, requester: &str) -> Option<Option<String>> {
    if state.pauses.contains_key(requester) || waiting(state, requester) || state.next_step_leases.contains_key(requester) {
        return None;
    }
    match state.leases.get(requester) {
        None => Some(None),
        Some(lease) => lease.turn_id.clone().map(Some),
    }
}

/// Whether `session` is inside an `delegation_wait` (the wait claims results).
pub fn waiting(state: &AskState, session: &str) -> bool {
    state.waits.values().any(|wait| wait.cutex_session_id == session)
}

/// The asks an event carries (in delivery order, once each).
fn event_asks(state: &AskState, event: &EventOutboxRecord) -> Vec<String> {
    let mut asks = Vec::new();
    for delivery in event.delivery_ids.iter().filter_map(|id| state.deliveries.get(id)) {
        if !asks.contains(&delivery.ask_id) {
            asks.push(delivery.ask_id.clone());
        }
    }
    asks
}


/// The ask a return item belongs to.
fn return_ask_id(item: &ReturnItem) -> &str {
    match item {
        ReturnItem::Answer { ask_id, .. } | ReturnItem::Question { ask_id, .. } | ReturnItem::State { ask_id, .. } => ask_id,
    }
}

/// Which return items go into one event (input overflow §A2).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FittedReturns {
    pub event_type: &'static str,
    pub text: String,
    /// Indexes of the items in this event (each at the first level that fit).
    pub included: Vec<usize>,
    /// Fit alone, not next to the included ones: wait for the next event.
    pub deferred: Vec<usize>,
    /// Too large even alone at the deepest level: never sent as an event.
    pub unfit: Vec<usize>,
    /// Sizes of the event text.
    pub item_bytes: usize,
}

/// Fit return items into one event under `budget`: in order, each at the
/// first render level that still fits next to the ones before it.
pub fn fit_returns(
    items: &[ReturnItem],
    budget: &crate::app_server::external_input::size::Budget,
    message_id: &str,
    preview_bytes: usize,
) -> FittedReturns {
    use crate::app_server::external_input::size::canonical_item_bytes;
    let source = super::scheduler::submission_source(None);
    let limit = Some(budget.notice_limit());
    let is_answer = |item: &ReturnItem| matches!(item, ReturnItem::Answer { .. });
    let fits = |has_answer: bool, pieces: &[String], later: usize| {
        let (event_type, text) = render::assemble_return(has_answer, pieces, later);
        let item_bytes = canonical_item_bytes(message_id, &source, event_type, &text);
        (budget.fits(item_bytes, text.len()), event_type, text, item_bytes)
    };
    let mut fitted = FittedReturns::default();
    let mut pieces: Vec<String> = Vec::new();
    let mut has_answer = false;
    for (index, item) in items.iter().enumerate() {
        // Once one item needs the next turn, later results cannot pass it.
        if !fitted.deferred.is_empty() {
            fitted.deferred.push(index);
            continue;
        }
        let mut placed = false;
        for level in 0..render::RETURN_LEVELS {
            let mut trial = pieces.clone();
            trial.push(render::return_piece(item, level, preview_bytes, limit));
            // Conservative: the marker for results that may still follow.
            if fits(has_answer || is_answer(item), &trial, items.len()).0 {
                pieces = trial;
                has_answer |= is_answer(item);
                fitted.included.push(index);
                placed = true;
                break;
            }
        }
        if !placed {
            let alone = [render::return_piece(item, render::RETURN_LEVELS - 1, preview_bytes, limit)];
            if fits(is_answer(item), &alone, items.len()).0 {
                fitted.deferred.push(index);
            } else {
                fitted.unfit.push(index);
            }
        }
    }
    let (_, event_type, text, item_bytes) = fits(has_answer, &pieces, fitted.deferred.len());
    fitted.event_type = event_type;
    fitted.text = text;
    fitted.item_bytes = item_bytes;
    fitted
}

/// An event lease whose outcome cannot be reconciled: drop the event and
/// the lease, make its results passive (never re-sent) and tell the owner.
fn settle_unreconcilable_event(
    store: &AskStore,
    session: &str,
    event_id: &str,
    delivery_ids: &[String],
    asks: Vec<String>,
    error: &str,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    let detail = format!(
        "{}: the delivery of these results to {session} could not be confirmed ({error}); they are not re-sent and stay readable with delegation_wait / agent_list",
        super::RESULTS_OUTCOME_UNKNOWN
    );
    store.with_state(|state| {
        if state.events.remove(event_id).is_none() {
            return;
        }
        remove_event_lease(state, session, event_id);
        make_passive_with(state, session, delivery_ids, asks, super::RESULTS_OUTCOME_UNKNOWN, detail, now);
    })?;
    Ok(())
}

/// Results that can never be sent as an event: passive (delegation_wait
/// and agent_list still return them), and the owner is told once.
fn make_passive(
    state: &mut AskState,
    requester: &str,
    delivery_ids: &[String],
    asks: Vec<String>,
    detail: String,
    now: DateTime<Utc>,
) {
    make_passive_with(state, requester, delivery_ids, asks, super::EVENT_TOO_LARGE, detail, now);
}

#[allow(clippy::too_many_arguments)]
fn make_passive_with(
    state: &mut AskState,
    requester: &str,
    delivery_ids: &[String],
    asks: Vec<String>,
    code: &str,
    detail: String,
    now: DateTime<Utc>,
) {
    for id in delivery_ids {
        if let Some(delivery) = state.deliveries.get_mut(id) {
            if matches!(delivery.state, DeliveryState::Pending | DeliveryState::QueuedAsEvent) {
                delivery.state = DeliveryState::Pending;
                delivery.event_id = None;
                delivery.passive = true;
                delivery.updated_at = now_rfc3339(now);
            }
        }
    }
    state.notice(
        super::scheduler::DELIVERY_FAILED_EVENT,
        vec![crate::management::v2::services::HOST_EVENT_SESSION_ID.to_string()],
        json!({
            "code": code,
            "kind": "results",
            "askIds": asks,
            "target": requester,
            "targetName": Value::Null,
            "requester": requester,
            "detail": detail,
        }),
    );
}

/// Take deliveries out of an event before it is submitted (they stay
/// pending for the next one).
fn detach_from_event(state: &mut AskState, event_id: &str, delivery_ids: &[String], now: DateTime<Utc>) {
    if let Some(event) = state.events.get_mut(event_id) {
        event.delivery_ids.retain(|id| !delivery_ids.contains(id));
    }
    for id in delivery_ids {
        if let Some(delivery) = state.deliveries.get_mut(id) {
            if delivery.state == DeliveryState::QueuedAsEvent {
                delivery.state = DeliveryState::Pending;
                delivery.event_id = None;
                delivery.updated_at = now_rfc3339(now);
            }
        }
    }
}

/// Admit coalesced ANSWER/QUESTION events for idle requesters (§5.7).
pub fn admit_events(
    world: &dyn SchedulerWorld,
    runtime: &dyn AskRuntime,
    store: &AskStore,
    report: &mut TickReport,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    let state = store.snapshot()?;
    if !state.leases.values().any(|lease| matches!(lease.holder, LeaseHolder::Event { .. }))
        && state.next_step_leases.is_empty()
        && super::delivery::requesters_with_pending(&state).is_empty()
    {
        return Ok(());
    }
    let directory = world.directory().ok();
    let limits = world.limits();
    // The busy wait (a config read) only when needed.
    let busy_wait_cell = std::cell::OnceCell::new();
    let busy_wait = || *busy_wait_cell.get_or_init(|| world.results_busy_wait());
    // Reconcile event leases whose submit outcome is unknown, and next-step
    // events not read yet (holding the slot, or beside a running turn's
    // lease).
    let in_flight = state.leases.values().map(|lease| (lease, true)).chain(state.next_step_leases.values().map(|lease| (lease, false)));
    for (lease, in_slot) in in_flight {
        let LeaseHolder::Event { event_id } = &lease.holder else {
            continue;
        };
        if lease.turn_id.is_some() {
            continue;
        }
        let Some(event) = state.events.get(event_id) else {
            continue;
        };
        let next_step = is_next_step_event(&event.native_message_id);
        if !in_slot && !next_step {
            continue; // never written: only next-step events go beside a lease
        }
        let mut reconcile_generation = lease.runtime_generation;
        if next_step {
            // Volatile admission can be released after restart. Durable
            // Claude inboxes reconcile the existing key on the new generation.
            let target = runtime.target_status(&lease.cutex_session_id);
            if target.online && target.runtime_generation != lease.runtime_generation {
                if runtime.durable_next_step(&lease.cutex_session_id) {
                    reconcile_generation = target.runtime_generation;
                } else {
                    let (session, event_id) = (lease.cutex_session_id.clone(), event_id.clone());
                    let freed = store.with_state(|state| {
                        super::delivery::release_event(state, &event_id, now);
                        remove_event_lease(state, &session, &event_id);
                        !state.leases.contains_key(&session)
                    })?;
                    forget_next_step(&event_id);
                    if freed {
                        report.released.push(session);
                    }
                    continue;
                }
            }
        }
        let Some(text) = event
            .event_artifact
            .as_deref()
            .and_then(|artifact| store.read_artifact(artifact, None).ok().flatten())
            .map(|(_, bytes)| String::from_utf8_lossy(&bytes).into_owned())
        else {
            continue;
        };
        let submission = Submission {
            message_id: event.native_message_id.clone(),
            source_agent: None,
            event_type: if text.contains("## Answer") { render::ANSWER_EVENT } else { render::QUESTION_EVENT },
            text,
            asks: event_asks(&state, event),
            attempt: None,
            view: event.view.clone(),
        };
        let session = lease.cutex_session_id.clone();
        let event_id = event_id.clone();
        match runtime.message_status(&session, reconcile_generation, &submission) {
            NativeMessageStatus::Persisted { turn_id } | NativeMessageStatus::Held { turn_id: Some(turn_id), .. } => {
                let rows = result_rows(&state, &event.delivery_ids, now);
                let freed = store.with_state(|state| {
                    super::delivery::acknowledge_event(state, &event_id, &turn_id, now);
                    if next_step {
                        // Read inside a turn the event did not start:
                        // nothing to hold the requester's admission for (a
                        // running turn's own lease beside it stays).
                        remove_event_lease(state, &session, &event_id);
                    } else if let Some(lease) = state.leases.get_mut(&session) {
                        lease.turn_id = Some(turn_id.clone());
                    }
                    !state.leases.contains_key(&session)
                })?;
                let path = if next_step { next_step_path(&event_id, &turn_id) } else { exclusive_path(&session) };
                log_results(&rows, &event.native_message_id, &session, busy_wait(), path, &turn_id, now);
                if next_step && freed {
                    report.released.push(session.clone());
                }
            }
            NativeMessageStatus::Absent => {
                let freed = store.with_state(|state| {
                    super::delivery::release_event(state, &event_id, now);
                    remove_event_lease(state, &session, &event_id);
                    !state.leases.contains_key(&session)
                })?;
                if next_step {
                    forget_next_step(&event_id);
                }
                forget_unavailable(&event_id);
                if freed {
                    report.released.push(session.clone());
                }
            }
            NativeMessageStatus::Pending => forget_unavailable(&event_id),
            // The outcome cannot be reconciled (status query failing, a
            // lost submit the runtime does not know, a receipt binding
            // mismatch, a terminal receipt without a turn). Re-sending could
            // deliver twice, so once this lasted the whole grace the results
            // become passive (delegation_wait / agent_list still return them),
            // the owner is told once, and the requester's admission is freed.
            NativeMessageStatus::Unavailable(error) => {
                if unavailable_past_grace(&event_id, now) {
                    settle_unreconcilable_event(store, &session, &event_id, &event.delivery_ids, submission.asks.clone(), &error, now)?;
                    forget_next_step(&event_id);
                    if in_slot {
                        report.released.push(session.clone());
                    }
                }
            }
            NativeMessageStatus::Held { reason, turn_id: None } => {
                settle_unreconcilable_event(store, &session, &event_id, &event.delivery_ids, submission.asks.clone(), &reason, now)?;
                forget_next_step(&event_id);
                if in_slot {
                    report.released.push(session.clone());
                }
            }
        }
    }
    let state = store.snapshot()?;
    for requester in super::delivery::requesters_with_pending(&state) {
        // `held_turn`: the running turn whose lease the results go beside.
        let Some(held_turn) = results_admissible(&state, &requester) else {
            continue;
        };
        let status = runtime.target_status(&requester);
        if !status.online && status.reconnectable {
            // A live runtime another cutex process started: reconnect (never
            // launch) and deliver on a later tick.
            if let Err(error) = runtime.reconnect(&requester) {
                eprintln!("asks: reconnect of {requester} failed: {error:#}");
            }
            continue;
        }
        if !status.online || status.plan_mode || !status.exclusive_admission {
            continue;
        }
        // A leased requester only beside the very turn its lease holds (a
        // lease of a turn that already ended waits for its release).
        if held_turn.is_some() && status.active_turn_id != held_turn {
            continue;
        }
        // r217h2: a busy requester waits for its turn to end (then its own
        // exclusive turn, as before). Once its oldest result has been
        // pending the busy wait, a runtime that takes `soon` reads them at
        // its next step instead. Owner input queued behind the busy turn does
        // not hold them back (they start no turn of their own).
        let next_step = if status.active_turn_id.is_some() {
            note_waited_busy(&requester);
            let due = busy_wait().is_some_and(|wait| {
                oldest_pending(&state, &requester)
                    .is_some_and(|since| (now - since).to_std().unwrap_or_default() >= wait)
            });
            if !due || !runtime.takes_next_step(&requester, status.runtime_generation) {
                continue;
            }
            true
        } else {
            if crate::management::v2::admission::owner_priority(&requester) {
                let admission = store.queue_repository()?.admission_with_state(&requester, &state)?;
                if admission.items.iter().any(|item| item.kind == "owner" && item.state == "waiting") {
                    continue;
                }
                crate::management::v2::admission::owner_started(&requester);
            }
            false
        };
        let key = directory
            .as_ref()
            .map(|directory| super::directory::domain_label(&directory.domain(&requester)))
            .unwrap_or_else(|| format!("agent:{requester}"));
        if !super::budget::take_auto_turn(store, &key, &requester, limits.auto_turns_per_hour, now)? {
            report.degraded.push(requester.clone());
            continue;
        }
        let message_id = new_id(if next_step { NEXT_STEP_EVENT_PREFIX } else { "cutex-ask-event" });
        let Some(event_id) = store.with_state(|state| -> anyhow::Result<_> {
            if results_admissible(state, &requester).as_ref() != Some(&held_turn) {
                return Ok(None);
            }
            Ok(super::delivery::coalesce_into_event(state, &requester, &message_id, now))
        })??
        else {
            continue;
        };
        let state = store.snapshot()?;
        let mut event = state.events.get(&event_id).cloned().expect("just created");
        let pairs: Vec<(String, ReturnItem)> = event
            .delivery_ids
            .iter()
            .filter_map(|id| state.deliveries.get(id))
            .filter_map(|delivery| return_item(store, &state, delivery).map(|item| (delivery.delivery_id.clone(), item)))
            .collect();
        let all_items: Vec<ReturnItem> = pairs.iter().map(|(_, item)| item.clone()).collect();
        let settings = world.documents();
        let budget = status.receiver_limit.budget(settings.margin_percent);
        let fitted = fit_returns(&all_items, &budget, &message_id, settings.preview_bytes as usize);
        let ids_of = |indexes: &[usize]| -> Vec<String> { indexes.iter().map(|index| pairs[*index].0.clone()).collect() };
        let (deferred, unfit) = (ids_of(&fitted.deferred), ids_of(&fitted.unfit));
        if !deferred.is_empty() || !unfit.is_empty() {
            let unfit_asks: Vec<String> = fitted.unfit.iter().map(|index| return_ask_id(&pairs[*index].1).to_string()).collect();
            let limit = budget.limit.describe();
            store.with_state(|state| {
                detach_from_event(state, &event_id, &deferred, now);
                if !unfit.is_empty() {
                    detach_from_event(state, &event_id, &unfit, now);
                    let detail = format!(
                        "{}: {} delegation result(s) for {requester} are larger than it accepts ({limit}) even as a short preview with a delegation_read pointer; they were not sent as a message and stay readable with delegation_wait / agent_list",
                        super::EVENT_TOO_LARGE,
                        unfit.len()
                    );
                    make_passive(state, &requester, &unfit, unfit_asks.clone(), detail, now);
                }
            })?;
            if !unfit.is_empty() {
                report.degraded.push(requester.clone());
            }
            if fitted.included.is_empty() {
                store.with_state(|state| super::delivery::release_event(state, &event_id, now))?;
                continue;
            }
            event.delivery_ids.retain(|id| !deferred.contains(id) && !unfit.contains(id));
        }
        let items: Vec<ReturnItem> = fitted.included.iter().map(|index| all_items[*index].clone()).collect();
        let (event_type, text) = (fitted.event_type, fitted.text);
        let results_view = super::views::results_view(
            &items,
            &|ask_id| {
                state
                    .asks
                    .get(ask_id)
                    .map(|ask| (ask.target_cutex_session_id.clone(), ask.target_name.clone()))
                    .unwrap_or_else(|| (String::new(), None))
            },
            &|answer_id| state.answers.get(answer_id).and_then(|answer| answer.preview.clone()),
        );
        let artifact = store.put_artifact(text.as_bytes(), "text/plain; charset=utf-8", &[], None, now)?;
        let taken = store.with_state(|state| {
            if state.pauses.contains_key(&requester)
                || state.next_step_leases.contains_key(&requester)
                || state.leases.get(&requester).map(|lease| lease.turn_id.clone()) != held_turn.clone().map(Some)
            {
                super::delivery::release_event(state, &event_id, now);
                return false;
            }
            if let Some(event) = state.events.get_mut(&event_id) {
                event.event_artifact = Some(artifact.clone());
                event.view = Some(results_view.clone());
            }
            let lease = AdmissionLease {
                cutex_session_id: requester.clone(),
                holder: LeaseHolder::Event { event_id: event_id.clone() },
                native_message_id: message_id.clone(),
                runtime_generation: status.runtime_generation,
                turn_id: None,
                since: now_rfc3339(now),
            };
            // Beside the running turn's lease, which stays untouched.
            let slot = if held_turn.is_some() { &mut state.next_step_leases } else { &mut state.leases };
            slot.insert(requester.clone(), lease);
            true
        })?;
        if !taken {
            continue;
        }
        let submission = Submission {
            message_id,
            source_agent: None,
            event_type,
            text,
            asks: event_asks(&state, &event),
            attempt: None,
            view: Some(results_view),
        };
        let prepared_turn = (!next_step).then(|| crate::management::v2::admission::prepare_turn(&requester, "delivery", None));
        let outcome = if next_step {
            runtime.steer_note(&requester, status.runtime_generation, &submission)
        } else {
            runtime.submit_exclusive(&requester, status.runtime_generation, &submission)
        };
        if matches!(outcome, SubmitOutcome::Started { .. } | SubmitOutcome::Uncertain(_)) {
            if let Some(prepared) = prepared_turn { prepared.submitted(); }
        }
        let text_bytes = submission.text.len();
        let rows = result_rows(&state, &event.delivery_ids, now);
        if next_step && matches!(outcome, SubmitOutcome::Started { .. }) {
            note_sent_into(&event_id, status.active_turn_id.as_deref(), &requester);
        }
        let started = store.with_state(|state| match outcome {
            // Accepted, not read yet: acknowledged (and the lease, which has
            // no turn, released) once the receipt shows it read (above).
            SubmitOutcome::Started { .. } if next_step => {
                report.events.push(event_id.clone());
                None
            }
            SubmitOutcome::Started { turn_id } => {
                super::delivery::acknowledge_event(state, &event_id, &turn_id, now);
                if let Some(lease) = state.leases.get_mut(&requester) {
                    lease.turn_id = Some(turn_id.clone());
                }
                report.events.push(event_id.clone());
                Some(turn_id)
            }
            SubmitOutcome::Uncertain(_) => None,
            SubmitOutcome::Busy { reason } => {
                super::delivery::release_event(state, &event_id, now);
                remove_event_lease(state, &requester, &event_id);
                if reason == super::control::NATIVE_GATE_CLOSED {
                    super::control::runtime_gate_pause(state, &requester, status.runtime_generation, now);
                }
                None
            }
            SubmitOutcome::Unsupported(_) | SubmitOutcome::Refused(_) => {
                super::delivery::release_event(state, &event_id, now);
                remove_event_lease(state, &requester, &event_id);
                None
            }
            SubmitOutcome::TooLarge { item_bytes, limit } => {
                // The same results would be refused again: never re-sent as
                // an event. They stay pending as passive items (delegation_wait
                // and agent_list return them), and the owner is told once.
                let delivery_ids = event.delivery_ids.clone();
                super::delivery::release_event(state, &event_id, now);
                remove_event_lease(state, &requester, &event_id);
                let detail = format!(
                    "{}; the results stay readable with delegation_wait / agent_list",
                    super::scheduler::too_large_detail("delegation results", text_bytes, item_bytes, limit, &requester)
                );
                make_passive(state, &requester, &delivery_ids, submission.asks.clone(), detail, now);
                report.degraded.push(requester.clone());
                None
            }
        })?;
        if let Some(turn_id) = started {
            log_results(&rows, &submission.message_id, &requester, busy_wait(), exclusive_path(&requester), &turn_id, now);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::scheduler::tests::{finish_turn, world, FakeRuntime};
    use crate::asks::store::tests::{new_ask, temp_store, REQUESTER, TARGET};

    /// Input overflow §A2: results are fitted to the requester's budget in
    /// order, each at the first level that still fits next to the earlier
    /// ones (preview + delegation_read pointer); the rest waits for the next
    /// event; an item too large even alone is never sent as an event.
    #[test]
    fn results_are_fitted_to_the_receiver_budget() {
        use crate::app_server::external_input::size::{canonical_item_bytes, ReceiverLimit};
        let answer = |n: usize, text: String| ReturnItem::Answer {
            ask_id: format!("ask-{n}"),
            title: "t".into(),
            target: "Kimi".into(),
            status: AnswerStatus::Completed,
            answer_id: format!("ans-{n}"),
            revision: 1,
            answer_bytes: text.len(),
            inline: Some(text),
            review: false,
            flags: vec![],
        };
        let long = || "a \"quoted\" line\n".repeat(400);
        let items: Vec<ReturnItem> = (0..20).map(|n| answer(n, long())).collect();
        let budget = ReceiverLimit::Default.budget(10);
        let fitted = fit_returns(&items, &budget, "cutex-ask-event-1", 800);
        assert!(!fitted.included.is_empty() && !fitted.deferred.is_empty(), "{fitted:?}");
        assert!(fitted.unfit.is_empty());
        assert_eq!(fitted.included.len() + fitted.deferred.len(), 20);
        assert_eq!(fitted.included, (0..fitted.included.len()).collect::<Vec<_>>());
        assert_eq!(fitted.deferred, (fitted.included.len()..20).collect::<Vec<_>>());
        let source = super::super::scheduler::submission_source(None);
        let measured = canonical_item_bytes("cutex-ask-event-1", &source, fitted.event_type, &fitted.text);
        assert_eq!(measured, fitted.item_bytes);
        assert!(measured <= 9_000, "{measured}");
        assert!(fitted.text.contains("delegation_read (cutex server) with delegation=\"delegation-0\""), "{}", fitted.text);
        assert!(fitted.text.contains("they follow in the next one"));
        // One short result fits whole at level 0.
        let short = fit_returns(&[answer(9, "done: 42".into())], &budget, "m", 800);
        assert_eq!(short.included, vec![0]);
        assert!(short.text.contains("done: 42") && !short.text.contains("continues"));
        // A receiver too small for even a pointer: never an event.
        let tiny = ReceiverLimit::Item(400).budget(0);
        let none = fit_returns(&items[..1], &tiny, "m", 800);
        assert_eq!((none.included.len(), none.unfit), (0, vec![0]));
    }

    /// Acceptance §12.3 item 6/7 (part): an answer to an idle requester that
    /// is not waiting becomes one ANSWER event in its own exclusive turn,
    /// acknowledged by the receipt; a waiting requester gets nothing.
    #[test]
    fn answer_event_goes_to_an_idle_requester_once() {
        let (world, dir) = world("returns");
        let (store, store_dir) = temp_store("returns");
        let runtime = FakeRuntime::idle(&[TARGET, REQUESTER]);
        let now = Utc::now();
        let ask = store.with_state(|state| state.create_ask(new_ask("a", "x"), now).unwrap().0.ask_id).unwrap();
        crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
        crate::asks::observe::observe(
            &store,
            TARGET,
            "item/completed",
            &json!({"turnId": "turn-1", "item": {"type": "agentMessage", "text": "done: 42"}}),
            now,
        )
        .unwrap();
        // The requester is waiting: no event.
        store
            .with_state(|state| {
                state.waits.insert(
                    "w".into(),
                    crate::asks::store::WaitRecord {
                        wait_id: "w".into(),
                        cutex_session_id: REQUESTER.into(),
                        action_id: "x".into(),
                        asks: vec![ask.clone()],
                        all: false,
                        started_at: now_rfc3339(now),
                        deadline: now_rfc3339(now + chrono::Duration::seconds(30)),
                        cancel_epoch: 0,
                    },
                )
            })
            .unwrap();
        crate::asks::observe::observe(&store, TARGET, "turn/completed", &json!({"turn": {"id": "turn-1", "status": "completed"}}), now).unwrap();
        finish_turn(&runtime, &store, TARGET);
        let report = crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
        assert!(report.events.is_empty());
        store.with_state(|state| state.waits.clear()).unwrap();
        let report = crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
        assert_eq!(report.events.len(), 1);
        let submitted = runtime.submitted.borrow();
        let (session, event) = submitted.last().unwrap();
        assert_eq!(session, REQUESTER);
        assert_eq!(event.event_type, render::ANSWER_EVENT);
        assert!(event.text.contains("done: 42"));
        let state = store.snapshot().unwrap();
        assert!(state.deliveries.values().all(|d| d.state == DeliveryState::Acknowledged));
        assert!(matches!(state.leases[REQUESTER].holder, LeaseHolder::Event { .. }));
        drop(submitted);
        // The event turn ends: lease released, nothing resent.
        let turn = state.leases[REQUESTER].turn_id.clone().unwrap();
        crate::asks::observe::observe(&store, REQUESTER, "turn/completed", &json!({"turn": {"id": turn, "status": "completed"}}), now).unwrap();
        assert!(!store.snapshot().unwrap().leases.contains_key(REQUESTER));
        runtime.set(REQUESTER, |status| status.active_turn_id = None);
        assert!(crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap().events.is_empty());
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    /// A target answered an ask of REQUESTER at `now` (one pending answer).
    fn answered(world: &crate::asks::scheduler::tests::FakeWorld, runtime: &FakeRuntime, store: &AskStore, now: DateTime<Utc>) -> String {
        let ask = store.with_state(|state| state.create_ask(new_ask("a", "x"), now).unwrap().0.ask_id).unwrap();
        crate::asks::scheduler::tick(world, runtime, store, now).unwrap();
        let turn = store.snapshot().unwrap().leases[TARGET].turn_id.clone().unwrap();
        crate::asks::observe::observe(
            store,
            TARGET,
            "item/completed",
            &json!({"turnId": turn, "item": {"type": "agentMessage", "text": "done: 42"}}),
            now,
        )
        .unwrap();
        crate::asks::observe::observe(store, TARGET, "turn/completed", &json!({"turn": {"id": turn, "status": "completed"}}), now).unwrap();
        finish_turn(runtime, store, TARGET);
        assert_eq!(store.snapshot().unwrap().deliveries.len(), 1);
        ask
    }

    /// r217h2: results for a busy requester wait for its turn to end; once
    /// pending 120 s they are read at its next step (`soon`, no turn of their
    /// own), acknowledged once the receipt shows them read, and never sent
    /// again (not at the turn end either). A runtime without `soon` (Claude,
    /// old natives) keeps waiting for the turn to end.
    #[test]
    fn busy_requester_reads_results_at_its_next_step_after_the_wait() {
        let (world, dir) = world("returns-next-step");
        let (store, store_dir) = temp_store("returns-next-step");
        let runtime = FakeRuntime::idle(&[TARGET, REQUESTER]);
        let now = Utc::now();
        runtime.set(REQUESTER, |status| status.active_turn_id = Some("turn-busy".into()));
        let ask = answered(&world, &runtime, &store, now);
        let at = |secs: i64| now + chrono::Duration::seconds(secs);
        // No `soon`: waits, even long after the busy wait.
        for secs in [1, 130, 3600] {
            assert!(crate::asks::scheduler::tick(&world, &runtime, &store, at(secs)).unwrap().events.is_empty());
        }
        assert!(runtime.steered.borrow().is_empty() && runtime.submitted.borrow().len() == 1, "only the ask itself");
        // Takes `soon`: still waits within the busy wait (a fresh result).
        runtime.next_step.set(true);
        let (store2, store2_dir) = temp_store("returns-next-step-fresh");
        answered(&world, &runtime, &store2, now);
        for secs in [1, 60, 119] {
            assert!(crate::asks::scheduler::tick(&world, &runtime, &store2, at(secs)).unwrap().events.is_empty());
        }
        assert!(runtime.steered.borrow().is_empty());
        let state = store2.snapshot().unwrap();
        assert!(state.deliveries.values().all(|d| d.state == DeliveryState::Pending) && state.leases.is_empty());
        // Pending 120 s: read at the next step, as the same ANSWER event.
        let exclusive_before = runtime.submitted.borrow().len();
        let report = crate::asks::scheduler::tick(&world, &runtime, &store2, at(120)).unwrap();
        assert_eq!(report.events.len(), 1);
        assert_eq!(runtime.submitted.borrow().len(), exclusive_before, "no exclusive turn");
        let (session, event) = runtime.steered.borrow().last().cloned().unwrap();
        assert_eq!(session, REQUESTER);
        assert!(is_next_step_event(&event.message_id), "{}", event.message_id);
        assert_eq!(event.event_type, render::ANSWER_EVENT);
        assert!(event.text.contains("done: 42"));
        let state = store2.snapshot().unwrap();
        assert!(state.leases[REQUESTER].turn_id.is_none());
        assert!(state.deliveries.values().all(|d| d.state == DeliveryState::QueuedAsEvent));
        // Not read yet: nothing resent, a wait cannot claim it, lease kept.
        for secs in [121, 125] {
            crate::asks::scheduler::tick(&world, &runtime, &store2, at(secs)).unwrap();
        }
        assert_eq!(runtime.steered.borrow().len(), 1);
        assert!(store2.with_state(|state| super::super::delivery::claim_for_wait(state, REQUESTER, &[], "w", at(125))).unwrap().is_empty());
        assert!(store2.snapshot().unwrap().leases.contains_key(REQUESTER));
        // Read at the busy turn's next step: acknowledged, lease released.
        runtime
            .native
            .borrow_mut()
            .insert(event.message_id.clone(), NativeMessageStatus::Persisted { turn_id: "turn-busy".into() });
        crate::asks::scheduler::tick(&world, &runtime, &store2, at(126)).unwrap();
        let state = store2.snapshot().unwrap();
        assert!(!state.leases.contains_key(REQUESTER));
        assert!(state.deliveries.values().all(|d| d.state == DeliveryState::Acknowledged));
        assert!(state.events.values().all(|e| e.acknowledged && e.turn_id.as_deref() == Some("turn-busy")));
        // The busy turn ends: no event turn, nothing resent, ever.
        crate::asks::observe::observe(&store2, REQUESTER, "turn/completed", &json!({"turn": {"id": "turn-busy", "status": "completed"}}), at(200)).unwrap();
        runtime.set(REQUESTER, |status| status.active_turn_id = None);
        for secs in [200, 4000] {
            assert!(crate::asks::scheduler::tick(&world, &runtime, &store2, at(secs)).unwrap().events.is_empty());
        }
        assert_eq!(runtime.submitted.borrow().len(), exclusive_before);
        assert_eq!(runtime.steered.borrow().len(), 1);
        // The first store (no `soon` then): its turn ends, one exclusive turn.
        runtime.next_step.set(false);
        let report = crate::asks::scheduler::tick(&world, &runtime, &store, at(4000)).unwrap();
        assert_eq!(report.events.len(), 1);
        let state = store.snapshot().unwrap();
        assert!(state.deliveries.values().filter(|d| d.ask_id == ask).all(|d| d.state == DeliveryState::Acknowledged));
        for dir in [dir, store_dir, store2_dir] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// Another ask of REQUESTER answered at `at` (TARGET runs it at once).
    fn answer_more(world: &crate::asks::scheduler::tests::FakeWorld, runtime: &FakeRuntime, store: &AskStore, action: &str, at: DateTime<Utc>) -> String {
        let ask = store.with_state(|state| state.create_ask(new_ask(action, action), at).unwrap().0.ask_id).unwrap();
        crate::asks::scheduler::tick(world, runtime, store, at).unwrap();
        let turn = store.snapshot().unwrap().leases[TARGET].turn_id.clone().unwrap();
        crate::asks::observe::observe(
            store,
            TARGET,
            "item/completed",
            &json!({"turnId": turn, "item": {"type": "agentMessage", "text": format!("done: {action}")}}),
            at,
        )
        .unwrap();
        crate::asks::observe::observe(store, TARGET, "turn/completed", &json!({"turn": {"id": turn, "status": "completed"}}), at).unwrap();
        finish_turn(runtime, store, TARGET);
        ask
    }

    /// The admission snapshot rows of REQUESTER's results: `(timing, nextStepAt)`.
    fn snapshot_timing(store: &AskStore) -> Vec<(String, Option<DateTime<Utc>>)> {
        let state = store.snapshot().unwrap();
        crate::management::v2::admission::ask_next_step(&state, REQUESTER)
            .into_iter()
            .map(|row| (row.timing, row.next_step_at.as_deref().and_then(super::super::parse_time)))
            .collect()
    }

    /// An owner requester agent (2026-10-12): results that arrive while the requester's
    /// running turn was started by an earlier results event follow the same
    /// busy wait → next step rule as for any busy requester: not before the
    /// wait, read at the next step after it, as a separate next-step event
    /// beside the running turn's lease, which stays exactly as it was and is
    /// still released at that turn's end. The admission snapshot promises the
    /// instant the loop acts.
    #[test]
    fn event_leased_requester_reads_later_results_at_its_next_step() {
        let (world, dir) = world("returns-leased-next-step");
        let (store, store_dir) = temp_store("returns-leased-next-step");
        let runtime = FakeRuntime::idle(&[TARGET, REQUESTER]);
        runtime.next_step.set(true);
        let now = Utc::now();
        let at = |secs: i64| now + chrono::Duration::seconds(secs);
        answered(&world, &runtime, &store, now);
        // Idle: the first result starts the requester's own event turn.
        assert_eq!(crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap().events.len(), 1);
        let lease = store.snapshot().unwrap().leases[REQUESTER].clone();
        let event_turn = lease.turn_id.clone().unwrap();
        assert!(matches!(lease.holder, LeaseHolder::Event { .. }) && !is_next_step_event(&lease.native_message_id));
        assert_eq!(runtime.target_status(REQUESTER).active_turn_id.as_deref(), Some(event_turn.as_str()));
        let exclusive = || runtime.submitted.borrow().iter().filter(|(session, _)| session == REQUESTER).count();
        assert_eq!(exclusive(), 1);
        // Two more results 4 s later, inside that turn.
        answer_more(&world, &runtime, &store, "b", at(4));
        answer_more(&world, &runtime, &store, "c", at(4));
        let wait = world.results_busy_wait().unwrap();
        assert_eq!(crate::agent_bus::delivery::wake_busy_wait(), Some(wait), "snapshot and loop use one wait");
        let created = store.snapshot().unwrap().deliveries.values().filter_map(|d| super::super::parse_time(&d.created_at)).max().unwrap();
        let due = created + chrono::Duration::from_std(wait).unwrap();
        let rows = snapshot_timing(&store);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|(_, next_step_at)| *next_step_at == Some(due)), "{rows:?}");
        // Not before the wait.
        for secs in [5, 60, 123] {
            assert!(crate::asks::scheduler::tick(&world, &runtime, &store, at(secs)).unwrap().events.is_empty());
        }
        assert!(runtime.steered.borrow().is_empty());
        assert_eq!(store.snapshot().unwrap().leases[REQUESTER], lease);
        // A lease whose turn is no longer the running one waits for its
        // release (turn/completed not observed yet), even when due.
        runtime.set(REQUESTER, |status| status.active_turn_id = Some("turn-other".into()));
        assert!(crate::asks::scheduler::tick(&world, &runtime, &store, due).unwrap().events.is_empty());
        runtime.set(REQUESTER, |status| status.active_turn_id = Some(event_turn.clone()));
        // At the promised instant: one next-step event with both results.
        let report = crate::asks::scheduler::tick(&world, &runtime, &store, due).unwrap();
        assert_eq!(report.events.len(), 1);
        assert_eq!(exclusive(), 1, "no exclusive turn");
        let (session, event) = runtime.steered.borrow().last().cloned().unwrap();
        assert_eq!(session, REQUESTER);
        assert!(is_next_step_event(&event.message_id));
        assert!(event.text.contains("done: b") && event.text.contains("done: c"), "{}", event.text);
        let state = store.snapshot().unwrap();
        assert_eq!(state.leases[REQUESTER], lease, "the running turn's lease is untouched");
        let beside = state.next_step_leases[REQUESTER].clone();
        assert_eq!(beside.native_message_id, event.message_id);
        assert!(beside.turn_id.is_none());
        assert!(snapshot_timing(&store).iter().all(|(timing, _)| timing == "nextStep"));
        // Not read yet: nothing resent.
        crate::asks::scheduler::tick(&world, &runtime, &store, at(130)).unwrap();
        assert_eq!(runtime.steered.borrow().len(), 1);
        // Read at the event turn's next step: acknowledged, the record goes,
        // the lease stays (and admission is not reported as released).
        runtime.native.borrow_mut().insert(event.message_id.clone(), NativeMessageStatus::Persisted { turn_id: event_turn.clone() });
        let report = crate::asks::scheduler::tick(&world, &runtime, &store, at(131)).unwrap();
        assert!(report.released.is_empty());
        let state = store.snapshot().unwrap();
        assert!(state.next_step_leases.is_empty());
        assert_eq!(state.leases[REQUESTER], lease);
        assert!(state.deliveries.values().all(|d| d.state == DeliveryState::Acknowledged));
        // The event turn ends: its lease is released as before, nothing resent.
        crate::asks::observe::observe(&store, REQUESTER, "turn/completed", &json!({"turn": {"id": event_turn, "status": "completed"}}), at(200)).unwrap();
        runtime.set(REQUESTER, |status| status.active_turn_id = None);
        assert!(store.snapshot().unwrap().leases.is_empty());
        for secs in [200, 4000] {
            assert!(crate::asks::scheduler::tick(&world, &runtime, &store, at(secs)).unwrap().events.is_empty());
        }
        assert_eq!((runtime.steered.borrow().len(), exclusive()), (1, 1));
        for dir in [dir, store_dir] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// The event turn ends before the next-step results were read: the
    /// in-flight record takes the freed admission slot at once (no owner or
    /// other admission slips in before the runtime reads it), and is
    /// released when the runtime shows it read in a turn of its own.
    #[test]
    fn unread_next_step_beside_a_lease_takes_the_slot_at_turn_end() {
        let (world, dir) = world("returns-leased-promote");
        let (store, store_dir) = temp_store("returns-leased-promote");
        let runtime = FakeRuntime::idle(&[TARGET, REQUESTER]);
        runtime.next_step.set(true);
        let now = Utc::now();
        let at = |secs: i64| now + chrono::Duration::seconds(secs);
        answered(&world, &runtime, &store, now);
        crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
        let event_turn = store.snapshot().unwrap().leases[REQUESTER].turn_id.clone().unwrap();
        answer_more(&world, &runtime, &store, "b", at(1));
        crate::asks::scheduler::tick(&world, &runtime, &store, at(121)).unwrap();
        let message_id = runtime.steered.borrow().last().unwrap().1.message_id.clone();
        crate::asks::observe::observe(&store, REQUESTER, "turn/completed", &json!({"turn": {"id": event_turn, "status": "completed"}}), at(130)).unwrap();
        runtime.set(REQUESTER, |status| status.active_turn_id = None);
        let state = store.snapshot().unwrap();
        assert!(state.next_step_leases.is_empty());
        let promoted = &state.leases[REQUESTER];
        assert_eq!(promoted.native_message_id, message_id);
        assert!(promoted.turn_id.is_none());
        assert!(is_next_step_event(&promoted.native_message_id) && matches!(promoted.holder, LeaseHolder::Event { .. }));
        // Pending: held; read in a turn of its own: acknowledged, released.
        let report = crate::asks::scheduler::tick(&world, &runtime, &store, at(131)).unwrap();
        assert!(report.released.is_empty() && report.events.is_empty());
        runtime.native.borrow_mut().insert(message_id, NativeMessageStatus::Persisted { turn_id: "turn-own".into() });
        let report = crate::asks::scheduler::tick(&world, &runtime, &store, at(132)).unwrap();
        assert_eq!(report.released, vec![REQUESTER.to_string()]);
        let state = store.snapshot().unwrap();
        assert!(state.leases.is_empty() && state.next_step_leases.is_empty());
        assert!(state.deliveries.values().all(|d| d.state == DeliveryState::Acknowledged));
        assert_eq!(runtime.steered.borrow().len(), 1);
        for dir in [dir, store_dir] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// Decision: an ask-leased turn (the requester handles a delegation it
    /// received) is a busy turn like any other; its own delegations' results
    /// follow the same rule, beside the ask's lease, which stays untouched.
    /// A lease not bound to a turn (a submit in flight) still waits, and a
    /// lease of a turn that is no longer the running one waits for its
    /// release (covered with an event lease above).
    #[test]
    fn ask_leased_requester_reads_results_at_its_next_step_too() {
        let (world, dir) = world("returns-ask-leased");
        let (store, store_dir) = temp_store("returns-ask-leased");
        let runtime = FakeRuntime::idle(&[TARGET, REQUESTER]);
        runtime.next_step.set(true);
        let now = Utc::now();
        let at = |secs: i64| now + chrono::Duration::seconds(secs);
        answered(&world, &runtime, &store, now);
        // REQUESTER handles a delegation from TARGET in turn-ask; its lease
        // is not bound to a turn yet.
        let lease = store
            .with_state(|state| {
                let mut handled = new_ask("handled", "handle this");
                handled.requester = Requester::Agent { cutex_session_id: TARGET.into(), name: None };
                handled.target_cutex_session_id = REQUESTER.into();
                let handled = state.create_ask(handled, now).unwrap().0.ask_id;
                state.update_ask(&handled, now, |ask| {
                    let mut record = AttemptRecord::submitting(1, "m-handled".into(), 1, now_rfc3339(now));
                    record.state = AttemptState::Running;
                    record.turn_id = Some("turn-ask".into());
                    ask.attempts.push(record);
                    ask.execution = ExecutionState::Running;
                });
                let lease = AdmissionLease {
                    cutex_session_id: REQUESTER.into(),
                    holder: LeaseHolder::Ask { ask_id: handled, attempt: 1 },
                    native_message_id: "m-handled".into(),
                    runtime_generation: 1,
                    turn_id: None,
                    since: now_rfc3339(now),
                };
                state.leases.insert(REQUESTER.into(), lease.clone());
                lease
            })
            .unwrap();
        runtime.set(REQUESTER, |status| status.active_turn_id = Some("turn-ask".into()));
        crate::asks::scheduler::tick(&world, &runtime, &store, at(300)).unwrap();
        assert!(runtime.steered.borrow().is_empty(), "a lease without a turn waits");
        let bound = AdmissionLease { turn_id: Some("turn-ask".into()), ..lease };
        store.with_state(|state| state.leases.insert(REQUESTER.into(), bound.clone())).unwrap();
        // Bound to the running turn: read at the next step, beside it.
        assert_eq!(crate::asks::scheduler::tick(&world, &runtime, &store, at(302)).unwrap().events.len(), 1);
        let message_id = runtime.steered.borrow().last().unwrap().1.message_id.clone();
        let state = store.snapshot().unwrap();
        assert_eq!(state.leases[REQUESTER], bound);
        assert!(state.next_step_leases.contains_key(REQUESTER));
        runtime.native.borrow_mut().insert(message_id, NativeMessageStatus::Persisted { turn_id: "turn-ask".into() });
        crate::asks::scheduler::tick(&world, &runtime, &store, at(303)).unwrap();
        let state = store.snapshot().unwrap();
        assert_eq!(state.leases[REQUESTER], bound);
        assert!(state.next_step_leases.is_empty());
        assert!(state.deliveries.values().filter(|d| d.requester_cutex_session_id == REQUESTER).all(|d| d.state == DeliveryState::Acknowledged));
        for dir in [dir, store_dir] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// The in-flight record beside a lease is reconciled like a next-step
    /// lease, and only ever removes itself: lost (absent) → sent again once;
    /// a volatile runtime restarted → back to pending; a durable one keeps the
    /// key across the restart; unreconcilable past the grace → passive. The
    /// running turn's lease is never touched by any of these.
    #[test]
    fn next_step_record_beside_a_lease_is_reconciled_without_touching_it() {
        let (world, dir) = world("returns-leased-reconcile");
        let runtime = FakeRuntime::idle(&[TARGET, REQUESTER]);
        runtime.next_step.set(true);
        let now = Utc::now();
        let at = |secs: i64| now + chrono::Duration::seconds(secs);
        // An event turn with one more result sent beside its lease.
        let setup = |label: &str| {
            let (store, store_dir) = temp_store(label);
            runtime.set(REQUESTER, |status| {
                status.active_turn_id = None;
                status.runtime_generation = 1;
            });
            answered(&world, &runtime, &store, now);
            crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
            let lease = store.snapshot().unwrap().leases[REQUESTER].clone();
            answer_more(&world, &runtime, &store, "b", at(1));
            crate::asks::scheduler::tick(&world, &runtime, &store, at(121)).unwrap();
            let message_id = runtime.steered.borrow().last().unwrap().1.message_id.clone();
            assert!(store.snapshot().unwrap().next_step_leases.contains_key(REQUESTER));
            (store, store_dir, lease, message_id)
        };
        // Lost: released and sent again under a new id beside the same lease.
        let (store, store_dir, lease, first) = setup("returns-leased-absent");
        runtime.native.borrow_mut().insert(first.clone(), NativeMessageStatus::Absent);
        let report = crate::asks::scheduler::tick(&world, &runtime, &store, at(122)).unwrap();
        assert!(report.released.is_empty());
        let state = store.snapshot().unwrap();
        assert_eq!(state.leases[REQUESTER], lease);
        let second = state.next_step_leases[REQUESTER].native_message_id.clone();
        assert_ne!(first, second);
        assert_eq!(runtime.steered.borrow().last().unwrap().1.message_id, second);
        let _ = std::fs::remove_dir_all(store_dir);
        // A volatile runtime restarted (the store, read back from disk, still
        // has the record): back to pending, the old lease untouched.
        let (store, store_dir, lease, _) = setup("returns-leased-restart");
        let reread: AskState = serde_json::from_slice(&std::fs::read(store_dir.join("asks-v1.json")).unwrap()).unwrap();
        assert!(reread.next_step_leases.contains_key(REQUESTER));
        runtime.set(REQUESTER, |status| {
            status.runtime_generation = 2;
            status.active_turn_id = None;
        });
        let steered = runtime.steered.borrow().len();
        crate::asks::scheduler::tick(&world, &runtime, &store, at(122)).unwrap();
        let state = store.snapshot().unwrap();
        assert_eq!(state.leases[REQUESTER], lease);
        assert!(state.next_step_leases.is_empty());
        assert!(state.deliveries.values().any(|d| d.state == DeliveryState::Pending && !d.passive));
        assert_eq!(runtime.steered.borrow().len(), steered, "not beside a turn the new runtime does not run");
        let _ = std::fs::remove_dir_all(store_dir);
        // A durable inbox keeps the key across the restart and reads it.
        runtime.durable_next_step.set(true);
        let (store, store_dir, lease, message_id) = setup("returns-leased-durable");
        runtime.set(REQUESTER, |status| status.runtime_generation = 2);
        runtime.native.borrow_mut().insert(message_id.clone(), NativeMessageStatus::Persisted { turn_id: "turn-later".into() });
        crate::asks::scheduler::tick(&world, &runtime, &store, at(122)).unwrap();
        let state = store.snapshot().unwrap();
        assert_eq!(state.leases[REQUESTER], lease);
        assert!(state.next_step_leases.is_empty());
        assert!(state.deliveries.values().all(|d| d.state == DeliveryState::Acknowledged));
        let _ = std::fs::remove_dir_all(store_dir);
        // Unreconcilable past the grace: passive, owner told, lease kept.
        let (store, store_dir, lease, message_id) = setup("returns-leased-unavailable");
        runtime.native.borrow_mut().insert(message_id, NativeMessageStatus::Unavailable("lost".into()));
        let grace = crate::asks::EVENT_UNAVAILABLE_GRACE_SECS;
        crate::asks::scheduler::tick(&world, &runtime, &store, at(122)).unwrap();
        let report = crate::asks::scheduler::tick(&world, &runtime, &store, at(122 + grace)).unwrap();
        assert!(report.released.is_empty());
        let state = store.snapshot().unwrap();
        assert_eq!(state.leases[REQUESTER], lease);
        assert!(state.next_step_leases.is_empty());
        assert!(state.deliveries.values().any(|d| d.passive && d.state == DeliveryState::Pending));
        assert!(state.notices.iter().any(|notice| notice.params["code"] == crate::asks::RESULTS_OUTCOME_UNKNOWN));
        for dir in [dir, store_dir] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// Store format: `nextStepLeases` is additive (absent when empty, so a
    /// store without one is byte-identical; an older store reads with none).
    /// A result ride-along names its delegation (`askId`), so a client can
    /// show and open it; `label` stays the title or the English fallback.
    #[test]
    fn result_ridealong_names_its_ask() {
        let (world, dir) = world("returns-ridealong-ask-id");
        let (store, store_dir) = temp_store("returns-ridealong-ask-id");
        let runtime = FakeRuntime::idle(&[TARGET, REQUESTER]);
        runtime.set(REQUESTER, |status| status.active_turn_id = Some("turn-busy".into()));
        let ask = answered(&world, &runtime, &store, Utc::now());
        let rows = crate::management::v2::admission::ask_next_step(&store.snapshot().unwrap(), REQUESTER);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].delivery_kind, "answer");
        assert_eq!(rows[0].ask_id.as_deref(), Some(ask.as_str()));
        let value = serde_json::to_value(&rows[0]).unwrap();
        assert_eq!(value["askId"], json!(ask));
        assert!(value["label"].is_string());
        // admission-ridealong-detail: the delegate and the answer's first line.
        assert_eq!(value["from"], "Kimi");
        assert_eq!(value["preview"], "done: 42");
        assert!(value.get("fromHost").is_none());
        let mut bus = rows[0].clone();
        bus.ask_id = None;
        assert!(serde_json::to_value(&bus).unwrap().get("askId").is_none());
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    #[test]
    fn next_step_leases_are_optional_in_the_store() {
        let empty = serde_json::to_value(AskState::default()).unwrap();
        assert!(empty.get("nextStepLeases").is_none());
        let old: AskState = serde_json::from_value(json!({"leases": {}})).unwrap();
        assert!(old.next_step_leases.is_empty());
        let mut state = AskState::default();
        state.next_step_leases.insert(REQUESTER.into(), AdmissionLease {
            cutex_session_id: REQUESTER.into(),
            holder: LeaseHolder::Event { event_id: "aev-1".into() },
            native_message_id: format!("{NEXT_STEP_EVENT_PREFIX}-1"),
            runtime_generation: 1,
            turn_id: None,
            since: "2026-10-12T00:00:00Z".into(),
        });
        let value = serde_json::to_value(&state).unwrap();
        assert_eq!(value["nextStepLeases"][REQUESTER]["nativeMessageId"], format!("{NEXT_STEP_EVENT_PREFIX}-1"));
        assert_eq!(serde_json::from_value::<AskState>(value).unwrap(), state);
        // Without its event it is dropped on the next write.
        promote_next_step_leases(&mut state);
        assert!(state.next_step_leases.is_empty() && state.leases.is_empty());
    }

    /// r217h2: a next-step event the runtime lost (status absent, or a
    /// runtime restart before it was read) goes back to pending and is sent
    /// once more; one that was read is never sent again.
    #[test]
    fn lost_next_step_results_are_sent_again_once() {
        let (world, dir) = world("returns-next-step-lost");
        let (store, store_dir) = temp_store("returns-next-step-lost");
        let runtime = FakeRuntime::idle(&[TARGET, REQUESTER]);
        runtime.next_step.set(true);
        let now = Utc::now();
        runtime.set(REQUESTER, |status| status.active_turn_id = Some("turn-busy".into()));
        answered(&world, &runtime, &store, now);
        let at = |secs: i64| now + chrono::Duration::seconds(secs);
        crate::asks::scheduler::tick(&world, &runtime, &store, at(150)).unwrap();
        let first = runtime.steered.borrow().last().cloned().unwrap().1.message_id;
        // Absent: released, and sent again under a new id (same tick).
        runtime.native.borrow_mut().insert(first.clone(), NativeMessageStatus::Absent);
        crate::asks::scheduler::tick(&world, &runtime, &store, at(151)).unwrap();
        assert_eq!(runtime.steered.borrow().len(), 2);
        let second = runtime.steered.borrow().last().cloned().unwrap().1.message_id;
        assert_ne!(first, second);
        let state = store.snapshot().unwrap();
        assert_eq!(state.events.len(), 1, "the lost event is gone");
        assert!(state.deliveries.values().all(|d| d.state == DeliveryState::QueuedAsEvent));
        crate::asks::scheduler::tick(&world, &runtime, &store, at(152)).unwrap();
        assert_eq!(runtime.steered.borrow().len(), 2);
        // The runtime restarts before reading it: back to pending; the new
        // (idle) runtime gets one exclusive event.
        runtime.set(REQUESTER, |status| {
            status.runtime_generation = 2;
            status.active_turn_id = None;
        });
        let exclusive_before = runtime.submitted.borrow().len();
        crate::asks::scheduler::tick(&world, &runtime, &store, at(160)).unwrap();
        crate::asks::scheduler::tick(&world, &runtime, &store, at(161)).unwrap();
        assert_eq!(runtime.submitted.borrow().len(), exclusive_before + 1);
        assert_eq!(runtime.steered.borrow().len(), 2);
        let state = store.snapshot().unwrap();
        assert!(state.deliveries.values().all(|d| d.state == DeliveryState::Acknowledged));
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    #[test]
    fn durable_next_step_results_keep_their_identity_across_restart_and_unknown_status() {
        let (world, dir) = world("returns-durable-next-step");
        let (store, store_dir) = temp_store("returns-durable-next-step");
        let runtime = FakeRuntime::idle(&[TARGET, REQUESTER]);
        runtime.next_step.set(true);
        runtime.durable_next_step.set(true);
        let now = Utc::now();
        runtime.set(REQUESTER, |status| status.active_turn_id = Some("turn-busy".into()));
        answered(&world, &runtime, &store, now);
        crate::asks::scheduler::tick(&world, &runtime, &store, now + chrono::Duration::seconds(150)).unwrap();
        let message_id = runtime.steered.borrow().last().unwrap().1.message_id.clone();
        let exclusive_before = runtime.submitted.borrow().len();
        runtime.set(REQUESTER, |status| { status.runtime_generation = 2; status.active_turn_id = None; });
        runtime.native.borrow_mut().insert(message_id.clone(), NativeMessageStatus::Unavailable("timeout".into()));
        crate::asks::scheduler::tick(&world, &runtime, &store, now + chrono::Duration::seconds(151)).unwrap();
        assert_eq!(runtime.steered.borrow().len(), 1);
        assert_eq!(runtime.submitted.borrow().len(), exclusive_before);
        assert!(store.snapshot().unwrap().deliveries.values().all(|delivery| delivery.state == DeliveryState::QueuedAsEvent));
        runtime.native.borrow_mut().insert(message_id, NativeMessageStatus::Persisted { turn_id: "turn-busy".into() });
        crate::asks::scheduler::tick(&world, &runtime, &store, now + chrono::Duration::seconds(152)).unwrap();
        assert_eq!(runtime.submitted.borrow().len(), exclusive_before);
        assert!(store.snapshot().unwrap().deliveries.values().all(|delivery| delivery.state == DeliveryState::Acknowledged));
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    /// Integration review (07): a next-step result claimed into a busy turn
    /// that then failed / was interrupted is held, not replayed. The event is
    /// acknowledged by that turn, the requester's lease released and reported
    /// (the management driver flushes owner input queued behind it), and the
    /// same result is never submitted again.
    #[test]
    fn held_next_step_result_releases_the_requester_lease() {
        let (world, dir) = world("returns-held-next-step");
        let (store, store_dir) = temp_store("returns-held-next-step");
        let runtime = FakeRuntime::idle(&[TARGET, REQUESTER]);
        runtime.next_step.set(true);
        runtime.durable_next_step.set(true);
        let now = Utc::now();
        runtime.set(REQUESTER, |status| status.active_turn_id = Some("turn-busy".into()));
        answered(&world, &runtime, &store, now);
        crate::asks::scheduler::tick(&world, &runtime, &store, now + chrono::Duration::seconds(150)).unwrap();
        let message_id = runtime.steered.borrow().last().unwrap().1.message_id.clone();
        assert!(store.snapshot().unwrap().leases.contains_key(REQUESTER));
        let submitted = runtime.submitted.borrow().len();
        runtime.set(REQUESTER, |status| status.active_turn_id = None);
        runtime.native.borrow_mut().insert(message_id, NativeMessageStatus::Held {
            reason: "request_uncertain".into(), turn_id: Some("turn-busy".into()),
        });
        let report = crate::asks::scheduler::tick(&world, &runtime, &store, now + chrono::Duration::seconds(151)).unwrap();
        assert_eq!(report.released, vec![REQUESTER.to_string()]);
        let state = store.snapshot().unwrap();
        assert!(!state.leases.contains_key(REQUESTER));
        assert!(state.deliveries.values().all(|delivery| delivery.state == DeliveryState::Acknowledged));
        crate::asks::scheduler::tick(&world, &runtime, &store, now + chrono::Duration::seconds(152)).unwrap();
        assert_eq!(runtime.steered.borrow().len(), 1);
        assert_eq!(runtime.submitted.borrow().len(), submitted);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    /// Integration review (07): a next-step event whose outcome cannot be
    /// reconciled (lost submit: the durable inbox does not know it, status
    /// keeps failing) no longer holds the requester's lease forever. Past
    /// the grace its results become passive (never re-sent), the owner is
    /// told, and the lease is released; a status that recovers in between
    /// starts the grace over.
    #[test]
    fn unreconcilable_next_step_event_is_settled_after_the_grace() {
        let (world, dir) = world("returns-unreconcilable");
        let (store, store_dir) = temp_store("returns-unreconcilable");
        let runtime = FakeRuntime::idle(&[TARGET, REQUESTER]);
        runtime.next_step.set(true);
        runtime.durable_next_step.set(true);
        let now = Utc::now();
        let at = |secs: i64| now + chrono::Duration::seconds(secs);
        runtime.set(REQUESTER, |status| status.active_turn_id = Some("turn-busy".into()));
        answered(&world, &runtime, &store, now);
        crate::asks::scheduler::tick(&world, &runtime, &store, at(150)).unwrap();
        let message_id = runtime.steered.borrow().last().unwrap().1.message_id.clone();
        let submitted = runtime.submitted.borrow().len();
        let grace = crate::asks::EVENT_UNAVAILABLE_GRACE_SECS;
        let lost = || NativeMessageStatus::Unavailable("next-step receipt binding mismatch".into());
        runtime.native.borrow_mut().insert(message_id.clone(), lost());
        crate::asks::scheduler::tick(&world, &runtime, &store, at(151)).unwrap();
        // Recovers (pending) once: the grace starts over.
        runtime.native.borrow_mut().insert(message_id.clone(), NativeMessageStatus::Pending);
        crate::asks::scheduler::tick(&world, &runtime, &store, at(151 + grace - 10)).unwrap();
        runtime.native.borrow_mut().insert(message_id.clone(), lost());
        let report = crate::asks::scheduler::tick(&world, &runtime, &store, at(151 + grace)).unwrap();
        assert!(report.released.is_empty());
        assert!(store.snapshot().unwrap().leases.contains_key(REQUESTER), "inside the grace the lease holds");
        let report = crate::asks::scheduler::tick(&world, &runtime, &store, at(151 + 2 * grace)).unwrap();
        assert_eq!(report.released, vec![REQUESTER.to_string()]);
        let state = store.snapshot().unwrap();
        assert!(!state.leases.contains_key(REQUESTER));
        assert!(state.events.is_empty());
        assert!(state.deliveries.values().all(|d| d.state == DeliveryState::Pending && d.passive));
        assert!(state.notices.iter().any(|notice| notice.method == crate::asks::scheduler::DELIVERY_FAILED_EVENT
            && notice.params["code"] == crate::asks::RESULTS_OUTCOME_UNKNOWN));
        // Never sent again, as next step or as an exclusive turn.
        runtime.set(REQUESTER, |status| status.active_turn_id = None);
        crate::asks::scheduler::tick(&world, &runtime, &store, at(152 + 2 * grace)).unwrap();
        assert_eq!(runtime.steered.borrow().len(), 1);
        assert_eq!(runtime.submitted.borrow().len(), submitted);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    /// r217h2: an idle requester still gets its own exclusive turn at once
    /// (no `soon`), and a requester inside delegation_wait still gets the
    /// results from the wait, also when busy past the busy wait.
    #[test]
    fn idle_and_waiting_requesters_are_unchanged() {
        let (world, dir) = world("returns-next-step-idle");
        let (store, store_dir) = temp_store("returns-next-step-idle");
        let runtime = FakeRuntime::idle(&[TARGET, REQUESTER]);
        runtime.next_step.set(true);
        let now = Utc::now();
        let ask = answered(&world, &runtime, &store, now);
        // Busy past the busy wait, but waiting on it: the wait claims it.
        runtime.set(REQUESTER, |status| status.active_turn_id = Some("turn-busy".into()));
        store
            .with_state(|state| {
                state.waits.insert(
                    "w".into(),
                    crate::asks::store::WaitRecord {
                        wait_id: "w".into(),
                        cutex_session_id: REQUESTER.into(),
                        action_id: "x".into(),
                        asks: vec![ask.clone()],
                        all: false,
                        started_at: now_rfc3339(now),
                        deadline: now_rfc3339(now + chrono::Duration::seconds(600)),
                        cancel_epoch: 0,
                    },
                )
            })
            .unwrap();
        let later = now + chrono::Duration::seconds(300);
        assert!(crate::asks::scheduler::tick(&world, &runtime, &store, later).unwrap().events.is_empty());
        assert!(runtime.steered.borrow().is_empty());
        let wake = store.snapshot().unwrap();
        assert_eq!(crate::asks::waits::wake_reason(&wake, REQUESTER, &wake.waits["w"], later), Some("answered"));
        // Idle and not waiting: one exclusive turn at once, no `soon`.
        store.with_state(|state| state.waits.clear()).unwrap();
        runtime.set(REQUESTER, |status| status.active_turn_id = None);
        let report = crate::asks::scheduler::tick(&world, &runtime, &store, now + chrono::Duration::seconds(1)).unwrap();
        assert_eq!(report.events.len(), 1);
        assert!(runtime.steered.borrow().is_empty());
        let (session, event) = runtime.submitted.borrow().last().cloned().unwrap();
        assert_eq!(session, REQUESTER);
        assert!(!is_next_step_event(&event.message_id));
        let state = store.snapshot().unwrap();
        assert!(matches!(state.leases[REQUESTER].holder, LeaseHolder::Event { .. }));
        assert!(state.leases[REQUESTER].turn_id.is_some());
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    /// An ANSWER event over the requester's receiver byte policy is never
    /// re-sent (it would be refused again on every tick): its items become
    /// passive (delegation_wait / agent_list still return them) and the owner
    /// gets one `cutex/ask/deliveryFailed`.
    #[test]
    fn too_large_results_event_turns_passive_and_tells_the_owner_once() {
        let (world, dir) = world("returns-too-large");
        let (store, store_dir) = temp_store("returns-too-large");
        let runtime = FakeRuntime::idle(&[TARGET, REQUESTER]);
        let now = Utc::now();
        let ask = store.with_state(|state| state.create_ask(new_ask("a", "x"), now).unwrap().0.ask_id).unwrap();
        crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
        crate::asks::observe::observe(
            &store,
            TARGET,
            "item/completed",
            &json!({"turnId": "turn-1", "item": {"type": "agentMessage", "text": "done: 42"}}),
            now,
        )
        .unwrap();
        crate::asks::observe::observe(&store, TARGET, "turn/completed", &json!({"turn": {"id": "turn-1", "status": "completed"}}), now).unwrap();
        finish_turn(&runtime, &store, TARGET);
        runtime.outcomes.borrow_mut().push(SubmitOutcome::TooLarge { item_bytes: Some(12_000), limit: Some(10_000) });
        let report = crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
        assert!(report.events.is_empty());
        assert_eq!(report.degraded, vec![REQUESTER.to_string()]);
        let submitted = runtime.submitted.borrow().len();
        let state = store.snapshot().unwrap();
        assert!(state.leases.is_empty());
        assert!(state.events.is_empty());
        let items: Vec<&DeliveryRecord> = state.deliveries.values().filter(|d| d.ask_id == ask).collect();
        assert!(!items.is_empty());
        assert!(items.iter().all(|d| d.passive && d.state == DeliveryState::Pending));
        let notices: Vec<_> = state
            .notices
            .iter()
            .filter(|notice| notice.method == crate::asks::scheduler::DELIVERY_FAILED_EVENT)
            .collect();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].params["kind"], "results");
        assert_eq!(notices[0].params["code"], crate::asks::EVENT_TOO_LARGE);
        assert!(notices[0].params["detail"].as_str().unwrap().contains("about 12000 bytes"));
        // Nothing is re-sent later; a wait still gets the results.
        for later in [now + chrono::Duration::seconds(5), now + chrono::Duration::hours(2)] {
            crate::asks::scheduler::tick(&world, &runtime, &store, later).unwrap();
        }
        assert_eq!(runtime.submitted.borrow().len(), submitted);
        let claimed = store
            .with_state(|state| super::super::delivery::claim_for_wait(state, REQUESTER, &[ask.clone()], "w", now))
            .unwrap();
        assert!(!claimed.is_empty());
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    /// r210 backstop: a compaction in the requester's ANSWER event turn
    /// (natives before r50 drop the event item) steers one reminder listing
    /// the delivered delegations and those awaiting acceptance.
    #[test]
    fn compaction_in_an_event_turn_reminds_the_requester_once() {
        let (world, dir) = world("returns-compaction");
        let (store, store_dir) = temp_store("returns-compaction");
        let runtime = FakeRuntime::idle(&[TARGET, REQUESTER]);
        let now = Utc::now();
        let mut reviewed = new_ask("a", "Check the counts");
        reviewed.options.review = true;
        let ask = store.with_state(|state| state.create_ask(reviewed, now).unwrap().0.ask_id).unwrap();
        crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
        let target = |method: &str, params: Value| crate::asks::observe::observe(&store, TARGET, method, &params, now).unwrap();
        target("item/completed", json!({"turnId": "turn-1", "item": {"type": "agentMessage", "text": "counts ok"}}));
        target("turn/completed", json!({"turn": {"id": "turn-1", "status": "completed"}}));
        finish_turn(&runtime, &store, TARGET);
        assert_eq!(crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap().events.len(), 1);
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&ask].review, Some(ReviewState::AwaitingAcceptance));
        let turn = state.leases[REQUESTER].turn_id.clone().unwrap();
        let requester = |method: &str, params: Value| crate::asks::observe::observe(&store, REQUESTER, method, &params, now).unwrap();
        // Not a compaction yet; then one compaction seen twice.
        requester("item/started", json!({"turnId": turn, "item": {"type": "contextCompaction", "id": "c1"}}));
        assert!(store.snapshot().unwrap().events.values().all(|event| !event.compaction_reminder_pending));
        requester("item/completed", json!({"turnId": turn, "item": {"type": "contextCompaction", "id": "c1"}}));
        requester("thread/compacted", json!({"threadId": "t", "turnId": turn}));
        crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
        crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
        let steered = runtime.steered.borrow();
        assert_eq!(steered.len(), 1, "reminded once");
        assert_eq!(steered[0].0, REQUESTER);
        assert_eq!(steered[0].1.event_type, render::REMINDER_EVENT);
        assert_eq!(steered[0].1.asks, vec![ask.clone()]);
        let text = &steered[0].1.text;
        assert!(text.contains("Delivered in this turn") && text.contains(&crate::short_ids::agent_id(&ask)) && text.contains("Check the counts"), "{text}");
        assert!(text.contains("delegation_read(") && text.contains("awaiting your acceptance"), "{text}");
        drop(steered);
        assert!(store.snapshot().unwrap().events.values().all(|event| !event.compaction_reminder_pending));
        // The event turn ends as before: lease released.
        requester("turn/completed", json!({"turn": {"id": turn, "status": "completed"}}));
        assert!(!store.snapshot().unwrap().leases.contains_key(REQUESTER));
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    /// r209 reads an event record written with the r210 fields.
    #[test]
    fn event_record_compaction_fields_are_optional() {
        let old = json!({"eventId": "aev-1", "cutexSessionId": REQUESTER, "deliveryIds": [], "nativeMessageId": "m", "acknowledged": true, "createdAt": "2026-10-08T00:00:00Z"});
        let record: EventOutboxRecord = serde_json::from_value(old.clone()).unwrap();
        assert!(!record.compaction_reminder_pending && record.compacted_at.is_none());
        assert_eq!(serde_json::to_value(&record).unwrap(), old, "unset fields are not written");
    }
}
