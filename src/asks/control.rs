//! Owner and requester control of asks (asks design §5.10).
//!
//! - **Owner stop** (`cutex/turn/stop`), owner decision 2026-10-12
//!   ("停止 = everything waits", replaces the r208 rule that a stop never
//!   pauses delegation): ends the session's `delegation_wait` calls and
//!   records a **stop hold**, an [`AdmissionPause`] with origin `ownerStop`
//!   and `stopHold: true` (an existing explicit pause just gains
//!   `stopHold`). While it exists no ask is admitted, no ANSWER/QUESTION
//!   event is delivered and no ride-along delivery (bus wake, tell, alarm)
//!   starts a turn (`crate::management::v2::admission::delivery_may_start`).
//!   The native external-input gate the interrupt closed stays closed, so
//!   items native already accepted wait too; every release reopens it.
//!   Released by `cutex/userInput/queue/resume` ([`release_stop_hold`]),
//!   `cutex/asks/resume`, or the owner's next message that starts or steers
//!   a turn (a message the owner only queues releases nothing). Durable in
//!   the ask store, so it survives a daemon restart.
//! - **Owner delegation pause** (`cutex/asks/pause`): the session admits no
//!   asks (queued ones show `paused(owner)`) and receives no ANSWER/QUESTION
//!   events; auto-wake never undoes it. Ride-along bus deliveries continue.
//! - Both are cleared by `cutex/asks/resume`, or by the owner's next message
//!   to that session (`cutex/userInput/submit` started / steered / queued;
//!   `queued` does not clear a stop hold). Every change publishes
//!   `cutex/asks/pauseChanged` on the session stream.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use super::model::*;
use super::store::{AskState, AskStore, QueuedCommand, RuntimeCommand};
use super::{new_id, now_rfc3339};

/// Session event for every change of the delegation pause.
pub const PAUSE_CHANGED_EVENT: &str = "cutex/asks/pauseChanged";
/// [`AdmissionPause::origin`] of an explicit owner pause.
pub const ORIGIN_OWNER_PAUSE: &str = "ownerPause";
/// [`AdmissionPause::origin`] of the pause an owner turn stop creates.
pub const ORIGIN_OWNER_STOP: &str = "ownerStop";
/// `held` of a ride-along the owner stop holds (admission `nextStep`).
pub const HELD_OWNER_STOP: &str = "ownerStop";
/// [`AdmissionPause::origin`] of the pause a closed native input gate
/// creates (Claude: the adapter halted its queue after an interrupted or
/// failed turn; Codex: the N2 external-input gate an interrupt closed).
pub const ORIGIN_RUNTIME_GATE: &str = "runtimeGate";
/// `heldByRuntime.native` of asks waiting on a closed gate.
pub const NATIVE_GATE_CLOSED: &str = "gate_closed";

pub fn queue_command(state: &mut AskState, command: RuntimeCommand, now: DateTime<Utc>) {
    if state.commands.iter().any(|queued| queued.command == command) {
        return;
    }
    state.commands.push(QueuedCommand { command_id: new_id("cmd"), command, created_at: now_rfc3339(now) });
}

/// Session event for a handover of an ask's control (design 044).
pub const HANDOVER_EVENT: &str = "cutex/ask/handover";

/// What [`apply_handover`] did, for the director_address sidecar.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HandoverApplied {
    /// Pending items that now go to the new controller (same delivery ids:
    /// nothing is copied).
    pub rerouted: Vec<String>,
    /// Items already claimed by a wait or carried by an event to the earlier
    /// controller (QueuedAsEvent may be unknown at native): they stay with it
    /// and are never resent; the sidecar keeps them as pending reconcile until
    /// their receipt settles.
    pub in_flight: Vec<String>,
    /// Waits of the earlier controller on this ask (they end with
    /// `handedOver`; the new controller waits itself).
    pub waits_ended: Vec<String>,
    /// Open questions of the ask: same ids, native request, attempt and
    /// generation; only who may answer changed.
    pub open_questions: Vec<String>,
    /// The handover summary item for the new controller (none on a repeat).
    pub summary_delivery: Option<String>,
}

/// Apply a handover of `ask_id`'s control to `to` inside the ask store
/// (call it from `AskStore::with_state` AFTER the director_address sidecar
/// names `to` as controller at `controller_revision`). It never changes
/// `requester`, `action_id`, `request_sha256`, the brief, the handler or its
/// attempt / runtime generation, and never moves a wait or resends an item:
/// pending items reroute through `delivery::recipient`; in-flight ones stay
/// with `from`; `from`'s waits on the ask end (`waits::wake_reason`
/// `handedOver`); questions keep their ids. Idempotent per
/// (ask, controller_revision): a repeat records nothing new.
pub fn apply_handover(
    state: &mut AskState,
    ask_id: &str,
    from: Option<&str>,
    to: &str,
    controller_revision: u64,
    now: DateTime<Utc>,
) -> Result<HandoverApplied, super::store::AskError> {
    use super::store::AskError;
    let ask = state.asks.get(ask_id).cloned().ok_or_else(|| AskError::new("ask_not_found", "no such delegation"))?;
    if ask.remote.is_some() || matches!(ask.requester, Requester::Peer { .. }) {
        return Err(AskError::new("remote_unsupported", "a remote delegation's control is not handed over (P0)"));
    }
    if !ask.controlled_by(to)? {
        return Err(AskError::new("handover_not_recorded", "the director_address record does not name this controller yet"));
    }
    let mut applied = HandoverApplied::default();
    for delivery in state.deliveries.values().filter(|delivery| delivery.ask_id == ask_id) {
        match delivery.state {
            DeliveryState::Pending if super::delivery::is_for(state, delivery, to) => applied.rerouted.push(delivery.delivery_id.clone()),
            DeliveryState::ClaimedByWait | DeliveryState::QueuedAsEvent
                if from.is_some_and(|from| super::delivery::is_for(state, delivery, from)) =>
            {
                applied.in_flight.push(delivery.delivery_id.clone())
            }
            _ => {}
        }
    }
    if let Some(from) = from {
        applied.waits_ended = state
            .waits
            .values()
            .filter(|wait| wait.cutex_session_id == from && wait.asks.iter().any(|id| id == ask_id))
            .map(|wait| wait.wait_id.clone())
            .collect();
    }
    applied.open_questions = state
        .questions
        .values()
        .filter(|question| question.ask_id == ask_id && question.state == QuestionState::Open)
        .map(|question| question.question_id.clone())
        .collect();
    applied.rerouted.sort();
    applied.in_flight.sort();
    applied.waits_ended.sort();
    applied.open_questions.sort();
    // The new controller learns of it once; actionable when something waits
    // for a decision there (an open question, an unread result, a review).
    let decision = !applied.open_questions.is_empty()
        || !applied.rerouted.is_empty()
        || ask.review == Some(ReviewState::AwaitingAcceptance);
    let passive = !(ask.is_open() && decision);
    applied.summary_delivery = super::delivery::record_handover_delivery(state, &ask, controller_revision, passive, now);
    if applied.summary_delivery.is_some() {
        let mut sessions = super::store::AskState::ask_sessions(&ask);
        if let Some(from) = from {
            sessions.push(from.to_string());
        }
        state.notice(
            HANDOVER_EVENT,
            sessions,
            json!({
                "askId": ask_id,
                "originalRequester": ask.requester.session(),
                "from": from,
                "controller": to,
                "controllerRevision": controller_revision,
                "rerouted": applied.rerouted,
                "inFlight": applied.in_flight,
                "openQuestions": applied.open_questions,
            }),
        );
    }
    Ok(applied)
}

