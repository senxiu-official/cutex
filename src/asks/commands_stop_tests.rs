//! Dispatch regressions use the real durable AskStore and command entrypoint.
use super::*;
use crate::asks::scheduler::{NativeMessageStatus, Submission, SubmitOutcome, TargetStatus};
use crate::asks::scheduler::tests::{world, FakeRuntime};
use crate::asks::store::tests::{new_ask, temp_store, TARGET};
use std::{cell::Cell, path::PathBuf, rc::Rc};

struct StopRuntime {
    inner: Rc<FakeRuntime>,
    calls: Cell<usize>,
    lose_response: bool,
    claim_check: Option<Box<dyn Fn()>>,
}
impl StopRuntime {
    fn new() -> Self {
        let inner = Rc::new(FakeRuntime::idle(&[TARGET]));
        inner.set(TARGET, |s| s.active_turn_id = Some("original-turn".into()));
        Self { inner, calls: Cell::new(0), lose_response: false, claim_check: None }
    }
}
impl AskRuntime for StopRuntime {
    fn target_status(&self, session: &str) -> TargetStatus { self.inner.target_status(session) }
    fn submit_exclusive(&self, session: &str, generation: u64, submission: &Submission) -> SubmitOutcome {
        self.inner.submit_exclusive(session, generation, submission)
    }
    fn message_status(&self, session: &str, generation: u64, submission: &Submission) -> NativeMessageStatus {
        self.inner.message_status(session, generation, submission)
    }
    fn interrupt_keep_gate(&self, _: &str, _: &str) -> anyhow::Result<()> { panic!("unbound legacy interrupt must never run") }
    fn interrupt_exact(&self, session: &str, generation: u64, turn: &str) -> anyhow::Result<()> {
        self.calls.set(self.calls.get() + 1);
        if let Some(check) = &self.claim_check { check(); }
        self.inner.interrupt_exact(session, generation, turn)?;
        if self.lose_response { anyhow::bail!("response lost after effect"); }
        Ok(())
    }
    fn resume_gate(&self, session: &str) -> anyhow::Result<()> { self.inner.resume_gate(session) }
    fn read_turn(&self, session: &str, turn: &str) -> Option<serde_json::Value> { self.inner.read_turn(session, turn) }
    fn respond_question(&self, session: &str, generation: u64, response_id: &str,
        request_id: &serde_json::Value, result: &serde_json::Value) -> RespondOutcome {
        self.inner.respond_question(session,generation,response_id,request_id,result)
    }
}

fn fixture(label: &str) -> (AskStore, PathBuf, String, RuntimeCommand) {
    let (store, dir) = temp_store(label);
    let now = Utc::now();
    let id = store.with_state(|state| {
        let mut new = new_ask("stop", "fixture");
        new.project_id = None;
        let id = state.create_ask(new, now).unwrap().0.ask_id;
        let mut attempt = AttemptRecord::submitting(1, "original-native-key".into(), 1, now.to_rfc3339());
        attempt.state = AttemptState::Running;
        attempt.turn_id = Some("original-turn".into());
        let ask = state.asks.get_mut(&id).unwrap();
        ask.execution = ExecutionState::Running;
        ask.attempts.push(attempt);
        state.leases.insert(TARGET.into(), AdmissionLease {
            cutex_session_id: TARGET.into(), holder: LeaseHolder::Ask { ask_id: id.clone(), attempt: 1 },
            native_message_id: "original-native-key".into(), runtime_generation: 1,
            turn_id: Some("original-turn".into()), since: now.to_rfc3339(),
        });
        assert!(capture_stop_intent(state, &id, 1, now));
        crate::asks::control::queue_command(state, RuntimeCommand::StopAttempt { ask_id: id.clone(), attempt: 1 }, now);
        id
    }).unwrap();
    let command = RuntimeCommand::StopAttempt { ask_id: id.clone(), attempt: 1 };
    (store, dir, id, command)
}
fn intent(store: &AskStore, id: &str) -> AttemptStopIntent {
    store.inspect(|s| s.asks[id].current_attempt().unwrap().stop_intent.clone().unwrap()).unwrap()
}

