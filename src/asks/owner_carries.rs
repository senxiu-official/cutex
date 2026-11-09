//! Atomic owner-input result reservation. Native IO belongs to the caller.
use super::model::*;
use super::returns::{
    event_submission, fit_returns, remove_event_lease, return_item, waiting, OWNER_NOTE_PREFIX,
};
use super::scheduler::Submission;
use super::store::{AskState, AskStore};
use super::{new_id, now_rfc3339};
use crate::app_server::external_input::size::ReceiverLimit;
use crate::config::documents::DocumentSettings;
use chrono::{DateTime, Utc};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnerResultBundle {
    pub event_id: String,
    pub ids: Vec<String>,
    pub input_ids: Vec<String>,
    pub submission: Submission,
}

fn reserved_event<'a>(
    state: &'a AskState,
    session: &str,
    owner_input_id: &str,
) -> Option<&'a EventOutboxRecord> {
    state.events.values().find(|event| {
        event.cutex_session_id == session
            && event.delivery_ids.iter().any(|id| {
                state
                    .deliveries
                    .get(id)
                    .is_some_and(|d| d.owner_input_id.as_deref() == Some(owner_input_id))
            })
    })
}

fn bundle(
    store: &AskStore,
    state: &AskState,
    event: &EventOutboxRecord,
) -> anyhow::Result<OwnerResultBundle> {
    let artifact = event
        .event_artifact
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("owner result artifact missing"))?;
    let (_, bytes) = store
        .read_artifact(artifact, None)?
        .ok_or_else(|| anyhow::anyhow!("owner result artifact unavailable"))?;
    let text = String::from_utf8(bytes)?;
    Ok(OwnerResultBundle {
        event_id: event.event_id.clone(),
        ids: event.delivery_ids.clone(),
        input_ids: event.delivery_ids.clone(),
        submission: event_submission(state, event, text),
    })
}

fn open(state: &AskState, session: &str) -> bool {
    // This reservation belongs to an admitted owner start/steer. Its message
    // may carry through a delegation pause/stop hold without lifting that
    // gate for independent workers. The landed-owner hook clears it later.
    !state.next_step_leases.contains_key(session)
        && !super::agent_pause::is_paused(state, session)
        && !waiting(state, session)
        && !state.events.values().any(|event| event.cutex_session_id == session && !event.acknowledged)
        // Steering an active ask preserves that ask's primary lease. An
        // unbound ask/event is an outstanding native obligation, not a turn.
        && state.leases.get(session).is_none_or(|lease| match &lease.holder {
            LeaseHolder::Owner => true,
            LeaseHolder::Ask { .. } | LeaseHolder::Event { .. } => lease.turn_id.is_some(),
        })
}

fn eligible(state: &AskState, delivery: &DeliveryRecord, session: &str) -> bool {
    !delivery.passive
        && delivery.owner_input_id.is_none()
        && delivery.event_id.is_none()
        && super::delivery::actionable(state, delivery)
        && super::delivery::is_for(state, delivery, session)
        && !state.asks.get(&delivery.ask_id).is_some_and(|ask| {
            ask.execution == ExecutionState::PendingReview
                || matches!(ask.hold, Some(Hold::WaitingForApproval))
        })
}