/// `asksPaused` as clients see it: `{since, by, origin, stopHold}` or `null`.
pub fn pause_json(pause: Option<&AdmissionPause>) -> Value {
    match pause {
        Some(pause) => json!({
            "since": pause.since,
            "by": pause.by,
            "origin": pause.origin.as_deref().unwrap_or("legacy"),
            "stopHold": pause.stop_hold,
        }),
        None => Value::Null,
    }
}

fn pause_changed_notice(state: &mut AskState, session: &str, reason: &str) {
    let paused = pause_json(state.pauses.get(session));
    state.notice(PAUSE_CHANGED_EVENT, vec![session.to_string()], json!({"asksPaused": paused, "reason": reason}));
}

/// Whether an owner stop holds the ride-alongs of `session`.
pub fn stop_held(state: &AskState, session: &str) -> bool {
    state.pauses.get(session).is_some_and(|pause| pause.stop_hold)
}

/// Record an owner pause on `session`'s admission and hold its queued asks.
/// Returns whether the pause was created now.
pub fn owner_pause(state: &mut AskState, session: &str, now: DateTime<Utc>) -> bool {
    pause_with(state, session, ORIGIN_OWNER_PAUSE, false, None, now)
}

/// The hold of a queued ask under `pause`: a closed runtime gate reads as
/// `heldByRuntime(gate_closed)`, every other pause as `paused(owner)`.
pub fn pause_hold(pause: &AdmissionPause) -> Hold {
    if pause.origin.as_deref() == Some(ORIGIN_RUNTIME_GATE) {
        Hold::HeldByRuntime { native: Some(NATIVE_GATE_CLOSED.into()) }
    } else {
        Hold::Paused { by: pause.by }
    }
}

/// Whether `session`'s admission waits on a closed native gate (origin
/// `runtimeGate`).
pub fn runtime_gate_closed(state: &AskState, session: &str) -> bool {
    state.pauses.get(session).is_some_and(|pause| pause.origin.as_deref() == Some(ORIGIN_RUNTIME_GATE))
}

/// The runtime (generation `generation`) refused a delivery because its
/// input gate is closed (the last turn was interrupted or failed; tool
/// effects may be uncertain). Record it as a pause the owner sees and
/// releases (`cutex/asks/resume`, `cutex/userInput/queue/resume` or the
/// owner's next message): until then no ask or ask result is retried, and
/// the owner's own input is not queued behind them. An existing pause stays
/// as it is (every owner release reopens the gate anyway). Returns whether
/// the pause was created now.
pub fn runtime_gate_pause(state: &mut AskState, session: &str, generation: u64, now: DateTime<Utc>) -> bool {
    if state.pauses.contains_key(session) {
        return false;
    }
    pause_with(state, session, ORIGIN_RUNTIME_GATE, false, Some(generation), now)
}

/// A `runtimeGate` pause of `session` ends without an owner action: the
/// runtime restarted (`generation` differs) or the gate demonstrably opened
/// (a turn started on a runtime that only starts turns with an open gate).
/// Returns whether a pause ended.
pub fn runtime_gate_opened(state: &mut AskState, session: &str, generation: Option<u64>, reason: &str, now: DateTime<Utc>) -> bool {
    let Some(pause) = state.pauses.get(session).filter(|pause| pause.origin.as_deref() == Some(ORIGIN_RUNTIME_GATE)) else {
        return false;
    };
    if generation.is_some_and(|generation| pause.runtime_generation == Some(generation)) {
        return false;
    }
    state.pauses.remove(session);
    clear_pause_holds(state, session, now);
    pause_changed_notice(state, session, reason);
    true
}

/// The usage limit of `session` reset and delegations that failed on it
/// wait to be redelivered (quota auto-continue, which sends no continuation
/// of its own then). The failed turn closed the native input gate, and
/// nobody else would reopen it: the owner never acted, so the redelivery
/// would wait on a `runtimeGate` pause (host-a 2026-10-21: 4–20 min until
/// the owner resumed each agent by hand). Queue the gate resume and end
/// such a pause (reason `quotaReset`); an owner pause or stop is never
/// touched (the caller checks it first). Returns whether a pause ended.
pub fn quota_reset_reopen(state: &mut AskState, session: &str, now: DateTime<Utc>) -> bool {
    if state.pauses.get(session).is_some_and(|pause| pause.origin.as_deref() != Some(ORIGIN_RUNTIME_GATE)) {
        return false;
    }
    queue_command(state, RuntimeCommand::ResumeGate { cutex_session_id: session.to_string() }, now);
    runtime_gate_opened(state, session, None, "quotaReset", now)
}

/// Whether a native gate resume of `session` is queued (a refusal on a
/// closed gate then is not a reason to pause: the resume runs first).
pub fn gate_resume_queued(state: &AskState, session: &str) -> bool {
    state.commands.iter().any(|queued| {
        matches!(&queued.command, RuntimeCommand::ResumeGate { cutex_session_id } if cutex_session_id == session)
    })
}

/// [`quota_reset_reopen`] on the default store. Errors are logged.
pub fn quota_reset_reopen_default(session: &str) {
    let Ok(store) = AskStore::open_default() else {
        return;
    };
    match store.with_state(|state| quota_reset_reopen(state, session, Utc::now())) {
        Ok(_) => crate::management::v2::asks::publish_pending_notices(&store),
        Err(error) => eprintln!("asks: could not reopen the gate of {session} after its usage limit reset: {error:#}"),
    }
}

fn clear_pause_holds(state: &mut AskState, session: &str, now: DateTime<Utc>) {
    let gate_hold = Some(Hold::HeldByRuntime { native: Some(NATIVE_GATE_CLOSED.into()) });
    let held: Vec<String> = state
        .waiting_for_target(session)
        .iter()
        .filter(|ask| matches!(ask.hold, Some(Hold::Paused { by: PausedBy::Owner })) || ask.hold == gate_hold)
        .map(|ask| ask.ask_id.clone())
        .collect();
    for ask_id in held {
        state.update_ask(&ask_id, now, |ask| ask.hold = None);
    }
}