#[test]
fn stop_unknown_active_turn_preserves_pending_until_exact_occurrence_is_known() {
    let (store, dir, id, command) = fixture("stop-unknown-active");
    let runtime = StopRuntime::new();
    runtime.inner.set(TARGET, |s| s.active_turn_id = None);
    let before = intent(&store, &id);
    assert!(!run(&runtime, &store, &command, Utc::now()).unwrap());
    assert_eq!(intent(&store, &id), before);
    assert!(store.inspect(|s|s.asks[&id].current_attempt().unwrap().stop_requested).unwrap());
    assert_eq!(runtime.calls.get(), 0);
    runtime.inner.set(TARGET, |s| s.active_turn_id = Some("original-turn".into()));
    assert!(run(&runtime, &store, &command, Utc::now()).unwrap());
    assert_eq!(runtime.calls.get(), 1);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn stop_replacement_generation_reusing_turn_is_never_interrupted() {
    let (store, dir, id, command) = fixture("stop-replacement-same-turn");
    let runtime = StopRuntime::new();
    runtime.inner.set(TARGET, |s| s.runtime_generation = 2);
    assert!(run(&runtime, &store, &command, Utc::now()).unwrap());
    assert_eq!(runtime.calls.get(), 0);
    assert!(runtime.inner.interrupts.borrow().is_empty());
    assert_eq!(intent(&store, &id).outcome, StopDispatchOutcome::Stale);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn stop_restart_after_snapshot_is_rejected_at_exact_rpc_and_not_retried() {
    let (store, dir, id, command) = fixture("stop-restart-before-rpc");
    let runtime = StopRuntime::new();
    let replaced = runtime.inner.clone();
    *runtime.inner.status_hook.borrow_mut() = Some(Box::new(move || replaced.set(TARGET, |s| s.runtime_generation = 2)));
    assert!(run(&runtime, &store, &command, Utc::now()).unwrap());
    assert_eq!(runtime.calls.get(), 1);
    assert!(runtime.inner.interrupts.borrow().is_empty());
    assert_eq!(intent(&store, &id).outcome, StopDispatchOutcome::Unknown);
    runtime.inner.set(TARGET, |s| s.runtime_generation = 1);
    assert!(run(&runtime, &AskStore::at(&dir), &command, Utc::now()).unwrap());
    assert_eq!(runtime.calls.get(), 1);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn stop_response_loss_has_durable_claim_before_effect_and_next_tick_never_replays() {
    let (store, dir, id, command) = fixture("stop-response-loss");
    let mut runtime = StopRuntime::new();
    runtime.lose_response = true;
    let reopened = AskStore::at(&dir);
    let checked_id = id.clone();
    let (begin_tx,begin_rx)=std::sync::mpsc::channel();
    let (held_tx,held_rx)=std::sync::mpsc::channel();
    let (release_tx,release_rx)=std::sync::mpsc::channel();
    let lifecycle=std::thread::spawn(move|| {
        let timeout=std::time::Duration::from_secs(5);
        begin_rx.recv_timeout(timeout).unwrap();
        let roster=crate::agent_management::AgentManagementStore::open_default().unwrap();
        let _fence=roster.try_lock_delivery_mutations().unwrap().expect("RPC must release authority");
        held_tx.send(()).unwrap();
        release_rx.recv_timeout(timeout).unwrap();
    });
    runtime.claim_check = Some(Box::new(move || {
        // This real RPC boundary must own neither AskStore nor authority.
        let checked=crate::asks::pause_admission::try_with_authority(|_| {
            let claim=intent(&reopened,&checked_id);
            assert!(claim.claimed_at.is_some());
            assert_eq!(claim.outcome,StopDispatchOutcome::Claimed);
            Ok(())
        }).unwrap();
        assert!(matches!(checked,crate::asks::pause_admission::Admission::Ready(())));
        begin_tx.send(()).unwrap();
        held_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    }));
    let (world, world_dir) = world("stop-response-loss");
    store.with_state(|s|s.notices.clear()).unwrap();
    crate::asks::scheduler::run_commands(&runtime,&store,Utc::now()).unwrap();
    // Result persistence is held, not an excuse to replay the native effect.
    assert!(!store.inspect(|s|s.commands.is_empty()).unwrap());
    assert_eq!(intent(&store,&id).outcome,StopDispatchOutcome::Claimed);
    assert_eq!(runtime.inner.interrupts.borrow().len(),1);
    release_tx.send(()).unwrap();
    lifecycle.join().unwrap();
    crate::asks::scheduler::tick(&world, &runtime, &AskStore::at(&dir), Utc::now()).unwrap();
    assert!(store.inspect(|s|s.commands.is_empty()).unwrap());
    assert_eq!(intent(&store,&id).outcome,StopDispatchOutcome::Unknown);
    let updates=store.inspect(|s|s.notices.iter().filter(|notice|notice.method=="cutex/ask/updated"
        && notice.params["askId"]==id && notice.params["changed"]==json!(["stop"]))
        .map(|notice|notice.params.clone()).collect::<Vec<_>>()).unwrap();
    assert!(updates.len()>=2,"both claim and outcome must invalidate the projection");
    for params in updates {
        let keys=params.as_object().unwrap();
        assert_eq!(keys.len(),3);
        assert!(keys.contains_key("askId")&&keys.contains_key("revision")&&keys.contains_key("changed"));
        assert_eq!(params["changed"],json!(["stop"]));
        let wire=params.to_string();
        for private in ["original-turn","original-native-key","runtimeGeneration","stop_outcome_unknown","response lost"] {
            assert!(!wire.contains(private),"stop invalidation leaked details: {private}");
        }
    }
    assert!(run(&runtime, &AskStore::at(&dir), &command, Utc::now()).unwrap());
    assert_eq!(runtime.calls.get(), 1);
    assert_eq!(runtime.inner.interrupts.borrow().len(), 1);
    let original = intent(&store, &id);
    store.with_state(|s| assert!(capture_stop_intent(s, &id, 1, Utc::now()))).unwrap();
    assert_eq!(intent(&store, &id), original);
    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(world_dir);
}

#[test]
fn stop_claim_survives_crash_before_rpc_and_offline_restart() {
    let (store, dir, id, command) = fixture("stop-claim-before-rpc");
    store.with_state(|s| {
        let record = s.asks.get_mut(&id).unwrap().current_attempt_mut().unwrap();
        let claim = record.stop_intent.as_mut().unwrap();
        claim.claimed_at = Some(Utc::now().to_rfc3339());
        claim.outcome = StopDispatchOutcome::Claimed;
    }).unwrap();
    let runtime = StopRuntime::new();
    runtime.inner.set(TARGET, |s| s.online = false);
    assert!(run(&runtime, &AskStore::at(&dir), &command, Utc::now()).unwrap());
    assert_eq!(runtime.calls.get(), 0);
    assert!(runtime.inner.target_probes.borrow().is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn stop_revalidates_lease_after_status_and_never_stops_another_ask() {
    let (store, dir, id, command) = fixture("stop-final-lease");
    let runtime = StopRuntime::new();
    let other = AskStore::at(&dir);
    *runtime.inner.status_hook.borrow_mut() = Some(Box::new(move || {
        other.with_state(|s| s.leases.get_mut(TARGET).unwrap().holder = LeaseHolder::Ask { ask_id: "replacement-ask".into(), attempt: 1 }).unwrap();
    }));
    assert!(run(&runtime, &store, &command, Utc::now()).unwrap());
    assert_eq!(runtime.calls.get(), 0);
    assert!(intent(&store, &id).claimed_at.is_none());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn stop_receipt_pending_binds_only_original_attempt_and_native_key() {
    let (store, dir, id, command) = fixture("stop-late-receipt");
    store.with_state(|s| {
        let record = s.asks.get_mut(&id).unwrap().current_attempt_mut().unwrap();
        record.turn_id = None;
        record.stop_intent.as_mut().unwrap().turn_id = None;
        s.leases.get_mut(TARGET).unwrap().turn_id = None;
    }).unwrap();
    let runtime = StopRuntime::new();
    assert!(!run(&runtime, &store, &command, Utc::now()).unwrap());
    assert!(runtime.inner.target_probes.borrow().is_empty());
    store.with_state(|s| crate::asks::scheduler::bind_receipt(s, TARGET, &id, 1, "original-turn", Utc::now())).unwrap();
    assert!(run(&runtime, &store, &command, Utc::now()).unwrap());
    let claim = intent(&store, &id);
    assert_eq!(claim.turn_id.as_deref(), Some("original-turn"));
    assert_eq!(claim.native_message_id, "original-native-key");
    assert_eq!(claim.outcome, StopDispatchOutcome::Submitted);
    assert_eq!(runtime.calls.get(), 1);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn stop_lifecycle_contention_is_deferred_without_probe_claim_or_dispatch() {
    let (store, dir, id, command) = fixture("stop-lifecycle-barrier");
    let lane = crate::management::v2::session_coordination::session_lane(TARGET).unwrap();
    let lifecycle = lane.lock().unwrap();
    let before = intent(&store, &id);
    let runtime = StopRuntime::new();
    assert!(!run(&runtime, &store, &command, Utc::now()).unwrap());
    assert_eq!(intent(&store, &id), before);
    assert!(runtime.inner.target_probes.borrow().is_empty());
    assert_eq!(runtime.calls.get(), 0);
    drop(lifecycle);
    let held=crate::asks::pause_admission::try_with_authority(|_| {
        assert!(!run(&runtime,&store,&command,Utc::now()).unwrap());
        assert_eq!(intent(&store,&id),before);
        assert!(runtime.inner.target_probes.borrow().is_empty());
        assert_eq!(runtime.calls.get(),0);
        Ok(())
    }).unwrap();
    assert!(matches!(held,crate::asks::pause_admission::Admission::Ready(())));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn stop_is_maintenance_and_remains_available_while_agent_paused() {
    let (store, dir, id, command) = fixture("stop-while-paused");
    store.with_state(|s| crate::asks::agent_pause::pause(s,TARGET,"owner",None,Utc::now())).unwrap();
    let runtime = StopRuntime::new();
    assert!(run(&runtime,&store,&command,Utc::now()).unwrap());
    assert_eq!(runtime.calls.get(),1);
    assert_eq!(intent(&store,&id).outcome,StopDispatchOutcome::Submitted);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn attempt_stop_intent_is_additive_for_old_records() {
    let mut wire=serde_json::to_value(AttemptRecord::submitting(1,"key".into(),4,Utc::now().to_rfc3339())).unwrap();
    wire.as_object_mut().unwrap().remove("stopIntent");
    let decoded:AttemptRecord=serde_json::from_value(wire).unwrap();
    assert!(decoded.stop_intent.is_none());
}

#[test]
fn legacy_stop_without_original_intent_is_explicitly_unsupported() {
    let (store, dir, id, command) = fixture("stop-no-original-intent");
    store.with_state(|s| s.asks.get_mut(&id).unwrap().current_attempt_mut().unwrap().stop_intent = None).unwrap();
    let runtime = StopRuntime::new();
    assert!(run(&runtime, &store, &command, Utc::now()).unwrap());
    assert_eq!(runtime.calls.get(), 0);
    assert!(runtime.inner.target_probes.borrow().is_empty());
    assert!(store.inspect(|s| s.asks[&id].current_attempt().unwrap().last_error.as_deref().unwrap().starts_with("stop_unsupported")).unwrap());
    let _ = std::fs::remove_dir_all(dir);
}
