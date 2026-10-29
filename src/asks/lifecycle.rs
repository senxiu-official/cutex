//! Fork, revert, profile switch and retry rules (asks design §5.10, §5.11).
//!
//! - **Fork / revert** of an agent are refused while it has unfinished
//!   external-input obligations from asks (an admitted or running attempt,
//!   or the admission lease). cutex checks first and names the blocking
//!   asks; the owner can stop them (`cutex/ask/stop`) and retry.
//! - **Revert** of a turn that answered an ask flags it `targetReverted`
//!   and tells the requester; the answer text stays.
//! - **Profile switch `afterTurn`** is an admission barrier (the scheduler's
//!   `profileSwitchPending` hold); target profile/model are snapshotted at
//!   attempt start.
//! - **Retry = new attempt** with a fresh gate revalidation, never the
//!   native `thread/externalInput/retry`.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use super::model::*;
use super::store::{AskError, AskState, AskStore};

/// Asks whose attempt is an unfinished native obligation on `session`.
pub fn blocking_asks(state: &AskState, session: &str) -> Vec<Value> {
    let leased = state.leases.get(session).and_then(|lease| match &lease.holder {
        LeaseHolder::Ask { ask_id, .. } => Some(ask_id.clone()),
        _ => None,
    });
    let mut blocking: Vec<Value> = state
        .asks
        .values()
        .filter(|ask| (ask.remote.is_none() && ask.target_cutex_session_id == session))
        // A delegation waiting for jobs (or woken, not admitted) has no turn
        // in the runtime: it does not block fork / revert.
        .filter(|ask| (ask.execution.in_runtime() && !ask.is_waiting() && !ask.resume_pending()) || leased.as_deref() == Some(ask.ask_id.as_str()))
        .map(|ask| json!({"askId": ask.ask_id, "title": ask.title, "requester": ask.requester, "execution": ask.execution}))
        .collect();
    if blocking.is_empty() {
        if let Some(lease) = state.leases.get(session) {
            if let LeaseHolder::Event { event_id } = &lease.holder {
                blocking.push(json!({"eventId": event_id, "kind": "askResults"}));
            }
        }
    }
    blocking
}

/// Refusal for a fork / revert while asks are in the runtime.
pub fn check_no_obligations(store: &AskStore, session: &str, operation: &str) -> Result<(), AskError> {
    let Some(state) = store.peek().map_err(|error| AskError::new("persistence_unavailable", format!("{error:#}")))? else {
        return Ok(());
    };
    let blocking = blocking_asks(&state, session);
    if blocking.is_empty() {
        return Ok(());
    }
    Err(AskError::new(
        "ask_obligations_pending",
        format!("{operation} is refused while asks (委托) are running on this agent; stop them first (cutex/ask/stop) or wait for them to end"),
    )
    .with(json!({"blockingAsks": blocking})))
}

/// Default-store wrapper for Management v2.
pub fn check_no_obligations_default(session: &str, operation: &str) -> Result<(), AskError> {
    match AskStore::open_default() {
        Ok(store) => check_no_obligations(&store, session, operation),
        Err(_) => Ok(()),
    }
}

/// After a revert removed `turn_ids` of `session`: flag asks answered in
/// them. Returns the flagged ask ids.
pub fn mark_reverted(state: &mut AskState, session: &str, turn_ids: &[String], now: DateTime<Utc>) -> Vec<String> {
    let flagged: Vec<String> = state
        .asks
        .values()
        .filter(|ask| (ask.remote.is_none() && ask.target_cutex_session_id == session) && !ask.target_reverted)
        .filter(|ask| {
            ask.attempts
                .iter()
                .any(|attempt| attempt.turn_id.as_ref().is_some_and(|turn| turn_ids.contains(turn)))
        })
        .map(|ask| ask.ask_id.clone())
        .collect();
    for ask_id in &flagged {
        state.update_ask(ask_id, now, |ask| ask.target_reverted = true);
        if let Some(ask) = state.asks.get(ask_id).cloned() {
            super::delivery::record_state_delivery(state, &ask, "targetReverted", now);
            state.notice(
                "cutex/ask/stateChanged",
                AskState::ask_sessions(&ask),
                json!({"askId": ask_id, "targetReverted": true, "revision": ask.revision}),
            );
        }
    }
    flagged
}

pub fn mark_reverted_default(session: &str, turn_ids: &[String]) {
    if turn_ids.is_empty() {
        return;
    }
    let Ok(store) = AskStore::open_default() else {
        return;
    };
    if let Ok(Some(_)) = store.peek() {
        if let Err(error) = mark_reverted_in_store(&store,session,turn_ids,Utc::now()) {
            eprintln!("asks: could not flag reverted asks of {session}: {error:#}");
        }
    }
}