fn pause_with(state: &mut AskState, session: &str, origin: &str, stop: bool, generation: Option<u64>, now: DateTime<Utc>) -> bool {
    let created = !state.pauses.contains_key(session);
    let pause = state.pauses.entry(session.to_string()).or_insert_with(|| AdmissionPause {
        cutex_session_id: session.to_string(),
        by: PausedBy::Owner,
        since: now_rfc3339(now),
        origin: Some(origin.to_string()),
        stop_hold: false,
        runtime_generation: generation,
    });
    let stop_added = stop && !pause.stop_hold;
    pause.stop_hold |= stop;
    let paused_hold = pause_hold(pause);
    let queued: Vec<String> = state
        .waiting_for_target(session)
        .iter()
        .filter(|ask| ask.execution == ExecutionState::Queued)
        .map(|ask| ask.ask_id.clone())
        .collect();
    for ask_id in queued {
        let hold = Some(paused_hold.clone());
        let changed = state.update_ask(&ask_id, now, |ask| {
            let changed = ask.hold != hold;
            ask.hold = hold.clone();
            changed
        });
        if changed == Some(true) {
            if let Some(ask) = state.asks.get(&ask_id).cloned() {
                super::delivery::record_state_delivery(state, &ask, "paused", now);
            }
        }
    }
    if created || stop_added {
        let reason = if stop {
            ORIGIN_OWNER_STOP
        } else if origin == ORIGIN_RUNTIME_GATE {
            ORIGIN_RUNTIME_GATE
        } else {
            "paused"
        };
        pause_changed_notice(state, session, reason);
    }
    created
}

/// Clear an owner pause; queue the N2 gate resume. Returns whether a pause
/// existed. `reason`: `resumed` (explicit) or `ownerMessage`.
pub fn owner_resume(state: &mut AskState, session: &str, reason: &str, now: DateTime<Utc>) -> bool {
    let existed = state.pauses.remove(session).is_some();
    // Reopen the native gate even without a cutex pause: an interrupt from
    // another path may have closed it.
    queue_command(state, RuntimeCommand::ResumeGate { cutex_session_id: session.to_string() }, now);
    clear_pause_holds(state, session, now);
    if existed {
        pause_changed_notice(state, session, reason);
    }
    existed
}

/// Owner stop of a turn (`cutex/turn/stop`): end the session's waits and
/// hold everything that could start a turn (owner decision 2026-10-12). The
/// native gate the interrupt closes is left closed; a release reopens it.
pub fn owner_stop(state: &mut AskState, session: &str, now: DateTime<Utc>) {
    super::waits::cancel_waits(state, session);
    // A stop supersedes a closed-gate pause: it is the owner's own hold now.
    if runtime_gate_closed(state, session) {
        if let Some(pause) = state.pauses.get_mut(session) {
            pause.origin = Some(ORIGIN_OWNER_STOP.into());
            pause.runtime_generation = None;
        }
    }
    pause_with(state, session, ORIGIN_OWNER_STOP, true, None, now);
}

/// Release a stop hold (`cutex/userInput/queue/resume`): an `ownerStop`
/// pause ends; an explicit owner pause the owner then stopped stays, only
/// its stop hold ends. Reopens the native gate. Returns whether a hold
/// existed.
pub fn release_stop_hold(state: &mut AskState, session: &str, reason: &str, now: DateTime<Utc>) -> bool {
    // The owner resuming the queue also acknowledges a closed runtime gate.
    if runtime_gate_closed(state, session) {
        return owner_resume(state, session, reason, now);
    }
    let Some(pause) = state.pauses.get_mut(session).filter(|pause| pause.stop_hold) else {
        return false;
    };
    if pause.origin.as_deref() == Some(ORIGIN_OWNER_STOP) {
        return owner_resume(state, session, reason, now);
    }
    pause.stop_hold = false;
    queue_command(state, RuntimeCommand::ResumeGate { cutex_session_id: session.to_string() }, now);
    pause_changed_notice(state, session, reason);
    true
}

/// The owner's message to `session` landed with `disposition`
/// (`started` / `steered` / `queued`). A delegation pause ends; a stop hold
/// ends only with a message that started or steered a turn.
pub fn owner_message(state: &mut AskState, session: &str, disposition: &str, now: DateTime<Utc>) -> bool {
    if disposition == "queued" && stop_held(state, session) {
        return false;
    }
    owner_resume(state, session, "ownerMessage", now)
}

/// Best-effort hook for `cutex/turn/stop`: a store failure is logged, never
/// fails the stop.
pub fn owner_stop_default(session: &str) {
    let Ok(store) = AskStore::open_default() else {
        return;
    };
    match store.with_state(|state| owner_stop(state, session, Utc::now())) {
        Ok(()) => crate::management::v2::asks::publish_pending_notices(&store),
        Err(error) => eprintln!("asks: could not record the owner stop for {session}: {error:#}"),
    }
}

/// Best-effort hook for `cutex/userInput/queue/resume`: release the stop
/// hold of `session`. Cheap when there is none; never creates the store.
pub fn release_stop_hold_default(session: &str) -> bool {
    let Ok(store) = AskStore::open_default() else {
        return false;
    };
    let held = store.inspect(|state| stop_held(state, session) || runtime_gate_closed(state, session)).unwrap_or(false);
    if !held {
        return false;
    }
    match store.with_state(|state| release_stop_hold(state, session, "resumed", Utc::now())) {
        Ok(released) => {
            if released {
                crate::management::v2::asks::publish_pending_notices(&store);
            }
            released
        }
        Err(error) => {
            eprintln!("asks: could not release the stop hold of {session}: {error:#}");
            false
        }
    }
}

/// A turn started on `session`'s Claude runtime: the adapter starts turns
/// only with an open input gate, so a `runtimeGate` pause ends (reason
/// `turnStarted`). Cheap when there is none; never creates the store.
pub fn runtime_gate_turn_started_default(session: &str) -> bool {
    let Ok(store) = AskStore::open_default() else {
        return false;
    };
    if !store.inspect(|state| runtime_gate_closed(state, session)).unwrap_or(false) {
        return false;
    }
    match store.with_state(|state| runtime_gate_opened(state, session, None, "turnStarted", Utc::now())) {
        Ok(ended) => {
            if ended {
                crate::management::v2::asks::publish_pending_notices(&store);
            }
            ended
        }
        Err(error) => {
            eprintln!("asks: could not end the runtime gate pause of {session}: {error:#}");
            false
        }
    }
}

/// `cutex/asks/pause`: `{asksPaused, changed}`.
pub fn owner_pause_in_store(store: &AskStore, session: &str, now: DateTime<Utc>) -> anyhow::Result<Value> {
    store.with_state(|state| {
        super::waits::cancel_waits(state, session);
        let changed = owner_pause(state, session, now);
        json!({"asksPaused": pause_json(state.pauses.get(session)), "changed": changed})
    })
}