/// Freeze a result set for the SAME atomic native owner request. The caller
/// holds the session input lane and has admitted the owner request. Delegation
/// pauses remain intact until the owner's native request actually lands. Repeated
/// owner ids reconstruct the identical event, even after a restart or read.
/// This does not submit passive IO or claim the primary admission lease.
#[allow(clippy::too_many_arguments)]
pub fn reserve_owner_results(
    store: &AskStore,
    session: &str,
    owner_input_id: &str,
    generation: u64,
    receiver_limit: ReceiverLimit,
    settings: DocumentSettings,
    now: DateTime<Utc>,
) -> anyhow::Result<Option<OwnerResultBundle>> {
    anyhow::ensure!(
        !owner_input_id.is_empty(),
        "owner input id must not be empty"
    );
    let state = store.shared()?;
    if let Some(event) = reserved_event(&state, session, owner_input_id) {
        return bundle(store, &state, event).map(Some);
    }
    if !open(&state, session) {
        return Ok(None);
    }
    let ids = super::delivery::in_creation_order(
        &state,
        state
            .deliveries
            .values()
            .filter(|d| eligible(&state, d, session))
            .map(|d| d.delivery_id.clone())
            .collect(),
    );
    let pairs: Vec<_> = ids
        .iter()
        .filter_map(|id| state.deliveries.get(id))
        .filter_map(|d| return_item(store, &state, d).map(|item| (d.delivery_id.clone(), item)))
        // The native owner batch has a 100-entry limit, including owner.
        // Leave room for bus carries alongside these source delivery ids.
        .take(64)
        .collect();
    if pairs.is_empty() {
        return Ok(None);
    }
    let message_id = new_id(OWNER_NOTE_PREFIX);
    let items: Vec<_> = pairs.iter().map(|(_, item)| item.clone()).collect();
    let fitted = fit_returns(
        &items,
        &receiver_limit.budget(settings.margin_percent),
        &message_id,
        settings.preview_bytes as usize,
    );
    // Reserve exactly what the fitter rendered. Deferred items keep FIFO;
    // individually unfit items also remain pending, never passive or lost.
    if fitted.included.is_empty() {
        return Ok(None);
    }
    let pairs: Vec<_> = fitted
        .included
        .iter()
        .map(|&index| pairs[index].clone())
        .collect();
    let items: Vec<_> = pairs.iter().map(|(_, item)| item.clone()).collect();
    let mut view = super::views::results_view(
        &items,
        &|id| {
            state
                .asks
                .get(id)
                .map(|a| (a.target_cutex_session_id.clone(), a.target_name.clone()))
                .unwrap_or_default()
        },
        &|id| state.answers.get(id).and_then(|a| a.preview.clone()),
    );
    // Views may omit tail rows, including the only answer. Freeze the
    // rendered type explicitly so detach/retry and quoted headings cannot
    // change the envelope. Existing events never gain this identity field.
    view["data"]["ownerCarryEventType"] = serde_json::json!(fitted.event_type);
    while super::views::structured(&view).is_none_or(|v| v.canonical_json().is_err()) {
        let rows = view["data"]["items"].as_array_mut().expect("results items");
        anyhow::ensure!(rows.pop().is_some(), "owner results view cannot fit");
        let omitted = view["data"]["omitted"].as_u64().unwrap_or(0) + 1;
        view["data"]["omitted"] = serde_json::json!(omitted);
    }
    let artifact = store.put_artifact(
        fitted.text.as_bytes(),
        "text/plain; charset=utf-8",
        &[],
        None,
        now,
    )?;
    let reserved = super::pause_admission::with_state_or(store, None, |state, _| {
        if let Some(event) = reserved_event(state, session, owner_input_id) {
            return Some(event.clone());
        }
        if !open(state, session)
            || !pairs.iter().all(|(id, _)| {
                state
                    .deliveries
                    .get(id)
                    .is_some_and(|d| eligible(state, d, session))
            })
        {
            return None;
        }
        let ids: Vec<_> = pairs.iter().map(|(id, _)| id.clone()).collect();
        let event_id = super::delivery::coalesce_selected_into_event(
            state,
            session,
            &message_id,
            ids.clone(),
            now,
        )?;
        for id in &ids {
            state
                .deliveries
                .get_mut(id)
                .expect("reserved delivery")
                .owner_input_id = Some(owner_input_id.into());
        }
        let event = state.events.get_mut(&event_id).expect("reserved event");
        event.event_artifact = Some(artifact);
        event.view = Some(view);
        let event = event.clone();
        state.next_step_leases.insert(
            session.into(),
            AdmissionLease {
                cutex_session_id: session.into(),
                holder: LeaseHolder::Event { event_id },
                native_message_id: message_id,
                runtime_generation: generation,
                turn_id: None,
                since: now_rfc3339(now),
            },
        );
        Some(event)
    })?;
    let Some(event) = reserved else {
        return Ok(None);
    };
    let current = store.shared()?;
    bundle(store, &current, &event).map(Some)
}

/// ONLY call when the atomic owner request is proven unsent/refused. Missing
/// receipts, transport errors and restarts are never proof. An acknowledged
/// event is never released. Returns whether an unconsumed reservation existed.
pub fn release_owner_results(
    store: &AskStore,
    session: &str,
    owner_input_id: &str,
    now: DateTime<Utc>,
) -> anyhow::Result<bool> {
    // Select exact identities without drafting the whole store at this call
    // site. The repository promotes their connected archive records under
    // its write lock; this read grants no authority to release anything.
    let selected = store.inspect(|state| {
        let event = reserved_event(state, session, owner_input_id)?;
        if event.acknowledged || event.turn_id.is_some() {
            return None;
        }
        let mut records = std::collections::BTreeSet::from([event.event_id.clone()]);
        for id in &event.delivery_ids {
            records.insert(id.clone());
            if let Some(delivery) = state.deliveries.get(id) {
                records.insert(delivery.ask_id.clone());
            }
        }
        Some((
            event.event_id.clone(),
            records.into_iter().collect::<Vec<_>>(),
        ))
    })?;
    let Some((event_id, records)) = selected else {
        return Ok(false);
    };
    let keys: Vec<&str> = records.iter().map(String::as_str).collect();
    store.with_records(&keys, |state| {
        let Some(event) = state.events.get(&event_id) else {
            return false;
        };
        // Revalidate after acquiring the mutation lock. A receipt, retry,
        // reroute or concurrent release may have changed these exact rows.
        if event.cutex_session_id != session
            || event.acknowledged
            || event.turn_id.is_some()
            || !event.delivery_ids.iter().any(|id| {
                state
                    .deliveries
                    .get(id)
                    .is_some_and(|d| d.owner_input_id.as_deref() == Some(owner_input_id))
            })
        {
            return false;
        }
        super::delivery::release_event(state, &event_id, now);
        remove_event_lease(state, session, &event_id);
        true
    })
}

