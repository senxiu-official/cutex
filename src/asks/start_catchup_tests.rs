use crate::asks::model::*;
use crate::asks::now_rfc3339;
use crate::asks::scheduler::tests::{world, FakeRuntime};
use crate::asks::scheduler::*;
use crate::asks::store::tests::{new_ask, temp_store, TARGET};
use crate::asks::store::{AskState, AskStore};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

fn history(lease: &AdmissionLease) -> Value {
    let LeaseHolder::Ask { ask_id, attempt } = &lease.holder else {
        panic!()
    };
    json!({"id":lease.turn_id,"status":"inProgress","items":[{
        "type":"functionCallOutput","id":lease.native_message_id,
        "externalInputView":{"schema":crate::asks::views::ASK_VIEW,"data":{"askId":ask_id,"attempt":attempt}}
    }]})
}

fn admitted(
    label: &str,
) -> (
    AskStore,
    std::path::PathBuf,
    FakeRuntime,
    AdmissionLease,
    DateTime<Utc>,
) {
    let (store, dir) = temp_store(label);
    let now = Utc::now();
    let lease = store
        .with_state(|state| {
            let ask_id = state
                .create_ask(new_ask(label, "input"), now)
                .unwrap()
                .0
                .ask_id;
            let mut record =
                AttemptRecord::submitting(1, "native-original".into(), 1, now_rfc3339(now));
            record.admission_occurrence = 7;
            record.state = AttemptState::Admitted;
            record.turn_id = Some("turn-original".into());
            state.update_ask(&ask_id, now, |ask| {
                ask.execution = ExecutionState::Admitted;
                ask.attempts.push(record);
            });
            let lease = AdmissionLease {
                cutex_session_id: TARGET.into(),
                holder: LeaseHolder::Ask { ask_id, attempt: 1 },
                native_message_id: "native-original".into(),
                runtime_generation: 1,
                turn_id: Some("turn-original".into()),
                since: now_rfc3339(now),
            };
            state.leases.insert(TARGET.into(), lease.clone());
            lease
        })
        .unwrap();
    let runtime = FakeRuntime::idle(&[TARGET]);
    runtime.set(TARGET, |status| {
        status.active_turn_id = lease.turn_id.clone()
    });
    runtime
        .turns
        .borrow_mut()
        .insert("turn-original".into(), history(&lease));
    (store, dir, runtime, lease, now)
}

#[test]
fn start_before_bind_is_recovered_from_exact_native_input_without_resubmission() {
    let (world, world_dir) = world("start-before-bind");
    let (store, dir) = temp_store("start-before-bind");
    let now = Utc::now();
    let ask_id = store
        .with_state(|state| {
            state
                .create_ask(new_ask("early-start", "input"), now)
                .unwrap()
                .0
                .ask_id
        })
        .unwrap();
    let runtime = FakeRuntime::idle(&[TARGET]);
    let observer = AskStore::at(dir.clone());
    *runtime.submit_hook.borrow_mut() = Some(Box::new(move || {
        let lease = observer.snapshot().unwrap().leases[TARGET].clone();
        assert!(
            lease.turn_id.is_none(),
            "reproduce notification before receipt binding"
        );
        crate::asks::observe::observe(
            &observer,
            TARGET,
            "turn/started",
            &json!({"turn":{"id":"turn-1"}}),
            now,
        )
        .unwrap();
    }));
    tick(&world, &runtime, &store, now).unwrap();
    let before = store.snapshot().unwrap();
    assert_eq!(before.asks[&ask_id].execution, ExecutionState::Admitted);
    assert!(before.asks[&ask_id]
        .current_attempt()
        .unwrap()
        .started_at
        .is_none());
    let lease = before.leases[TARGET].clone();
    runtime
        .turns
        .borrow_mut()
        .insert("turn-1".into(), history(&lease));
    reconcile_bound(
        &runtime,
        &store,
        &lease,
        &mut TickReport::default(),
        now + chrono::Duration::seconds(1),
    )
    .unwrap();
    let after = store.snapshot().unwrap();
    let ask = &after.asks[&ask_id];
    assert_eq!(ask.execution, ExecutionState::Running);
    assert_eq!(ask.current_attempt().unwrap().state, AttemptState::Running);
    assert!(ask.current_attempt().unwrap().started_at.is_some());
    assert!(ask.current_attempt().unwrap().started_at_inferred);
    assert_eq!(
        crate::asks::view::attempt_summary(ask.current_attempt().unwrap())["startedAtInferred"],
        true
    );
    assert_eq!(runtime.submitted.borrow().len(), 1);
    // The normal ledger writer/drift path must now heal a timed-out row.
    use crate::management::v2::input_ledger::{Evidence, Expect, InputLedger, InputState, Next};
    let ledger = InputLedger::open(dir.join("catchup-ledger.sqlite"));
    for job in crate::asks::ledger::store_jobs(
        &AskState::default(),
        &before,
        &crate::asks::store::ask_state_changes(&AskState::default(), &before).unwrap(),
    ) {
        crate::asks::ledger::apply_job(&ledger, &job);
    }
    let row = ledger
        .get(TARGET, &[ask_id.clone()])
        .unwrap()
        .records
        .pop()
        .unwrap();
    ledger
        .transition(
            TARGET,
            &ask_id,
            Expect::rev(row.rev),
            Next::uncertain("outcomeUnknown"),
        )
        .unwrap();
    let row = ledger
        .get(TARGET, &[ask_id.clone()])
        .unwrap()
        .records
        .pop()
        .unwrap();
    crate::asks::ledger::apply_job(
        &ledger,
        &crate::asks::ledger::drift_job(&after, &row).unwrap(),
    );
    let healed = ledger
        .get(TARGET, &[ask_id.clone()])
        .unwrap()
        .records
        .pop()
        .unwrap();
    assert_eq!(healed.state, InputState::InContext);
    assert_eq!(healed.evidence, Some(Evidence::TurnStartInput));
    assert_eq!(
        healed.timeline_item_id.as_ref(),
        Some(&lease.native_message_id)
    );
    // Owner disposal must win even if a delayed positive source job arrives.
    let disposed = InputLedger::open(dir.join("disposed/catchup-ledger.sqlite"));
    for job in crate::asks::ledger::store_jobs(
        &AskState::default(),
        &before,
        &crate::asks::store::ask_state_changes(&AskState::default(), &before).unwrap(),
    ) {
        crate::asks::ledger::apply_job(&disposed, &job);
    }
    let observed = disposed
        .get(TARGET, &[ask_id.clone()])
        .unwrap()
        .records
        .pop()
        .unwrap();
    let uncertain = disposed
        .transition(
            TARGET,
            &ask_id,
            Expect::rev(observed.rev),
            Next::uncertain("outcomeUnknown"),
        )
        .unwrap()
        .record;
    let late = crate::asks::ledger::drift_job(&after, &uncertain).unwrap();
    disposed
        .transition(
            TARGET,
            &ask_id,
            Expect::rev(uncertain.rev),
            Next::discarded("ownerDiscarded"),
        )
        .unwrap();
    crate::asks::ledger::apply_job(&disposed, &late);
    assert_eq!(
        disposed.get(TARGET, &[ask_id]).unwrap().records[0].state,
        InputState::Discarded
    );
    drop(disposed);
    drop(ledger);
    std::fs::remove_dir_all(dir).unwrap();
    if world_dir.exists() {
        std::fs::remove_dir_all(world_dir).unwrap();
    }
}