/// `cutex/asks/resume`: `{asksPaused: null, changed}`.
pub fn owner_resume_in_store(store: &AskStore, session: &str, now: DateTime<Utc>) -> anyhow::Result<Value> {
    store.with_state(|state| {
        let changed = owner_resume(state, session, "resumed", now);
        json!({"asksPaused": Value::Null, "changed": changed})
    })
}

/// The owner submitted a message to `session` (`disposition`: started,
/// steered or queued): a delegation pause ends ([`owner_message`]). Cheap
/// when there is none; never creates the store. Returns whether a pause was
/// cleared.
pub fn owner_message_default(session: &str, disposition: &str) -> bool {
    let Ok(store) = AskStore::open_default() else {
        return false;
    };
    match owner_message_in_store(&store, session, disposition) {
        Ok(cleared) => {
            if cleared {
                crate::management::v2::asks::publish_pending_notices(&store);
            }
            cleared
        }
        Err(error) => {
            eprintln!("asks: could not clear the delegation pause of {session}: {error:#}");
            false
        }
    }
}

fn owner_message_in_store(store: &AskStore, session: &str, disposition: &str) -> anyhow::Result<bool> {
    // Most submissions have no pause to release. Inspect only the relevant
    // pause, without cloning the full ask history under the shared lock.
    // A queued message also cannot release a stop hold, so avoid an exclusive
    // transaction (and its full-state work) for that known no-op.
    let needs_resume = store
        .inspect(|state| state.pauses.contains_key(session) && !(disposition == "queued" && stop_held(state, session)))?;
    if !needs_resume {
        return Ok(false);
    }
    // Recheck the current stop hold inside the mutation: another process may
    // have stopped the session since the read above.
    store.with_state(|state| owner_message(state, session, disposition, Utc::now()))
}

/// Whether the owner holds `session` (a delegation pause or stop hold; a
/// closed runtime gate is not the owner's hold). Never creates the store.
pub fn owner_paused_default(session: &str) -> bool {
    AskStore::open_default()
        .ok()
        .and_then(|store| store.inspect(|state| {
            state.pauses.get(session).is_some_and(|pause| pause.origin.as_deref() != Some(ORIGIN_RUNTIME_GATE))
        }).ok())
        .unwrap_or(false)
}