/// Explicit user revert can name a historical, already cold attempt.
/// Inventory both locations once, then promote+mark under the same write lock.
pub(crate) fn mark_reverted_in_store(store:&AskStore,session:&str,turn_ids:&[String],now:DateTime<Utc>)->anyhow::Result<Vec<String>> {
    let historical=store.history_snapshot()?;
    let ids:Vec<&str>=historical.asks.values().filter(|ask|ask.remote.is_none()&&ask.target_cutex_session_id==session&&!ask.target_reverted)
        .filter(|ask|ask.attempts.iter().any(|attempt|attempt.turn_id.as_ref().is_some_and(|turn|turn_ids.contains(turn))))
        .map(|ask|ask.ask_id.as_str()).collect();
    store.with_records(&ids,|state|mark_reverted(state,session,turn_ids,now))
}

/// `session` was retired (archived): settle its open asks as **target**.
///
/// - Waiting asks (queued, pending review), delegations waiting for jobs and
///   woken continuations fail `recipient_archived` (what the scheduler
///   does for a queued ask to an archived target);
/// - admitted / running attempts end `interrupted(stopped)` through the
///   end-of-turn path (the stop rule of [`super::scheduler`]); the ask
///   closes `stopped`, a review `abandoned` is kept, the lease is freed.
///
/// Asks `session` requested are left alone (their targets still answer;
/// events to an archived requester are dropped by delivery as before).
/// Returns the settled ask ids. Takes only the asks store lock.
pub fn settle_retired_target(store: &AskStore, session: &str, now: DateTime<Utc>) -> anyhow::Result<Vec<String>> {
    if store.peek()?.is_none() {
        return Ok(Vec::new());
    }
    let (failed, running) = store.with_state(|state| {
        let leased = state.leases.get(session).and_then(|lease| match &lease.holder {
            LeaseHolder::Ask { ask_id, attempt } => Some((ask_id.clone(), *attempt)),
            _ => None,
        });
        let mut failed = Vec::new();
        let mut running: Vec<(String, u32)> = Vec::new();
        for ask in state.asks.values().filter(|ask| (ask.remote.is_none() && ask.target_cutex_session_id == session) && ask.is_open()) {
            match ask.execution {
                ExecutionState::Queued | ExecutionState::PendingReview => failed.push(ask.ask_id.clone()),
                ExecutionState::Running if ask.is_waiting() || ask.resume_pending() => failed.push(ask.ask_id.clone()),
                ExecutionState::Admitted | ExecutionState::Running => {
                    if let Some(attempt) = ask.current_attempt().filter(|attempt| !attempt.state.is_terminal()) {
                        running.push((ask.ask_id.clone(), attempt.n));
                    }
                }
                _ => {}
            }
        }
        if let Some((ask_id, attempt)) = leased {
            if !running.iter().any(|(id, _)| *id == ask_id) && !failed.contains(&ask_id) {
                running.push((ask_id, attempt));
            }
        }
        for ask_id in &failed {
            super::scheduler::fail_ask(state, ask_id, "recipient_archived", now);
        }
        (failed, running)
    })?;
    let mut settled = failed;
    for (ask_id, attempt) in running {
        if super::scheduler::settle_stopped_attempt(store, session, &ask_id, attempt, now)? {
            settled.push(ask_id);
        }
    }
    Ok(settled)
}

/// Default-store hook for the retirement paths (agent archive, session
/// retire). Best effort: a failure is logged, never fails the retirement
/// (the scheduler settles the leased attempts of an archived target on its
/// next tick anyway). Never creates the store.
///
/// Lock order: call it only after the session store lock and the Agent
/// Management execution / mutation locks are released. It then takes the
/// asks store file lock alone (briefly, once per change), so the
/// retirement never holds the session store or Agent Management locks and
/// the asks lock at the same time; no new lock nesting is introduced.
pub fn settle_retired_target_default(session: &str) {
    let Ok(store) = AskStore::open_default() else {
        return;
    };
    match settle_retired_target(&store, session, Utc::now()) {
        Ok(settled) => {
            if !settled.is_empty() {
                crate::management::v2::asks::publish_pending_notices(&store);
            }
        }
        Err(error) => eprintln!("asks: could not settle the delegations of retired {session}: {error:#}"),
    }
}