#[test]
fn active_turn_and_unrelated_tools_are_not_ask_start_evidence() {
    for wrong in [
        "turn", "message", "ask", "attempt", "schema", "status", "type",
    ] {
        let (store, dir, runtime, lease, now) = admitted(wrong);
        let mut turn = history(&lease);
        match wrong {
            "turn" => turn["id"] = json!("other"),
            "message" => turn["items"][0]["id"] = json!("other"),
            "ask" => turn["items"][0]["externalInputView"]["data"]["askId"] = json!("other"),
            "attempt" => turn["items"][0]["externalInputView"]["data"]["attempt"] = json!(2),
            "schema" => turn["items"][0]["externalInputView"]["schema"] = json!("other"),
            "status" => turn["status"] = json!("completed"),
            _ => turn["items"][0]["type"] = json!("mcpToolCall"),
        }
        runtime
            .turns
            .borrow_mut()
            .insert("turn-original".into(), turn);
        reconcile_bound(&runtime, &store, &lease, &mut TickReport::default(), now).unwrap();
        assert_eq!(
            store
                .snapshot()
                .unwrap()
                .asks
                .values()
                .next()
                .unwrap()
                .execution,
            ExecutionState::Admitted,
            "{wrong}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn catchup_rechecks_lease_occurrence_stop_and_attempt_after_native_read() {
    for changed in [
        "lease",
        "occurrence",
        "ask-occurrence",
        "native",
        "generation",
        "attempt",
        "stop",
        "closed",
    ] {
        let (store, dir, runtime, lease, now) = admitted(changed);
        let writer = AskStore::at(dir.clone());
        let LeaseHolder::Ask { ask_id, .. } = lease.holder.clone() else {
            panic!()
        };
        let id = ask_id.clone();
        *runtime.read_turn_hook.borrow_mut() = Some(Box::new(move || {
            // A regression retaining the read lock must fail rather than hang.
            let lock = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(writer.dir().join("asks-v1.lock"))
                .unwrap();
            lock.try_lock()
                .expect("history IO must be outside the source lock");
            drop(lock);
            writer
                .with_state(|state| {
                    if changed == "lease" {
                        state.leases.get_mut(TARGET).unwrap().since.push('x');
                        return;
                    }
                    state.update_ask(&id, now, |ask| {
                        if changed == "ask-occurrence" {
                            ask.admission_occurrence += 1;
                            return;
                        }
                        let record = ask.current_attempt_mut().unwrap();
                        match changed {
                            "occurrence" => record.admission_occurrence += 1,
                            "native" => record.native_message_id = "new-native".into(),
                            "generation" => record.runtime_generation += 1,
                            "attempt" => record.n += 1,
                            "stop" => record.stop_requested = true,
                            _ => ask.execution = ExecutionState::Cancelled,
                        }
                    });
                })
                .unwrap();
        }));
        reconcile_bound(&runtime, &store, &lease, &mut TickReport::default(), now).unwrap();
        assert!(
            store.snapshot().unwrap().asks[&ask_id]
                .current_attempt()
                .unwrap()
                .started_at
                .is_none(),
            "{changed}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn unreadable_active_start_has_backoff_and_running_attempt_needs_no_history() {
    let (store, dir, runtime, lease, now) = admitted("start-backoff");
    runtime.turns.borrow_mut().clear();
    reconcile_bound(&runtime, &store, &lease, &mut TickReport::default(), now).unwrap();
    *runtime.read_turn_hook.borrow_mut() =
        Some(Box::new(|| panic!("read repeated before backoff")));
    reconcile_bound(
        &runtime,
        &store,
        &lease,
        &mut TickReport::default(),
        now + chrono::Duration::seconds(1),
    )
    .unwrap();
    runtime.read_turn_hook.borrow_mut().take();
    runtime
        .turns
        .borrow_mut()
        .insert("turn-original".into(), history(&lease));
    reconcile_bound(
        &runtime,
        &store,
        &lease,
        &mut TickReport::default(),
        now + chrono::Duration::seconds(5),
    )
    .unwrap();
    *runtime.read_turn_hook.borrow_mut() =
        Some(Box::new(|| panic!("running attempt does not need history")));
    reconcile_bound(
        &runtime,
        &store,
        &lease,
        &mut TickReport::default(),
        now + chrono::Duration::seconds(10),
    )
    .unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn runtime_generation_or_turn_change_during_read_blocks_catchup() {
    for changed in ["generation", "turn", "offline"] {
        let (store, dir, runtime, lease, now) = admitted(changed);
        let runtime = std::rc::Rc::new(runtime);
        let updating = std::rc::Rc::downgrade(&runtime);
        *runtime.read_turn_hook.borrow_mut() = Some(Box::new(move || {
            updating
                .upgrade()
                .unwrap()
                .set(TARGET, |status| match changed {
                    "generation" => status.runtime_generation += 1,
                    "turn" => status.active_turn_id = Some("replacement-turn".into()),
                    _ => status.online = false,
                });
        }));
        reconcile_bound(
            runtime.as_ref(),
            &store,
            &lease,
            &mut TickReport::default(),
            now,
        )
        .unwrap();
        assert!(store
            .snapshot()
            .unwrap()
            .asks
            .values()
            .next()
            .unwrap()
            .current_attempt()
            .unwrap()
            .started_at
            .is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn activity_while_history_is_read_is_preserved_and_does_not_starve_catchup() {
    let (store, dir, runtime, lease, now) = admitted("concurrent-activity");
    let writer = AskStore::at(dir.clone());
    let LeaseHolder::Ask { ask_id, .. } = lease.holder.clone() else {
        panic!()
    };
    let id = ask_id.clone();
    *runtime.read_turn_hook.borrow_mut() = Some(Box::new(move || {
        writer
            .with_state(|state| {
                state.update_ask(&id, now, |ask| {
                    ask.current_attempt_mut().unwrap().activity.tool_calls = 89;
                });
            })
            .unwrap();
    }));
    reconcile_bound(&runtime, &store, &lease, &mut TickReport::default(), now).unwrap();
    let state = store.snapshot().unwrap();
    assert_eq!(state.asks[&ask_id].execution, ExecutionState::Running);
    assert_eq!(
        state.asks[&ask_id]
            .current_attempt()
            .unwrap()
            .activity
            .tool_calls,
        89
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn catchup_timestamp_provenance_survives_late_start_and_does_not_replace_known_time() {
    for preexisting in [false, true] {
        let (store, dir, runtime, lease, now) = admitted("start-time-provenance");
        let LeaseHolder::Ask { ask_id, .. } = &lease.holder else {
            panic!()
        };
        let earlier = now_rfc3339(now - chrono::Duration::seconds(10));
        if preexisting {
            store
                .with_state(|state| {
                    state.update_ask(ask_id, now, |ask| {
                        ask.current_attempt_mut().unwrap().started_at = Some(earlier.clone());
                    });
                })
                .unwrap();
        }
        reconcile_bound(&runtime, &store, &lease, &mut TickReport::default(), now).unwrap();
        crate::asks::observe::observe(
            &store,
            TARGET,
            "turn/started",
            &json!({"turn":{"id":lease.turn_id}}),
            now + chrono::Duration::seconds(2),
        )
        .unwrap();
        let state = store.snapshot().unwrap();
        let attempt = state.asks[ask_id].current_attempt().unwrap();
        assert_eq!(
            attempt.started_at.as_deref(),
            Some(if preexisting {
                earlier.as_str()
            } else {
                lease.since.as_str()
            })
        );
        assert_eq!(attempt.started_at_inferred, !preexisting);
        let reloaded: AttemptRecord =
            serde_json::from_value(serde_json::to_value(attempt).unwrap()).unwrap();
        assert_eq!(reloaded.started_at_inferred, !preexisting);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