/// `asksPaused` of a session for queue/list and bootstrap (never creates
/// the store; `null` when there is none).
pub fn pause_state_default(session: &str) -> Value {
    AskStore::open_default()
        .ok()
        .and_then(|store| store.inspect(|state| pause_json(state.pauses.get(session))).ok())
        .unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::store::tests::{new_ask, temp_store, TARGET};

    #[test]
    fn owner_message_probe_leaves_absent_store_absent() {
        let (store, dir) = temp_store("owner-message-absent");
        assert!(!owner_message_in_store(&store, TARGET, "started").unwrap());
        assert!(!dir.exists(), "ordinary owner input must not create ask state or a lock");
    }

    #[test]
    fn queued_owner_message_preserves_stop_hold_without_mutating_store() {
        let (store, dir) = temp_store("owner-message-held");
        store.with_state(|state| owner_stop(state, TARGET, Utc::now())).unwrap();
        let database = dir.join(crate::management::v2::hot_store::DATABASE_FILE);
        let before = crate::management::v2::hot_store::generation(&database, "asks").unwrap();
        assert!(!owner_message_in_store(&store, TARGET, "queued").unwrap());
        assert_eq!(crate::management::v2::hot_store::generation(&database, "asks").unwrap(), before);
        let state = store.snapshot().unwrap();
        assert!(stop_held(&state, TARGET));
        assert!(state.commands.is_empty());
        // A started message still releases the hold and queues the native gate.
        assert!(owner_message_in_store(&store, TARGET, "started").unwrap());
        let state = store.snapshot().unwrap();
        assert!(!stop_held(&state, TARGET));
        assert!(state.commands.iter().any(|queued| queued.command ==
            RuntimeCommand::ResumeGate { cutex_session_id: TARGET.into() }));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// An explicit owner pause holds queued asks (also ones created later);
    /// resume clears it and reopens the native gate; both publish
    /// `cutex/asks/pauseChanged` once.
    #[test]
    fn owner_pause_holds_asks_until_resumed() {
        let mut state = AskState::default();
        let now = Utc::now();
        assert!(owner_pause(&mut state, TARGET, now));
        assert!(!owner_pause(&mut state, TARGET, now));
        assert_eq!(state.pauses[TARGET].origin.as_deref(), Some(ORIGIN_OWNER_PAUSE));
        let (ask, _) = state.create_ask(new_ask("a", "x"), now).unwrap();
        owner_pause(&mut state, TARGET, now);
        assert_eq!(state.asks[&ask.ask_id].hold, Some(Hold::Paused { by: PausedBy::Owner }));
        assert!(owner_resume(&mut state, TARGET, "resumed", now));
        assert_eq!(state.asks[&ask.ask_id].hold, None);
        assert_eq!(
            state.commands[0].command,
            RuntimeCommand::ResumeGate { cutex_session_id: TARGET.into() }
        );
        assert!(!owner_resume(&mut state, TARGET, "resumed", now));
        assert_eq!(state.commands.len(), 1, "deduplicated");
        let changes: Vec<&Value> = state
            .notices
            .iter()
            .filter(|notice| notice.method == PAUSE_CHANGED_EVENT)
            .map(|notice| &notice.params)
            .collect();
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0]["asksPaused"]["origin"], json!(ORIGIN_OWNER_PAUSE));
        assert_eq!(changes[0]["reason"], json!("paused"));
        assert_eq!(changes[1]["asksPaused"], Value::Null);
    }

    /// Owner decision 2026-10-12 ("停止 = everything waits"): a stop holds
    /// asks and every ride-along, leaves the native gate closed, survives
    /// a message the owner only queues, and is released by queue resume
    /// (which reopens the gate) or by an owner message that starts a turn.
    #[test]
    fn owner_stop_holds_everything_until_resume_or_owner_message() {
        let mut state = AskState::default();
        let now = Utc::now();
        let (ask, _) = state.create_ask(new_ask("a", "x"), now).unwrap();
        owner_stop(&mut state, TARGET, now);
        let pause = &state.pauses[TARGET];
        assert_eq!(pause.origin.as_deref(), Some(ORIGIN_OWNER_STOP));
        assert!(pause.stop_hold && stop_held(&state, TARGET));
        assert_eq!(state.asks[&ask.ask_id].hold, Some(Hold::Paused { by: PausedBy::Owner }));
        assert!(state.commands.is_empty(), "the gate the interrupt closed stays closed");
        assert_eq!(pause_json(state.pauses.get(TARGET))["stopHold"], json!(true));
        // A second stop changes nothing; a queued owner message releases nothing.
        owner_stop(&mut state, TARGET, now);
        assert!(!owner_message(&mut state, TARGET, "queued", now));
        assert!(stop_held(&state, TARGET));
        // Queue resume releases it and reopens the gate.
        assert!(release_stop_hold(&mut state, TARGET, "resumed", now));
        assert!(state.pauses.is_empty());
        assert_eq!(state.asks[&ask.ask_id].hold, None);
        assert_eq!(state.commands[0].command, RuntimeCommand::ResumeGate { cutex_session_id: TARGET.into() });
        assert!(!release_stop_hold(&mut state, TARGET, "resumed", now));
        // The owner's next message that starts (or steers) a turn releases it too.
        for disposition in ["started", "steered"] {
            owner_stop(&mut state, TARGET, now);
            assert!(owner_message(&mut state, TARGET, disposition, now));
            assert!(!stop_held(&state, TARGET));
        }
        let reasons: Vec<&Value> = state.notices.iter()
            .filter(|notice| notice.method == PAUSE_CHANGED_EVENT)
            .map(|notice| &notice.params["reason"]).collect();
        assert_eq!(reasons, [&json!("ownerStop"), &json!("resumed"), &json!("ownerStop"),
            &json!("ownerMessage"), &json!("ownerStop"), &json!("ownerMessage")]);
        for notice in state.notices.iter().filter(|notice| notice.method == PAUSE_CHANGED_EVENT) {
            crate::management::v2::contract_validation::validate_cutex_event_message(
                &json!({"method": notice.method, "params": notice.params}),
            ).expect("pauseChanged matches the schema");
        }
    }

    /// A closed runtime gate pause: created once (never over an owner
    /// pause), released by queue resume (gate reopened), superseded by a
    /// stop, ended by an observed turn start without a gate resume.
    #[test]
    fn runtime_gate_pause_lifecycle() {
        let mut state = AskState::default();
        let now = Utc::now();
        let (ask, _) = state.create_ask(new_ask("a", "x"), now).unwrap();
        assert!(runtime_gate_pause(&mut state, TARGET, 4, now));
        assert!(!runtime_gate_pause(&mut state, TARGET, 4, now));
        assert!(runtime_gate_closed(&state, TARGET));
        assert_eq!(state.asks[&ask.ask_id].hold, Some(Hold::HeldByRuntime { native: Some(NATIVE_GATE_CLOSED.into()) }));
        assert_eq!(pause_json(state.pauses.get(TARGET))["origin"], json!(ORIGIN_RUNTIME_GATE));
        assert_eq!(pause_json(state.pauses.get(TARGET))["stopHold"], json!(false));
        // The same generation keeps it; queue resume releases it and reopens the gate.
        assert!(!runtime_gate_opened(&mut state, TARGET, Some(4), "runtimeRestart", now));
        assert!(release_stop_hold(&mut state, TARGET, "resumed", now));
        assert!(state.pauses.is_empty());
        assert_eq!(state.asks[&ask.ask_id].hold, None);
        assert_eq!(state.commands.len(), 1);
        // A stop takes it over (owner hold, gate stays closed).
        runtime_gate_pause(&mut state, TARGET, 4, now);
        owner_stop(&mut state, TARGET, now);
        assert!(!runtime_gate_closed(&state, TARGET) && stop_held(&state, TARGET));
        assert_eq!(state.asks[&ask.ask_id].hold, Some(Hold::Paused { by: PausedBy::Owner }));
        assert!(!runtime_gate_opened(&mut state, TARGET, None, "turnStarted", now), "never ends an owner hold");
        owner_resume(&mut state, TARGET, "resumed", now);
        // An owner pause is never replaced by a gate pause.
        owner_pause(&mut state, TARGET, now);
        assert!(!runtime_gate_pause(&mut state, TARGET, 4, now));
        owner_resume(&mut state, TARGET, "resumed", now);
        // A turn start ends it without a gate command.
        state.commands.clear();
        runtime_gate_pause(&mut state, TARGET, 4, now);
        assert!(runtime_gate_opened(&mut state, TARGET, None, "turnStarted", now));
        assert!(state.pauses.is_empty() && state.commands.is_empty());
        assert_eq!(state.asks[&ask.ask_id].hold, None);
        for notice in state.notices.iter().filter(|notice| notice.method == PAUSE_CHANGED_EVENT) {
            crate::management::v2::contract_validation::validate_cutex_event_message(
                &json!({"method": notice.method, "params": notice.params}),
            ).expect("pauseChanged matches the schema");
        }
    }

    /// A stop on an explicit delegation pause adds the stop hold; queue
    /// resume ends only the stop hold, the explicit pause stays.
    #[test]
    fn owner_stop_on_an_explicit_pause_keeps_it_after_queue_resume() {
        let mut state = AskState::default();
        let now = Utc::now();
        owner_pause(&mut state, TARGET, now);
        assert!(!stop_held(&state, TARGET));
        owner_stop(&mut state, TARGET, now);
        assert_eq!(state.pauses[TARGET].origin.as_deref(), Some(ORIGIN_OWNER_PAUSE));
        assert!(stop_held(&state, TARGET));
        assert!(release_stop_hold(&mut state, TARGET, "resumed", now));
        assert!(!stop_held(&state, TARGET));
        assert!(state.pauses.contains_key(TARGET));
        assert_eq!(state.commands.len(), 1);
        // A legacy pause (no origin) reads as `legacy`.
        state.pauses.insert(
            TARGET.into(),
            AdmissionPause { cutex_session_id: TARGET.into(), by: PausedBy::Owner, since: "t".into(), origin: None, stop_hold: false, runtime_generation: None },
        );
        assert_eq!(pause_json(state.pauses.get(TARGET))["origin"], json!("legacy"));
    }
}

/// Design 044 routing inside the ask store: a handover (pinned per test
/// thread with `model::route::pin`) moves control, pending items, questions
/// and waits to the new controller without changing the ask's request.
#[cfg(test)]
mod handover_tests {
    use super::*;
    use crate::asks::delivery::{self, claim_for_wait, coalesce_into_event, is_for, recipient, record_answer_delivery, record_question_delivery, release_event, requesters_with_pending, settle_for_ask};
    use crate::asks::model::route::{pin, Pin};
    use crate::asks::store::tests::{new_ask, REQUESTER, TARGET};
    use crate::asks::store::{AskState, WaitRecord};

    const NEW: &str = "cutex.00000000-0000-0000-0000-0000000000c1";
    const THIRD: &str = "cutex.00000000-0000-0000-0000-0000000000d1";

    fn handed(ask: &str, to: &str) {
        pin(ask, Pin { controller: Ok(Some(to.into())), history: vec![REQUESTER.into()], questions: None });
    }