/// Retry an interrupted ask as a new attempt (requester or owner).
pub fn retry(state: &mut AskState, ask_id: &str, requester: Option<&str>, now: DateTime<Utc>) -> Result<AskRecord, AskError> {
    let ask = state.asks.get(ask_id).cloned().ok_or_else(|| AskError::new("ask_not_found", "no such ask"))?;
    if ask.remote.is_some() { return Err(AskError::new("remote_unsupported","remote retry is unavailable in P0")); }
    if let Some(requester) = requester {
        ask.require_controller(requester, "ask_not_found", "no such ask of yours")?;
    }
    if ask.execution != ExecutionState::Interrupted || !ask.is_open() {
        return Err(AskError::new("ask_not_retryable", "only an open, interrupted ask can be retried")
            .with(json!({"execution": ask.execution, "closed": ask.closed})));
    }
    state.update_ask(ask_id, now, |ask| {
        ask.execution = ExecutionState::Queued;
        ask.hold = None;
        // A deliberate retry gets a fresh submit-retry budget.
        ask.submit_retry = None;
    });
    Ok(state.asks[ask_id].clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::scheduler::tests::{world, FakeRuntime};
    use crate::asks::scheduler::tick;
    use crate::asks::store::tests::{new_ask, temp_store, REQUESTER, TARGET};

    /// Acceptance §12.3 item 8 (part): fork/revert refused with the
    /// blocking asks named; revert of an answering turn flags the ask.
    /// Item 5/4 (part): retry is a new attempt with the interruption noted.
    #[test]
    fn obligations_revert_flag_and_retry() {
        let (world, dir) = world("lifecycle");
        let (store, store_dir) = temp_store("lifecycle");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        assert!(check_no_obligations(&store, TARGET, "fork").is_ok());
        let ask = store.with_state(|state| state.create_ask(new_ask("a", "x"), now).unwrap().0.ask_id).unwrap();
        tick(&world, &runtime, &store, now).unwrap();
        let refused = check_no_obligations(&store, TARGET, "fork").unwrap_err();
        assert_eq!(refused.code, "ask_obligations_pending");
        assert_eq!(refused.extra["blockingAsks"][0]["askId"], ask.as_str());
        assert!(check_no_obligations(&store, REQUESTER, "fork").is_ok());
        crate::asks::observe::observe(&store, TARGET, "turn/completed", &json!({"turn": {"id": "turn-1", "status": "interrupted"}}), now).unwrap();
        assert!(check_no_obligations(&store, TARGET, "revert").is_ok());
        // Retry: a new attempt that says attempt 1 was interrupted.
        store.with_state(|state| retry(state, &ask, Some(REQUESTER), now)).unwrap().unwrap();
        runtime.set(TARGET, |status| status.active_turn_id = None);
        tick(&world, &runtime, &store, now).unwrap();
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&ask].attempts.len(), 2);
        assert!(runtime.submitted.borrow()[1].1.text.contains("attempt 1 was interrupted"));
        assert_eq!(
            store.with_state(|state| retry(state, &ask, Some(REQUESTER), now)).unwrap().unwrap_err().code,
            "ask_not_retryable"
        );
        // Revert of the answering turn flags the ask.
        crate::asks::observe::observe(&store, TARGET, "turn/completed", &json!({"turn": {"id": "turn-2", "status": "completed"}}), now).unwrap();
        let flagged = store.with_state(|state| mark_reverted(state, TARGET, &["turn-2".into()], now)).unwrap();
        assert_eq!(flagged, vec![ask.clone()]);
        assert!(store.snapshot().unwrap().asks[&ask].target_reverted);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    /// Retirement settles the retired agent's asks as target: the running
    /// one ends `interrupted(stopped)` (lease freed), the queued one fails
    /// `recipient_archived`; an ask it requested is left alone.
    #[test]
    fn retirement_settles_queued_and_running_asks_as_target() {
        let (world, dir) = world("retire");
        let (store, store_dir) = temp_store("retire");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        assert!(settle_retired_target(&store, TARGET, now).unwrap().is_empty(), "no store: nothing");
        let (running, queued) = store
            .with_state(|state| {
                (state.create_ask(new_ask("a", "x"), now).unwrap().0.ask_id, state.create_ask(new_ask("b", "y"), now).unwrap().0.ask_id)
            })
            .unwrap();
        tick(&world, &runtime, &store, now).unwrap();
        crate::asks::observe::observe(&store, TARGET, "turn/started", &json!({"turn": {"id": "turn-1"}}), now).unwrap();
        let mut reverse = new_ask("c", "z");
        reverse.requester = Requester::Agent { cutex_session_id: TARGET.into(), name: Some("Kimi".into()) };
        reverse.target_cutex_session_id = REQUESTER.into();
        reverse.target_name = Some("Eva".into());
        let requested = store.with_state(|state| state.create_ask(reverse, now).unwrap().0.ask_id).unwrap();
        runtime.set(TARGET, |status| {
            status.online = false;
            status.active_turn_id = None;
        });
        let mut settled = settle_retired_target(&store, TARGET, now).unwrap();
        settled.sort();
        let mut expected = vec![running.clone(), queued.clone()];
        expected.sort();
        assert_eq!(settled, expected);
        let state = store.snapshot().unwrap();
        let record = &state.asks[&running];
        assert_eq!(record.execution, ExecutionState::Interrupted);
        assert_eq!(record.closed, Some(CloseOutcome::Stopped));
        assert_eq!(record.attempts[0].interrupt_reason, Some(InterruptReason::Stopped));
        assert!(state.leases.get(TARGET).is_none());
        assert_eq!(state.asks[&queued].execution, ExecutionState::Failed);
        assert!(state.asks[&queued].attempts.is_empty(), "never ran");
        assert!(state.asks[&queued].closed.is_some());
        assert!(state.asks[&requested].is_open(), "asks it requested are left alone");
        assert_eq!(state.asks[&requested].execution, ExecutionState::Queued);
        assert!(state.notices.iter().any(|notice| notice.method == "cutex/ask/answered"));
        // Idempotent.
        assert!(settle_retired_target(&store, TARGET, now).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }
}