/// Explicit owner hold on a pending delivery. In-flight reservations cannot
/// be changed. Tool wait/read access remains available, independent of hold.
pub fn set_owner_hold(
    store: &AskStore,
    session: &str,
    input_id: &str,
    held: bool,
    now: DateTime<Utc>,
) -> anyhow::Result<bool> {
    store.with_records(&[input_id], |state| {
        let Some(delivery) = state.deliveries.get(input_id) else {
            return false;
        };
        if delivery.state != DeliveryState::Pending
            || !super::delivery::is_for(state, delivery, session)
        {
            return false;
        }
        let delivery = state
            .deliveries
            .get_mut(input_id)
            .expect("checked delivery");
        delivery.owner_hold = held;
        delivery.updated_at = now_rfc3339(now);
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::scheduler::tests::{world, FakeRuntime};
    use crate::asks::scheduler::{NativeMessageStatus, TickReport};
    use crate::asks::store::tests::{new_ask, temp_store, REQUESTER, TARGET};

    struct ReservationAuthority;

    impl Drop for ReservationAuthority {
        fn drop(&mut self) {
            super::super::pause_admission::release_worker_fence();
        }
    }

    // These tests assert source selection, not admission-fence contention.
    // Other carries tests share the private host authority. Enter through
    // the owner worker's fence so parallel tests cannot turn a valid
    // reservation into the production API's expected Deferred/None result.
    // RAII releases the fence even when an assertion or store read panics.
    #[allow(clippy::too_many_arguments)]
    fn reserve_owner_results(
        store: &AskStore,
        session: &str,
        owner_input_id: &str,
        generation: u64,
        receiver_limit: ReceiverLimit,
        settings: DocumentSettings,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Option<OwnerResultBundle>> {
        let acquired = !super::super::pause_admission::worker_fence_held();
        if acquired {
            super::super::pause_admission::acquire_worker_fence()?;
        }
        let _authority = acquired.then(|| ReservationAuthority);
        super::reserve_owner_results(
            store,
            session,
            owner_input_id,
            generation,
            receiver_limit,
            settings,
            now,
        )
    }

    fn pending(store: &AskStore, name: &str, now: DateTime<Utc>) -> String {
        store
            .with_state(|state| {
                let (ask, _) = state.create_ask(new_ask(name, name), now).unwrap();
                state.update_ask(&ask.ask_id, now, |ask| {
                    ask.execution = ExecutionState::Answered
                });
                super::super::delivery::record_owner_stopped_delivery(state, &ask, now).unwrap()
            })
            .unwrap()
    }

    fn reserve(store: &AskStore, owner: &str, now: DateTime<Utc>) -> Option<OwnerResultBundle> {
        reserve_owner_results(
            store,
            REQUESTER,
            owner,
            1,
            ReceiverLimit::Off,
            DocumentSettings::default(),
            now,
        )
        .unwrap()
    }

    #[test]
    fn stale_blocked_notice_owner_carry_and_resume_both_commit_orders() {
        for reserve_first in [false, true] {
            let (store, dir) = temp_store("stale-notice-owner-carry-race");
            let now = Utc::now();
            let (ask, notice) = store.with_state(|state| {
                let (ask, _) = state.create_ask(new_ask("blocked", "blocked"), now).unwrap();
                super::super::wake::record_owner_action(state, TARGET, "ownerStopped", None, now);
                let id = state.deliveries.values().find(|d| d.ask_id == ask.ask_id).unwrap().delivery_id.clone();
                (ask.ask_id, id)
            }).unwrap();
            let bundle = reserve_first.then(|| reserve(&store, "owner-race", now).unwrap());
            store.with_state(|state| {
                state.update_ask(&ask, now, |ask| ask.execution = ExecutionState::Running);
                super::super::wake::clear_owner_action(state, TARGET, now);
            }).unwrap();
            // Reopen the actual source database: protection is durable, not
            // a process-local selection cache.
            drop(store);
            let store = AskStore::at(&dir);
            if let Some(bundle) = bundle {
                assert_eq!(reserve(&store, "owner-race", now).unwrap(), bundle);
                let state = store.snapshot().unwrap();
                assert_eq!(state.deliveries[&notice].state, DeliveryState::QueuedAsEvent);
                assert!(!state.deliveries[&notice].unsubmitted);
                assert_eq!(state.deliveries[&notice].owner_input_id.as_deref(), Some("owner-race"));
                assert!(release_owner_results(&store, REQUESTER, "owner-race", now).unwrap());
                assert_eq!(store.snapshot().unwrap().deliveries[&notice].settled_by.as_deref(), Some(super::super::delivery::SUPERSEDED));
                assert!(reserve(&store, "new-owner", now).is_none());
            } else {
                assert!(reserve(&store, "owner-race", now).is_none());
                assert!(super::super::delivery::claim_for_wait(&mut store.snapshot().unwrap(), REQUESTER, &[ask], "read", now).is_empty());
            }
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn fixed_selection_duplicate_owner_and_proven_refusal_release() {
        let (store, dir) = temp_store("owner-carry-fixed");
        let now = Utc::now();
        let first = pending(&store, "first", now);
        let held = pending(&store, "held", now);
        let passive = pending(&store, "passive", now);
        assert!(set_owner_hold(&store, REQUESTER, &held, true, now).unwrap());
        store
            .with_state(|state| state.deliveries.get_mut(&passive).unwrap().passive = true)
            .unwrap();
        let bundle = reserve(&store, "owner-1", now).unwrap();
        assert_eq!(bundle.ids, vec![first.clone()]);
        assert!(bundle.submission.message_id.starts_with(OWNER_NOTE_PREFIX));
        let late = pending(&store, "late", now + chrono::Duration::seconds(1));
        assert_eq!(reserve(&store, "owner-1", now).unwrap(), bundle);
        assert!(reserve(&store, "owner-2", now).is_none());
        assert!(
            !set_owner_hold(&store, REQUESTER, &first, true, now).unwrap(),
            "cannot hold in-flight input"
        );
        assert!(!release_owner_results(&store, TARGET, "owner-1", now).unwrap());
        assert!(release_owner_results(&store, REQUESTER, "owner-1", now).unwrap());
        let state = store.snapshot().unwrap();
        assert_eq!(state.deliveries[&first].state, DeliveryState::Pending);
        assert!(state.deliveries[&first].owner_input_id.is_none());
        assert!(state.deliveries[&held].owner_hold);
        let next = reserve(&store, "owner-2", now).unwrap();
        assert_eq!(next.ids, vec![first, late]);
        assert_ne!(next.event_id, bundle.event_id);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn persistent_hold_excludes_scheduler_but_preserves_manual_wait_and_release() {
        let (store, dir) = temp_store("owner-carry-hold");
        let now = Utc::now();
        let id = pending(&store, "held", now);
        assert!(!set_owner_hold(&store, TARGET, &id, true, now).unwrap());
        assert!(set_owner_hold(&store, REQUESTER, &id, true, now).unwrap());
        drop(store);
        let store = AskStore::at(&dir);
        let state = store.snapshot().unwrap();
        assert!(state.deliveries[&id].owner_hold);
        store
            .with_state(|state| {
                super::super::control::owner_stop(state, REQUESTER, now);
                super::super::control::owner_resume(state, REQUESTER, "test", now);
            })
            .unwrap();
        assert!(
            store.snapshot().unwrap().deliveries[&id].owner_hold,
            "stop/resume preserves per-delivery hold"
        );
        assert!(!super::super::delivery::actionable(
            &state,
            &state.deliveries[&id]
        ));
        assert!(super::super::delivery::requesters_with_pending(&state).is_empty());
        assert!(reserve(&store, "held-owner", now).is_none());
        assert!(set_owner_hold(&store, REQUESTER, &id, false, now).unwrap());
        assert!(super::super::delivery::actionable(
            &store.snapshot().unwrap(),
            &store.snapshot().unwrap().deliveries[&id]
        ));
        assert!(set_owner_hold(&store, REQUESTER, &id, true, now).unwrap());
        let claimed = store
            .with_state(|state| {
                super::super::delivery::claim_for_wait(state, REQUESTER, &[], "manual", now)
            })
            .unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].delivery_id, id);
        assert!(
            claimed[0].owner_hold,
            "manual read need not clear the owner's hold"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    fn pause_for_owner(store: &AskStore, stopped: bool, now: DateTime<Utc>) {
        store
            .with_state(|state| {
                if stopped {
                    super::super::control::owner_stop(state, REQUESTER, now);
                } else {
                    super::super::control::owner_pause(state, REQUESTER, now);
                }
            })
            .unwrap();
    }

    #[test]
    fn owner_pause_and_stop_allow_capture_and_proven_unsent_release_without_resuming_workers() {
        for stopped in [false, true] {
            let (world, world_dir) = world("owner-carry-paused-unsent");
            let (store, dir) = temp_store("owner-carry-paused-unsent");
            let runtime = FakeRuntime::idle(&[TARGET, REQUESTER]);
            let now = Utc::now();
            let ready = pending(&store, "paused-ready", now);
            let held = pending(&store, "paused-item-held", now);
            assert!(set_owner_hold(&store, REQUESTER, &held, true, now).unwrap());
            pause_for_owner(&store, stopped, now);
            let before = store.snapshot().unwrap();
            let reserved = reserve(&store, "paused-owner", now).unwrap();
            assert_eq!(reserved.input_ids, vec![ready.clone()]);
            let captured = store.snapshot().unwrap();
            assert_eq!(captured.pauses, before.pauses);
            assert_eq!(captured.commands, before.commands);
            assert!(captured.deliveries[&held].owner_hold);
            assert!(!captured.leases.contains_key(REQUESTER));
            assert!(release_owner_results(&store, REQUESTER, "paused-owner", now).unwrap());
            let released = store.snapshot().unwrap();
            assert_eq!(released.pauses, before.pauses);
            assert_eq!(released.commands, before.commands);
            assert_eq!(released.deliveries[&ready].state, DeliveryState::Pending);
            assert!(released.deliveries[&ready].owner_input_id.is_none());
            super::super::returns::admit_events(
                &world,
                &runtime,
                &store,
                &mut TickReport::default(),
                now + chrono::Duration::minutes(10),
            )
            .unwrap();
            assert!(
                runtime.submitted.borrow().is_empty(),
                "paused independent result delivery remains blocked"
            );
            assert!(runtime.steered.borrow().is_empty());
            let final_state = store.snapshot().unwrap();
            assert_eq!(final_state.pauses, before.pauses);
            assert_eq!(final_state.commands, before.commands);
            assert_eq!(final_state.deliveries[&ready].state, DeliveryState::Pending);
            let _ = std::fs::remove_dir_all(dir);
            let _ = std::fs::remove_dir_all(world_dir);
        }
    }

    #[test]
    fn paused_owner_result_consumes_actual_receipt_without_clearing_pause_or_commands() {
        for stopped in [false, true] {
            let (world, world_dir) = world("owner-carry-paused-receipt");
            let (store, dir) = temp_store("owner-carry-paused-receipt");
            let runtime = FakeRuntime::idle(&[TARGET, REQUESTER]);
            let now = Utc::now();
            let ready = pending(&store, "paused-receipt", now);
            pause_for_owner(&store, stopped, now);
            let before = store.snapshot().unwrap();
            let reserved = reserve(&store, "paused-receipt-owner", now).unwrap();
            runtime.native.borrow_mut().insert(
                reserved.submission.message_id.clone(),
                NativeMessageStatus::Persisted {
                    turn_id: "actual-paused-owner-turn".into(),
                },
            );
            super::super::returns::admit_events(
                &world,
                &runtime,
                &store,
                &mut TickReport::default(),
                now,
            )
            .unwrap();
            let consumed = store.snapshot().unwrap();
            assert_eq!(
                consumed.deliveries[&ready].state,
                DeliveryState::Acknowledged
            );
            assert_eq!(
                consumed.events[&reserved.event_id].turn_id.as_deref(),
                Some("actual-paused-owner-turn")
            );
            assert!(!consumed.next_step_leases.contains_key(REQUESTER));
            assert_eq!(
                consumed.pauses, before.pauses,
                "only the landed-owner hook resumes delegation admission"
            );
            assert_eq!(consumed.commands, before.commands);
            assert!(runtime.submitted.borrow().is_empty());
            assert!(runtime.steered.borrow().is_empty());
            let _ = std::fs::remove_dir_all(dir);
            let _ = std::fs::remove_dir_all(world_dir);
        }
    }

    #[test]
    fn owner_capture_through_stop_does_not_override_whole_agent_pause_or_wait() {
        let (store, dir) = temp_store("owner-carry-whole-pause");
        let now = Utc::now();
        let ready = pending(&store, "whole-pause-ready", now);
        pause_for_owner(&store, true, now);
        store
            .with_state(|state| {
                super::super::agent_pause::pause(state, REQUESTER, "owner", None, now);
            })
            .unwrap();
        let before = store.snapshot().unwrap();
        assert!(reserve(&store, "whole-paused-owner", now).is_none());
        assert_eq!(store.snapshot().unwrap().pauses, before.pauses);
        assert_eq!(store.snapshot().unwrap().presence, before.presence);
        store
            .with_state(|state| {
                super::super::agent_pause::resume(state, REQUESTER);
                state.waits.insert(
                    "active-wait".into(),
                    super::super::store::WaitRecord {
                        wait_id: "active-wait".into(),
                        cutex_session_id: REQUESTER.into(),
                        action_id: "manual-wait".into(),
                        asks: vec![state.deliveries[&ready].ask_id.clone()],
                        all: false,
                        started_at: now_rfc3339(now),
                        deadline: now_rfc3339(now + chrono::Duration::minutes(5)),
                        cancel_epoch: 0,
                    },
                );
            })
            .unwrap();
        let waiting = store.snapshot().unwrap();
        assert!(reserve(&store, "waiting-owner", now).is_none());
        let after = store.snapshot().unwrap();
        assert_eq!(after.waits, waiting.waits);
        assert_eq!(after.commands, waiting.commands);
        assert_eq!(after.deliveries[&ready].state, DeliveryState::Pending);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn atomic_receipt_survives_restart_and_unknown_then_binds_actual_turn() {
        let (world, world_dir) = world("owner-carry-receipt");
        let (store, dir) = temp_store("owner-carry-receipt");
        let runtime = FakeRuntime::idle(&[TARGET, REQUESTER]);
        let now = Utc::now();
        let id = pending(&store, "result", now);
        let primary = AdmissionLease {
            cutex_session_id: REQUESTER.into(),
            holder: LeaseHolder::Ask {
                ask_id: "primary".into(),
                attempt: 1,
            },
            native_message_id: "primary-native".into(),
            runtime_generation: 1,
            turn_id: Some("busy-turn".into()),
            since: now_rfc3339(now),
        };
        store
            .with_state(|state| state.leases.insert(REQUESTER.into(), primary.clone()))
            .unwrap();
        let bundle = reserve(&store, "owner-receipt", now).unwrap();
        assert_eq!(store.snapshot().unwrap().leases[REQUESTER], primary);
        drop(store);
        let store = AskStore::at(&dir);
        assert_eq!(reserve(&store, "owner-receipt", now).unwrap(), bundle);
        runtime.set(REQUESTER, |status| {
            status.runtime_generation = 2;
            status.active_turn_id = Some("actual-owner-turn".into());
        });
        let submitted = runtime.submitted.borrow().len();
        for status in [
            NativeMessageStatus::Absent,
            NativeMessageStatus::Unavailable("lost response".into()),
            NativeMessageStatus::Held {
                reason: "request_uncertain".into(),
                turn_id: None,
            },
        ] {
            runtime
                .native
                .borrow_mut()
                .insert(bundle.submission.message_id.clone(), status);
            super::super::returns::admit_events(
                &world,
                &runtime,
                &store,
                &mut TickReport::default(),
                now + chrono::Duration::hours(2),
            )
            .unwrap();
            assert_eq!(
                store.snapshot().unwrap().deliveries[&id].state,
                DeliveryState::QueuedAsEvent
            );
            assert_eq!(reserve(&store, "owner-receipt", now).unwrap(), bundle);
        }
        let before = store.snapshot().unwrap();
        runtime.native.borrow_mut().insert(
            bundle.submission.message_id.clone(),
            NativeMessageStatus::Persisted {
                turn_id: "actual-owner-turn".into(),
            },
        );
        super::super::returns::admit_events(
            &world,
            &runtime,
            &store,
            &mut TickReport::default(),
            now,
        )
        .unwrap();
        let after = store.snapshot().unwrap();
        assert_eq!(after.deliveries[&id].state, DeliveryState::Acknowledged);
        assert_eq!(
            after.events[&bundle.event_id].turn_id.as_deref(),
            Some("actual-owner-turn")
        );
        assert_eq!(
            after.events[&bundle.event_id].native_message_id,
            bundle.submission.message_id
        );
        assert_eq!(after.leases[REQUESTER], primary);
        assert!(!after.next_step_leases.contains_key(REQUESTER));
        assert!(!release_owner_results(&store, REQUESTER, "owner-receipt", now).unwrap());
        assert_eq!(runtime.submitted.borrow().len(), submitted);
        assert!(runtime.steered.borrow().is_empty());
        let job = super::super::ledger::delivery_job(
            &before,
            &after,
            before.deliveries.get(&id),
            &after.deliveries[&id],
        )
        .unwrap();
        let context = job
            .steps
            .iter()
            .flat_map(|step| step.alts.iter())
            .find(|next| next.state == crate::management::v2::input_ledger::InputState::InContext)
            .unwrap();
        assert_eq!(context.reason.as_deref(), Some("carriedByOwner"));
        assert_eq!(context.turn_id.as_deref(), Some("actual-owner-turn"));
        assert_eq!(context.batch_id.as_deref(), Some(bundle.event_id.as_str()));
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(world_dir);
    }

    #[test]
    fn unbound_lease_review_hold_and_receiver_budget_do_not_reserve() {
        let (store, dir) = temp_store("owner-carry-exclusions");
        let now = Utc::now();
        let id = pending(&store, "eligible", now);
        let ask_id = store.snapshot().unwrap().deliveries[&id].ask_id.clone();
        store
            .with_state(|state| {
                state.leases.insert(
                    REQUESTER.into(),
                    AdmissionLease {
                        cutex_session_id: REQUESTER.into(),
                        holder: LeaseHolder::Ask {
                            ask_id: ask_id.clone(),
                            attempt: 1,
                        },
                        native_message_id: "native-inflight".into(),
                        runtime_generation: 1,
                        turn_id: None,
                        since: now_rfc3339(now),
                    },
                )
            })
            .unwrap();
        assert!(reserve(&store, "owner-exclusion", now).is_none());
        store
            .with_state(|state| {
                state.leases.remove(REQUESTER);
                state.update_ask(&ask_id, now, |ask| {
                    ask.execution = ExecutionState::PendingReview
                });
            })
            .unwrap();
        assert!(reserve(&store, "owner-exclusion", now).is_none());
        store
            .with_state(|state| {
                state.update_ask(&ask_id, now, |ask| ask.execution = ExecutionState::Answered);
            })
            .unwrap();
        assert!(reserve_owner_results(
            &store,
            REQUESTER,
            "tiny",
            1,
            ReceiverLimit::RawText(1),
            DocumentSettings::default(),
            now
        )
        .unwrap()
        .is_none());
        assert!(store.snapshot().unwrap().events.is_empty());
        assert!(!store.snapshot().unwrap().deliveries[&id].passive);
        store
            .with_state(|state| {
                super::super::delivery::coalesce_into_event(state, REQUESTER, "already-native", now)
            })
            .unwrap();
        assert!(
            reserve(&store, "owner-exclusion", now).is_none(),
            "an existing event cannot be stolen or duplicated"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn quoted_answer_heading_keeps_frozen_question_identity_after_reconstruction() {
        use crate::asks::render;
        use serde_json::json;
        let (store, dir) = temp_store("owner-carry-question-type");
        let now = Utc::now();
        let state_id = pending(&store, "state", now + chrono::Duration::seconds(1));
        let question_id =
            store
                .with_state(|state| {
                    let (ask, _) = state
                        .create_ask(new_ask("quoted-question", "Question"), now)
                        .unwrap();
                    state.questions.insert("quoted-q".into(), QuestionRecord {
                question_id: "quoted-q".into(), ask_id: ask.ask_id.clone(), attempt: 1,
                runtime_generation: 1, native: NativeQuestionRef {
                    thread_id: "thread".into(), turn_id: "target-turn".into(),
                    item_id: "question-item".into(), request_id: json!(1),
                }, questions: json!([{"question": "Does the heading ## Answer belong here?"}]),
                route: QuestionRoute::Requester, routed_to_owner_at: None,
                state: QuestionState::Open, resolution: None, created_at: now_rfc3339(now),
            });
                    super::super::delivery::record_question_delivery(state, &ask, "quoted-q", now)
                        .unwrap()
                })
                .unwrap();
        let original = reserve(&store, "question-owner", now).unwrap();
        assert!(original.submission.text.contains("## Answer"));
        assert_eq!(original.submission.event_type, render::QUESTION_EVENT);
        assert_eq!(
            original.input_ids,
            vec![question_id.clone(), state_id.clone()]
        );
        drop(store);
        let store = AskStore::at(&dir);
        assert_eq!(reserve(&store, "question-owner", now).unwrap(), original);
        store
            .with_state(|state| {
                super::super::ledger::retry_result(state, REQUESTER, &question_id, now).unwrap()
            })
            .unwrap();
        let state = store.snapshot().unwrap();
        let event = &state.events[&original.event_id];
        assert_eq!(event.delivery_ids, vec![state_id]);
        assert_eq!(
            event.view, original.submission.view,
            "retry never edits the frozen view"
        );
        let rebuilt = event_submission(&state, event, original.submission.text.clone());
        assert_eq!(rebuilt.event_type, render::QUESTION_EVENT);
        let binding = crate::launch::stock::ExternalInputBinding {
            version: 1,
            owner_id: REQUESTER.into(),
            thread_id: "owner-thread".into(),
            runtime_generation: 1,
            canonical_byte_limit: Default::default(),
        };
        assert_eq!(
            rebuilt.envelope(&binding, true).unwrap().semantic_sha256,
            original
                .submission
                .envelope(&binding, true)
                .unwrap()
                .semantic_sha256
        );
        // Missing marker means an older frozen identity: do not migrate it
        // to the new rule simply because it already has a results view.
        let mut legacy = event.clone();
        legacy.view.as_mut().unwrap()["data"]
            .as_object_mut()
            .unwrap()
            .remove("ownerCarryEventType");
        assert_eq!(
            event_submission(&state, &legacy, original.submission.text).event_type,
            render::ANSWER_EVENT
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn partial_receiver_budget_reserves_only_rendered_results() {
        let (store, dir) = temp_store("owner-carry-partial-budget");
        let now = Utc::now();
        let first = pending(&store, "partial-first", now);
        let second = pending(&store, "partial-second", now + chrono::Duration::seconds(1));
        let third = pending(&store, "partial-third", now + chrono::Duration::seconds(2));
        store
            .with_state(|state| {
                for id in [&first, &second, &third] {
                    let ask_id = state.deliveries[id].ask_id.clone();
                    state.update_ask(&ask_id, now, |ask| {
                        ask.title = format!("{id}-{}", "title".repeat(120))
                    });
                }
            })
            .unwrap();
        let before = store.snapshot().unwrap();
        let item = return_item(&store, &before, &before.deliveries[&first]).unwrap();
        let settings = DocumentSettings::default();
        let single = fit_returns(
            &[item],
            &ReceiverLimit::Off.budget(settings.margin_percent),
            "sizing-only",
            settings.preview_bytes as usize,
        );
        let limit = ReceiverLimit::RawText((single.text.len() + 256) as u64);
        let bundle = reserve_owner_results(
            &store,
            REQUESTER,
            "partial-owner",
            1,
            limit,
            settings.clone(),
            now,
        )
        .unwrap()
        .unwrap();
        assert_eq!(bundle.input_ids, vec![first.clone()]);
        assert_eq!(
            bundle.submission.asks,
            vec![before.deliveries[&first].ask_id.clone()]
        );
        assert!(bundle.submission.text.len() <= limit.text_limit());
        let after = store.snapshot().unwrap();
        for id in [&second, &third] {
            assert_eq!(after.deliveries[id].state, DeliveryState::Pending);
            assert!(after.deliveries[id].event_id.is_none());
            assert!(after.deliveries[id].owner_input_id.is_none());
            assert!(!after.deliveries[id].passive);
        }
        assert_eq!(
            after.events[&bundle.event_id].delivery_ids,
            bundle.input_ids
        );
        assert_eq!(
            reserve_owner_results(
                &store,
                REQUESTER,
                "partial-owner",
                1,
                ReceiverLimit::Off,
                settings,
                now
            )
            .unwrap()
            .unwrap(),
            bundle,
            "a later budget cannot enlarge a frozen reservation"
        );
        assert!(release_owner_results(&store, REQUESTER, "partial-owner", now).unwrap());
        store
            .with_state(|state| {
                let ask_id = state.deliveries[&second].ask_id.clone();
                state.update_ask(&ask_id, now, |ask| ask.title = "oversized".repeat(1000));
            })
            .unwrap();
        let with_unfit =
            reserve_owner_results(&store, REQUESTER, "partial-unfit", 1, limit, settings, now)
                .unwrap()
                .unwrap();
        assert_eq!(with_unfit.input_ids, vec![first]);
        let state = store.snapshot().unwrap();
        assert_eq!(state.deliveries[&second].state, DeliveryState::Pending);
        assert!(!state.deliveries[&second].passive);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 057 §6: a carried results event comes back only when the runtime gives
    /// it back; a hold or a missing status alone changes nothing.
    #[test]
    fn a_carried_result_is_released_only_when_the_runtime_gives_it_back() {
        let (world, world_dir) = world("owner-carry-returned");
        let (store, dir) = temp_store("owner-carry-returned-store");
        let runtime = FakeRuntime::idle(&[TARGET, REQUESTER]);
        runtime.set(REQUESTER, |status| status.active_turn_id = Some("busy".into()));
        let now = Utc::now();
        let delivery = pending(&store, "r", now);
        let bundle = reserve_owner_results(&store, REQUESTER, "owner-dropped", 1, ReceiverLimit::Off, DocumentSettings::default(), now).unwrap().unwrap();
        let message = bundle.submission.message_id.clone();
        for status in [NativeMessageStatus::Held { reason: "interrupted".into(), turn_id: None },
            NativeMessageStatus::Absent, NativeMessageStatus::Pending, NativeMessageStatus::Unavailable("no".into())] {
            runtime.native.borrow_mut().insert(message.clone(), status);
            crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
            let state = store.snapshot().unwrap();
            assert_eq!(state.deliveries[&delivery].owner_input_id.as_deref(), Some("owner-dropped"));
            assert!(state.events.contains_key(&bundle.event_id));
        }
        runtime.carries_returned.borrow_mut().insert(message.clone());
        crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
        let state = store.snapshot().unwrap();
        assert_eq!(state.deliveries[&delivery].owner_input_id, None);
        assert!(!state.events.contains_key(&bundle.event_id));
        assert!(state.next_step_leases.get(REQUESTER).is_none_or(|lease| lease.native_message_id != message));
        for dir in [dir, world_dir] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn owner_batch_caps_source_ids_and_only_marks_accepted_as_carried() {
        let (store, dir) = temp_store("owner-carry-cap");
        let now = Utc::now();
        let mut ids = Vec::new();
        for n in 0..66 {
            ids.push(pending(
                &store,
                &format!("r{n}"),
                now + chrono::Duration::milliseconds(n),
            ));
        }
        let reserved = reserve(&store, "capped", now).unwrap();
        assert_eq!(reserved.input_ids, ids[..64]);
        assert_eq!(reserved.ids, reserved.input_ids);
        let state = store.snapshot().unwrap();
        assert!(ids[64..]
            .iter()
            .all(|id| state.deliveries[id].state == DeliveryState::Pending));
        let jobs = super::super::ledger::submitted_jobs_in(
            &state,
            REQUESTER,
            &reserved.input_ids,
            &reserved.event_id,
            Some(&reserved.submission.message_id),
            Some("owner-turn"),
            false,
        );
        assert_eq!(jobs.len(), 64);
        for job in jobs {
            assert_eq!(
                job.steps[0].alts[0].state,
                crate::management::v2::input_ledger::InputState::Sending
            );
            assert!(job.steps[0].alts[0].reason.is_none());
            assert_eq!(
                job.steps[1].alts[0].reason.as_deref(),
                Some("carriedByOwner")
            );
        }
        assert_eq!(reserve(&store, "capped", now).unwrap(), reserved);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn waiting_projection_exposes_explicit_hold_boolean() {
        let (store, dir) = temp_store("owner-carry-projection");
        let now = Utc::now();
        let id = pending(&store, "hold-projection", now);
        let before = store.snapshot().unwrap();
        let job =
            super::super::ledger::delivery_job(&before, &before, None, &before.deliveries[&id])
                .unwrap();
        assert_eq!(
            job.steps[0].alts[0].details.as_ref().unwrap()["ownerHold"],
            false
        );
        set_owner_hold(&store, REQUESTER, &id, true, now).unwrap();
        let after = store.snapshot().unwrap();
        let job = super::super::ledger::delivery_job(
            &before,
            &after,
            before.deliveries.get(&id),
            &after.deliveries[&id],
        )
        .unwrap();
        assert_eq!(job.steps[0].alts[0].reason.as_deref(), Some("ownerPause"));
        assert_eq!(
            job.steps[0].alts[0].details.as_ref().unwrap()["ownerHold"],
            true
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