    fn with_attempt(state: &mut AskState, ask_id: &str, now: DateTime<Utc>) {
        state.update_ask(ask_id, now, |ask| ask.attempts.push(AttemptRecord::submitting(1, "m".into(), 3, "t".into())));
    }

    fn question(state: &mut AskState, ask_id: &str, now: DateTime<Utc>) -> String {
        let id = format!("q-{ask_id}");
        state.questions.insert(id.clone(), QuestionRecord {
            question_id: id.clone(),
            ask_id: ask_id.into(),
            attempt: 1,
            runtime_generation: 3,
            native: NativeQuestionRef { thread_id: "th".into(), turn_id: "t".into(), item_id: "i".into(), request_id: json!(7) },
            questions: json!([{"id": "q0", "question": "Which DB?"}]),
            route: QuestionRoute::Requester,
            routed_to_owner_at: None,
            state: QuestionState::Open,
            resolution: None,
            created_at: now_rfc3339(now),
        });
        let ask = state.asks[ask_id].clone();
        record_question_delivery(state, &ask, &id, now);
        id
    }

    #[test]
    fn pending_items_reroute_and_in_flight_items_stay_without_copies() {
        let mut state = AskState::default();
        let now = Utc::now();
        let (ask, _) = state.create_ask(new_ask("a", "x"), now).unwrap();
        let before = state.asks[&ask.ask_id].clone();
        // d1 claimed by the old requester's wait, d2 carried by an event to it, d3 pending.
        let d1 = record_answer_delivery(&mut state, &ask, "ans-1", now).unwrap();
        assert_eq!(claim_for_wait(&mut state, REQUESTER, &[], "w1", now).len(), 1);
        let d2 = record_answer_delivery(&mut state, &ask, "ans-2", now).unwrap();
        let event = coalesce_into_event(&mut state, REQUESTER, "msg-1", now).unwrap();
        let current = state.asks[&ask.ask_id].clone();
        let d3 = record_answer_delivery(&mut state, &current, "ans-3", now).unwrap();
        handed(&ask.ask_id, NEW);
        assert_eq!(recipient(&state, &state.deliveries[&d1]).as_deref(), Some(REQUESTER));
        assert_eq!(recipient(&state, &state.deliveries[&d2]).as_deref(), Some(REQUESTER));
        assert_eq!(recipient(&state, &state.deliveries[&d3]).as_deref(), Some(NEW));
        assert_eq!(requesters_with_pending(&state), vec![NEW.to_string()], "the pending item goes to the new controller only");

        let applied = apply_handover(&mut state, &ask.ask_id, Some(REQUESTER), NEW, 1, now).unwrap();
        assert_eq!(applied.rerouted, vec![d3.clone()]);
        let mut in_flight = vec![d1.clone(), d2.clone()];
        in_flight.sort();
        assert_eq!(applied.in_flight, in_flight);
        let summary = applied.summary_delivery.clone().expect("one handover summary");
        assert!(is_for(&state, &state.deliveries[&summary], NEW));
        assert!(!state.deliveries[&summary].passive, "a rerouted result needs the new controller");
        assert!(state.notices.iter().any(|n| n.method == HANDOVER_EVENT && n.sessions.contains(&NEW.to_string())));
        // Idempotent per controller revision; the request is never rewritten.
        let again = apply_handover(&mut state, &ask.ask_id, Some(REQUESTER), NEW, 1, now).unwrap();
        assert!(again.summary_delivery.is_none());
        let after = &state.asks[&ask.ask_id];
        assert_eq!((&after.requester, &after.action_id, &after.request_sha256), (&before.requester, &before.action_id, &before.request_sha256));
        assert!(state.deliveries.values().all(|d| d.requester_cutex_session_id == REQUESTER), "items keep the original requester");
        // The new controller's event takes the pending item and the summary, never d1 / d2.
        let to_new = coalesce_into_event(&mut state, NEW, "msg-2", now).unwrap();
        let mut carried = state.events[&to_new].delivery_ids.clone();
        carried.sort();
        let mut want = vec![d3.clone(), summary.clone()];
        want.sort();
        assert_eq!(carried, want);
        assert_eq!(state.deliveries[&d3].routed_to.as_deref(), Some(NEW));
        // A refused event to the old requester puts d2 back as pending: it is
        // resolved anew (to the controller), never sent twice to the old one.
        release_event(&mut state, &event, now);
        assert_eq!(state.deliveries[&d2].routed_to, None);
        assert_eq!(recipient(&state, &state.deliveries[&d2]).as_deref(), Some(NEW));
        // An event whose outcome is unknown after a handover is never resent
        // to the new controller: settled handoverUnknown, still readable.
        let current = state.asks[&ask.ask_id].clone();
        let d4 = record_answer_delivery(&mut state, &current, "ans-4", now).unwrap();
        pin(&ask.ask_id, Pin { controller: Ok(None), history: vec![], questions: None });
        let lost = coalesce_into_event(&mut state, REQUESTER, "msg-3", now).unwrap();
        handed(&ask.ask_id, NEW);
        assert!(state.events[&lost].delivery_ids.contains(&d4));
        assert!(delivery::release_event_unknown(&mut state, &lost, now).contains(&d4));
        assert_eq!(state.deliveries[&d4].state, DeliveryState::Acknowledged);
        assert_eq!(state.deliveries[&d4].settled_by.as_deref(), Some(delivery::HANDOVER_UNKNOWN));
        // A handover of a remote / unrecorded controller is refused.
        let (other, _) = state.create_ask(new_ask("b", "y"), now).unwrap();
        assert_eq!(apply_handover(&mut state, &other.ask_id, Some(REQUESTER), NEW, 1, now).unwrap_err().code, "handover_not_recorded");
    }

    #[test]
    fn the_original_requester_reads_but_never_controls_or_settles() {
        let mut state = AskState::default();
        let now = Utc::now();
        let (ask, _) = state.create_ask(new_ask("a", "x"), now).unwrap();
        let answer = record_answer_delivery(&mut state, &ask, "ans-1", now).unwrap();
        state.update_ask(&ask.ask_id, now, |ask| {
            ask.execution = ExecutionState::Answered;
            ask.review = Some(ReviewState::AwaitingAcceptance);
        });
        handed(&ask.ask_id, NEW);
        let ask = state.asks[&ask.ask_id].clone();
        assert!(ask.readable_by(REQUESTER) && ask.readable_by(NEW) && !ask.readable_by(THIRD));
        assert!(!ask.controlled_by(REQUESTER).unwrap() && ask.controlled_by(NEW).unwrap());
        assert_eq!(settle_for_ask(&mut state, REQUESTER, &ask.ask_id, None, "read", now), 0, "the old requester's read settles nothing of the new controller");
        assert_eq!(state.deliveries[&answer].state, DeliveryState::Pending);
        // Every control path asks the controller, with the r253 "not yours" error.
        let denied = |code: &str| assert_eq!(code, "ask_not_found");
        denied(&crate::asks::revisions::check(&ask, REQUESTER, None).unwrap_err().code);
        assert!(crate::asks::revisions::check(&ask, NEW, None).is_ok());
        denied(&crate::asks::awaits::reopen(&mut state, &ask.ask_id, Some(REQUESTER), Some("redo"), now).unwrap_err().code);
        denied(&crate::asks::awaits::resume_now(&mut state, &ask.ask_id, Some(REQUESTER), None, now).unwrap_err().code);
        denied(&crate::asks::lifecycle::retry(&mut state, &ask.ask_id, Some(REQUESTER), now).unwrap_err().code);
        // The new controller's reopen settles its own pending item.
        crate::asks::awaits::reopen(&mut state, &ask.ask_id, Some(NEW), Some("redo"), now).unwrap();
        assert_eq!(state.deliveries[&answer].state, DeliveryState::Acknowledged);
        // A third handover: B loses control at once, C gains it.
        handed(&ask.ask_id, THIRD);
        let ask = state.asks[&ask.ask_id].clone();
        denied(&crate::asks::revisions::check(&ask, NEW, None).unwrap_err().code);
        assert!(crate::asks::revisions::check(&ask, THIRD, None).is_ok());
        // Notices reach the controller too (progress, questions).
        assert!(AskState::ask_sessions(&ask).contains(&THIRD.to_string()));
    }

    /// Review #4: a session-owned ask whose questions go to a project
    /// Director that is neither its requester nor its controller. That
    /// Director gets the question (notice, item, answer right) and nothing
    /// of the delegation: no created event, brief, artifact, read or control.
    #[test]
    fn a_question_only_director_gets_the_question_but_never_the_brief() {
        use crate::asks::questions::{resolve, Responder};
        let (store, dir) = crate::asks::store::tests::temp_store("question-only");
        let now = Utc::now();
        let ask_id = "ask-0000000000000000000000000000q044".to_string();
        pin(&ask_id, Pin { controller: Ok(None), history: vec![], questions: Some(THIRD.into()) });
        let mut request = new_ask("q-only", "private delegation brief");
        request.ask_id = Some(ask_id.clone());
        let created = store
            .with_state(|state| {
                state.create_ask(request, now).unwrap();
                state.notices.iter().rev().find(|notice| notice.method == "cutex/ask/created").unwrap().sessions.clone()
            })
            .unwrap();
        assert!(created.contains(&REQUESTER.to_string()) && created.contains(&TARGET.to_string()));
        assert!(!created.contains(&THIRD.to_string()), "no created event (full brief) for the question-only Director");
        let ask = store.snapshot().unwrap().asks[&ask_id].clone();
        assert!(!AskState::ask_sessions(&ask).contains(&THIRD.to_string()));
        assert!(!ask.readable_by(THIRD) && !ask.controlled_by(THIRD).unwrap());
        // The handler's final message: readers are the whole-ask audience only.
        let artifact = store.put_artifact(b"handler answer", "text/markdown", &AskState::ask_sessions(&ask), Some(&ask_id), now).unwrap();
        assert!(store.read_artifact(&artifact, Some(THIRD)).unwrap().is_none(), "no artifact for the question-only Director");
        assert!(store.read_artifact(&artifact, Some(REQUESTER)).unwrap().is_some());
        // The question reaches it: notice audience, pending item, answer right.
        assert!(AskState::question_sessions(&ask).contains(&THIRD.to_string()));
        let answers = json!({"q0": {"answers": ["sqlite"]}});
        let resolved = store
            .with_state(|state| {
                with_attempt(state, &ask_id, now);
                let q = question(state, &ask_id, now);
                assert!(requesters_with_pending(state).contains(&THIRD.to_string()), "the question item goes to the Director");
                assert!(crate::asks::questions::may_answer(&state.asks[&ask_id], THIRD).unwrap());
                resolve(state, &q, &Responder::Agent(THIRD.into()), &answers, Some(3), now).unwrap();
                state.notices.iter().rev().find(|notice| notice.method == "cutex/ask/question").unwrap().sessions.clone()
            })
            .unwrap();
        assert!(resolved.contains(&THIRD.to_string()) && resolved.contains(&REQUESTER.to_string()));
        // Still no whole-delegation read or control afterwards.
        let ask = store.snapshot().unwrap().asks[&ask_id].clone();
        assert!(!ask.readable_by(THIRD) && !ask.controlled_by(THIRD).unwrap());
        assert_eq!(crate::asks::revisions::check(&ask, THIRD, None).unwrap_err().code, "ask_not_found");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_unknown_handover_fails_closed() {
        let mut state = AskState::default();
        let now = Utc::now();
        let (ask, _) = state.create_ask(new_ask("a", "x"), now).unwrap();
        let answer = record_answer_delivery(&mut state, &ask, "ans-1", now).unwrap();
        pin(&ask.ask_id, Pin { controller: Err("handover_pending_reconcile"), history: vec![], questions: None });
        let ask = state.asks[&ask.ask_id].clone();
        assert_eq!(recipient(&state, &state.deliveries[&answer]), None, "held, not guessed");
        assert!(requesters_with_pending(&state).is_empty());
        assert!(coalesce_into_event(&mut state, REQUESTER, "m", now).is_none());
        assert!(claim_for_wait(&mut state, REQUESTER, &[], "w", now).is_empty());
        assert_eq!(crate::asks::revisions::check(&ask, REQUESTER, None).unwrap_err().code, "handover_pending_reconcile");
        assert_eq!(crate::asks::view::summary(&state, &ask)["controlError"], "handover_pending_reconcile");
        assert!(ask.readable_by(REQUESTER), "history stays readable");
    }

    #[test]
    fn waits_end_with_handed_over_and_the_new_controller_waits_itself() {
        let mut state = AskState::default();
        let now = Utc::now();
        let (ask, _) = state.create_ask(new_ask("a", "x"), now).unwrap();
        let wait = WaitRecord {
            wait_id: "w".into(),
            cutex_session_id: REQUESTER.into(),
            action_id: "act".into(),
            asks: vec![ask.ask_id.clone()],
            all: false,
            started_at: now_rfc3339(now),
            deadline: now_rfc3339(now + chrono::Duration::minutes(5)),
            cancel_epoch: 0,
        };
        state.waits.insert("w".into(), wait.clone());
        assert_eq!(crate::asks::waits::wake_reason(&state, REQUESTER, &wait, now), None);
        handed(&ask.ask_id, NEW);
        let applied = apply_handover(&mut state, &ask.ask_id, Some(REQUESTER), NEW, 2, now).unwrap();
        assert_eq!(applied.waits_ended, vec!["w".to_string()]);
        assert!(state.deliveries[&applied.summary_delivery.unwrap()].passive, "nothing to decide yet: a passive summary");
        assert_eq!(crate::asks::waits::wake_reason(&state, REQUESTER, &wait, now), Some("handedOver"));
        assert_eq!(crate::asks::waits::awaited_asks(&state, REQUESTER, &[ask.ask_id.clone()]).unwrap_err().code, "ask_not_found");
        assert!(crate::asks::waits::awaited_asks(&state, REQUESTER, &[]).unwrap().is_empty());
        assert_eq!(crate::asks::waits::awaited_asks(&state, NEW, &[]).unwrap(), vec![ask.ask_id.clone()]);
        // A new result reaches the new controller's wait, not the old one's.
        let current = state.asks[&ask.ask_id].clone();
        record_answer_delivery(&mut state, &current, "ans-1", now).unwrap();
        assert!(claim_for_wait(&mut state, REQUESTER, &[ask.ask_id.clone()], "act", now).is_empty());
        assert_eq!(claim_for_wait(&mut state, NEW, &[ask.ask_id.clone()], "act2", now).len(), 2, "the result and the summary");
    }

    #[test]
    fn questions_keep_their_id_and_only_the_current_responder_answers() {
        use crate::asks::questions::{resolve, Responder};
        let mut state = AskState::default();
        let now = Utc::now();
        let (ask, _) = state.create_ask(new_ask("a", "x"), now).unwrap();
        with_attempt(&mut state, &ask.ask_id, now);
        let q = question(&mut state, &ask.ask_id, now);
        let answers = json!({"q0": {"answers": ["sqlite"]}});
        handed(&ask.ask_id, NEW);
        let applied = apply_handover(&mut state, &ask.ask_id, Some(REQUESTER), NEW, 1, now).unwrap();
        assert_eq!(applied.open_questions, vec![q.clone()]);
        assert_eq!(state.deliveries.values().filter(|d| d.kind == DeliveryKind::Question).count(), 1, "no second question item");
        // The earlier controller's late answer is refused like a stranger's.
        let late = resolve(&mut state, &q, &Responder::Agent(REQUESTER.into()), &answers, Some(3), now).unwrap_err();
        assert_eq!(late.code, "question_not_found");
        let won = resolve(&mut state, &q, &Responder::Agent(NEW.into()), &answers, Some(3), now).unwrap();
        assert_eq!(won.resolution.unwrap().by, NEW, "the real actor is recorded");
        assert_eq!(resolve(&mut state, &q, &Responder::Owner, &answers, Some(3), now).unwrap_err().code, "already_resolved");
        // A question routed to the Director (questions: director) while the
        // requester keeps control: both may answer, nobody else.
        let (other, _) = state.create_ask(new_ask("b", "y"), now).unwrap();
        with_attempt(&mut state, &other.ask_id, now);
        let q2 = question(&mut state, &other.ask_id, now);
        pin(&other.ask_id, Pin { controller: Ok(None), history: vec![], questions: Some(THIRD.into()) });
        let item = state.deliveries.values().find(|d| d.ref_id == q2).unwrap().clone();
        assert_eq!(recipient(&state, &item).as_deref(), Some(THIRD));
        assert_eq!(resolve(&mut state, &q2, &Responder::Agent(NEW.into()), &answers, Some(3), now).unwrap_err().code, "question_not_found");
        resolve(&mut state, &q2, &Responder::Agent(THIRD.into()), &answers, Some(3), now).unwrap();
        // While the ask's control awaits review, the designated question
        // route still answers alone; it gets no read or control of the ask.
        let (fourth, _) = state.create_ask(new_ask("d", "w"), now).unwrap();
        with_attempt(&mut state, &fourth.ask_id, now);
        let q4 = question(&mut state, &fourth.ask_id, now);
        pin(&fourth.ask_id, Pin { controller: Err("handover_pending_review"), history: vec![], questions: Some(THIRD.into()) });
        let fourth = state.asks[&fourth.ask_id].clone();
        assert!(!fourth.readable_by(THIRD) && fourth.controlled_by(THIRD).is_err());
        assert_eq!(resolve(&mut state, &q4, &Responder::Agent(REQUESTER.into()), &answers, Some(3), now).unwrap_err().code, "handover_pending_review");
        resolve(&mut state, &q4, &Responder::Agent(THIRD.into()), &answers, Some(3), now).unwrap();
        // Fall-through follows the current recipient's presence.
        let (third, _) = state.create_ask(new_ask("c", "z"), now).unwrap();
        with_attempt(&mut state, &third.ask_id, now);
        let q3 = question(&mut state, &third.ask_id, now);
        handed(&third.ask_id, NEW);
        let offline = |session: &str| session == NEW;
        assert_eq!(crate::asks::questions::fall_through(&mut state, &offline, now), vec![q3]);
        let _ = TARGET;
    }

    #[test]
    fn views_show_the_controller_only_after_a_handover_and_records_stay_r253_shaped() {
        let mut state = AskState::default();
        let now = Utc::now();
        let (ask, _) = state.create_ask(new_ask("a", "x"), now).unwrap();
        let plain = crate::asks::view::summary(&state, &ask);
        assert!(plain.get("controller").is_none() && plain.get("originallyRequestedBy").is_none());
        let id = record_answer_delivery(&mut state, &ask, "ans-1", now).unwrap();
        let stored = serde_json::to_value(&state.deliveries[&id]).unwrap();
        assert!(stored.get("routedTo").is_none(), "no new field without a handover");
        // An r253 record (no routedTo) still reads.
        let mut r253 = stored.clone();
        r253.as_object_mut().unwrap().remove("routedTo");
        let back: DeliveryRecord = serde_json::from_value(r253).unwrap();
        assert_eq!(back.routed_to, None);
        handed(&ask.ask_id, NEW);
        let summary = crate::asks::view::summary(&state, &ask);
        assert_eq!((summary["controller"].as_str(), summary["originallyRequestedBy"].as_str()), (Some(NEW), Some(REQUESTER)));
        assert_eq!(summary["requester"]["cutexSessionId"], REQUESTER, "the requester stays the original one");
        let view = crate::asks::view::requester_view(&state, &ask);
        assert_eq!(view["originally_requested_by"], REQUESTER);
        assert_eq!(crate::asks::view::owner_view(&state, &ask)["controller"], NEW);
        // Claimed by the new controller: routedTo is written (additive).
        claim_for_wait(&mut state, NEW, &[], "w", now);
        let claimed = serde_json::to_value(&state.deliveries[&id]).unwrap();
        assert_eq!(claimed["routedTo"], NEW);
        assert_eq!(claimed["requesterCutexSessionId"], REQUESTER);
        let _ = delivery::HANDED_OVER;
    }
}
