use super::*;
use crate::asks::scheduler::tests::{world, FakeRuntime, FakeWorld};
use crate::asks::store::tests::{new_ask, temp_store, TARGET};
use crate::asks::{awaits, observe, scheduler};

#[test]
fn verified_delegation_child_finished_before_declaration_wakes_once(){
    let f=Fixture::new("bounded-child-before");
    let child=f.store.with_state(|state|{
        let mut new=new_ask("child-before","review");
        new.requester=model::Requester::Agent{cutex_session_id:TARGET.into(),name:None};
        new.target_cutex_session_id=crate::asks::store::tests::REQUESTER.into();
        let id=state.create_ask(new,f.now).unwrap().0.ask_id;
        state.update_ask(&id,f.now,|ask|{
            ask.execution=ExecutionState::Cancelled;
            ask.closed=Some(model::CloseOutcome::Stopped);
            ask.closed_at=Some(crate::asks::now_rfc3339(f.now));
        });
        id
    }).unwrap();
    let mut request=f.request(1);
    request.wake=Some(WakeCondition::Children{children:vec![ChildRef{kind:ChildKind::Delegation,id:child}],until:Until::All});
    let result=f.store.with_state(|state|declare_current(state,TARGET,&f.ask,&request,
        Guards{online:true,permission_valid:true,occurrence_valid:true,..Default::default()},f.now,None)).unwrap().unwrap();
    assert_eq!(result["accepted"],true);
    f.finish(1,f.now,"completed");
    f.tick(61);f.tick(62);
    assert_eq!(f.runtime.submitted.borrow().len(),2);
    assert_eq!(f.store.snapshot().unwrap().asks[&f.ask].attempts.len(),2);
}

struct Fixture {
    world: FakeWorld,
    runtime: FakeRuntime,
    store: AskStore,
    ask: String,
    now: DateTime<Utc>,
    dirs: Vec<std::path::PathBuf>,
}

#[test]
fn delegation_child_terminal_after_stage_end_wakes_only_after_authenticated_completion(){
    let f=Fixture::new("bounded-child-after");
    // Keep the child target online so offline/needs-owner notifications do
    // not independently wake the parent while this condition is under test.
    f.runtime.set(crate::asks::store::tests::REQUESTER,|status|{
        status.online=true;status.runtime_generation=1;status.exclusive_admission=true;
        status.active_turn_id=Some("unrelated-child-target-turn".into());
    });
    let child=f.store.with_state(|state|{
        // Hold the child on its own target so this test controls its terminal
        // fact; an offline target may otherwise fail through normal scheduling.
        crate::asks::agent_pause::pause(state,crate::asks::store::tests::REQUESTER,"owner",None,f.now);
        let mut new=new_ask("child-after","review");
        // State notices are independently valid wake sources. Keep them
        // passive here to isolate the declared child-completion condition.
        new.options.notify=model::Notify::Passive;
        new.requester=model::Requester::Agent{cutex_session_id:TARGET.into(),name:None};
        new.target_cutex_session_id=crate::asks::store::tests::REQUESTER.into();
        state.create_ask(new,f.now).unwrap().0.ask_id
    }).unwrap();
    let mut request=f.request(1);
    request.wake=Some(WakeCondition::Children{children:vec![ChildRef{kind:ChildKind::Delegation,id:child.clone()}],until:Until::All});
    f.store.with_state(|state|declare_current(state,TARGET,&f.ask,&request,
        Guards{online:true,permission_valid:true,occurrence_valid:true,..Default::default()},f.now,None)).unwrap().unwrap();
    f.finish(1,f.now,"completed");
    f.tick(61);
    assert_eq!(f.runtime.submitted.borrow().len(),1);
    assert!(!f.store.snapshot().unwrap().asks[&child].execution.is_terminal());
    f.store.with_state(|state|{
        state.update_ask(&child,f.now+chrono::Duration::seconds(62),|ask|{
            ask.execution=ExecutionState::Cancelled;ask.closed=Some(model::CloseOutcome::Stopped);
            ask.closed_at=Some(crate::asks::now_rfc3339(f.now+chrono::Duration::seconds(62)));
        });
    }).unwrap();
    f.tick(63);f.tick(64);
    assert_eq!(f.runtime.submitted.borrow().len(),2);
}

#[test]
fn child_wait_rejects_stale_generation_even_with_a_matching_parent_string(){
    let f=Fixture::new("bounded-child-stale");
    f.store.with_state(|state|{
        let mut new=new_ask("child-stale","review");
        new.requester=model::Requester::Agent{cutex_session_id:TARGET.into(),name:None};
        new.target_cutex_session_id=crate::asks::store::tests::REQUESTER.into();
        let child=state.create_ask(new,f.now).unwrap().0.ask_id;
        state.asks.get_mut(&child).unwrap().source_lineage.as_mut().unwrap().runtime_generation+=1;
        let mut request=f.request(1);
        request.wake=Some(WakeCondition::Children{children:vec![ChildRef{kind:ChildKind::Delegation,id:child}],until:Until::All});
        assert!(declare_current(state,TARGET,&f.ask,&request,
            Guards{online:true,permission_valid:true,occurrence_valid:true,..Default::default()},f.now,None).is_err());
        assert!(state.asks[&f.ask].bounded_continuation.current.is_none());
    }).unwrap();
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for dir in &self.dirs {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
impl Fixture {
    fn new(tag: &str) -> Self {
        let (world, world_dir) = world(tag);
        let (store, store_dir) = temp_store(tag);
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        let ask = store
            .with_state(|state| {
                state
                    .create_ask(new_ask(tag, "implement and verify"), now)
                    .unwrap()
                    .0
                    .ask_id
            })
            .unwrap();
        scheduler::tick(&world, &runtime, &store, now).unwrap();
        assert_eq!(runtime.submitted.borrow().len(), 1);
        Self {
            world,
            runtime,
            store,
            ask,
            now,
            dirs: vec![world_dir, store_dir],
        }
    }
    fn request(&self, n: u32) -> DeclareRequest {
        DeclareRequest {
            action_id: format!("continue-{n}"),
            expected_attempt_id: format!("{}#{n}", self.ask),
            expected_revision: None,
            note: "Integrate the remaining work".into(),
            wake: None,
        }
    }
    fn declare(&self, n: u32) -> Value {
        self.store
            .with_state(|state| {
                declare_current(
                    state,
                    TARGET,
                    &self.ask,
                    &self.request(n),
                    Guards {
                        online: true,
                        permission_valid: true,
                        occurrence_valid: true,
                        ..Guards::default()
                    },
                    self.now,
                    None,
                )
            })
            .unwrap()
            .unwrap()
    }
    fn finish(&self, n: u32, at: DateTime<Utc>, status: &str) {
        let turn = format!("turn-{n}");
        observe::observe(&self.store, TARGET, "turn/completed", &json!({"threadId":"t", "turn": {
            "id": turn, "status": status, "items": [{"type":"agentMessage", "id":format!("final-{n}"),
                "phase":"final_answer", "text":format!("full stage from turn {n}")}], "error":null }}), at).unwrap();
        self.runtime
            .set(TARGET, |status| status.active_turn_id = None);
    }
    fn tick(&self, seconds: i64) {
        scheduler::tick(
            &self.world,
            &self.runtime,
            &self.store,
            self.now + Duration::seconds(seconds),
        )
        .unwrap();
    }
}

#[test]
fn actual_observer_stages_then_existing_scheduler_admits_once_and_next_turn_can_answer() {
    let f = Fixture::new("bounded-runtime-normal");
    assert_eq!(f.declare(1)["continuation"]["roundNumber"], 1);
    f.finish(1, f.now, "completed");
    f.store
        .inspect(|state| {
            let ask = &state.asks[&f.ask];
            assert_eq!(ask.execution, ExecutionState::Running);
            assert!(ask.closed.is_none() && ask.answer_id.is_none());
            assert!(state.answers.is_empty() && state.deliveries.is_empty());
            assert!(!state.leases.contains_key(TARGET));
            assert_eq!(ask.attempts[0].state, AttemptState::Yielded);
            assert_eq!(
                ask.attempts[0].stage_note.as_deref(),
                Some("full stage from turn 1")
            );
        })
        .unwrap();
    f.tick(59);
    assert_eq!(f.runtime.submitted.borrow().len(), 1);
    f.tick(60);
    assert_eq!(f.runtime.submitted.borrow().len(), 2);
    f.tick(61);
    assert_eq!(f.runtime.submitted.borrow().len(), 2);
    f.store
        .inspect(|state| {
            let ask = &state.asks[&f.ask];
            assert_eq!(ask.attempts.len(), 2);
            assert_eq!(
                ask.bounded_continuation
                    .current
                    .as_ref()
                    .unwrap()
                    .round_number,
                2
            );
            assert!(ask.attempts[1]
                .continuation
                .as_ref()
                .unwrap()
                .bounded_wake_id
                .is_some());
        })
        .unwrap();
    f.finish(2, f.now + Duration::seconds(70), "completed");
    f.store
        .inspect(|state| {
            assert_eq!(state.asks[&f.ask].execution, ExecutionState::Answered);
            assert_eq!(state.answers.len(), 1);
            assert_eq!(state.deliveries.len(), 1);
            assert_eq!(state.asks[&f.ask].bounded_continuation.stages.len(), 1);
        })
        .unwrap();
}

#[test]
fn application_default_answer_and_abnormal_declaration_end_remain_compatible() {
    for (tag, declare, status) in [
        ("bounded-runtime-default", false, "completed"),
        ("bounded-runtime-interrupt", true, "interrupted"),
    ] {
        let f = Fixture::new(tag);
        if declare {
            f.declare(1);
        }
        f.finish(1, f.now, status);
        f.tick(60);
        f.store
            .inspect(|state| {
                assert_eq!(state.answers.len(), 1);
                assert!(state.asks[&f.ask].bounded_continuation.stages.is_empty());
                assert!(state.asks[&f.ask].resume.is_none());
            })
            .unwrap();
        assert_eq!(f.runtime.submitted.borrow().len(), 1);
    }
}

#[test]
fn await_conflict_occurs_before_any_job_binding_and_stage_retains_artifact_on_reload() {
    let f = Fixture::new("bounded-runtime-await");
    f.declare(1);
    let err = f
        .store
        .with_state(|state| {
            awaits::register(
                state,
                TARGET,
                &awaits::AwaitRequest {
                    delegation: Some(f.ask.clone()),
                    jobs: Some(vec!["new-job".into()]),
                    note: "waiting".into(),
                    ..Default::default()
                },
                f.now,
            )
        })
        .unwrap()
        .unwrap_err();
    assert_eq!(err.code, "continuation_conflict");
    assert!(f.store.snapshot().unwrap().asks[&f.ask].jobs.is_empty());
    f.finish(1, f.now, "completed");
    let reloaded = AskStore::at(&f.dirs[1]);
    let snapshot = reloaded.snapshot().unwrap();
    let stage = &snapshot.asks[&f.ask].bounded_continuation.stages[0];
    let artifact = stage.final_message_artifact.as_ref().unwrap();
    let (_, bytes) = reloaded.read_artifact(artifact, None).unwrap().unwrap();
    assert_eq!(String::from_utf8(bytes).unwrap(), "full stage from turn 1");
    scheduler::tick(
        &f.world,
        &f.runtime,
        &reloaded,
        f.now + Duration::seconds(60),
    )
    .unwrap();
    scheduler::tick(
        &f.world,
        &f.runtime,
        &reloaded,
        f.now + Duration::seconds(61),
    )
    .unwrap();
    assert_eq!(f.runtime.submitted.borrow().len(), 2);
}

#[test]
fn a_real_pause_and_offline_runtime_hold_the_timer_without_auto_wake() {
    let f = Fixture::new("bounded-runtime-pause");
    f.declare(1);
    f.finish(1, f.now, "completed");
    f.store
        .with_state(|state| {
            crate::asks::agent_pause::pause(state, TARGET, "owner", None, f.now);
        })
        .unwrap();
    f.tick(60);
    assert_eq!(f.runtime.submitted.borrow().len(), 1);
    assert_eq!(
        f.store.snapshot().unwrap().asks[&f.ask]
            .bounded_continuation
            .current
            .as_ref()
            .unwrap()
            .state,
        Phase::Ready
    );
    f.store
        .with_state(|state| {
            crate::asks::agent_pause::resume(state, TARGET);
        })
        .unwrap();
    f.runtime.set(TARGET, |status| status.online = false);
    f.tick(61);
    assert!(f.runtime.wakes.borrow().is_empty());
    assert_eq!(f.runtime.submitted.borrow().len(), 1);
    f.runtime.set(TARGET, |status| status.online = true);
    f.tick(62);
    assert_eq!(f.runtime.submitted.borrow().len(), 2);
}

#[test]
fn uncertain_actual_submit_reconciles_same_message_and_never_resubmits_it() {
    let f = Fixture::new("bounded-runtime-unknown");
    f.declare(1);
    f.finish(1, f.now, "completed");
    f.runtime
        .outcomes
        .borrow_mut()
        .push(scheduler::SubmitOutcome::Uncertain("lost reply".into()));
    f.tick(60);
    assert_eq!(f.runtime.submitted.borrow().len(), 2);
    let sent = f.runtime.submitted.borrow()[1].1.message_id.clone();
    f.runtime
        .native
        .borrow_mut()
        .insert(sent.clone(), scheduler::NativeMessageStatus::Pending);
    f.tick(61);
    f.tick(62);
    assert_eq!(f.runtime.submitted.borrow().len(), 2);
    let ask = f.store.snapshot().unwrap().asks[&f.ask].clone();
    assert_eq!(
        ask.bounded_continuation
            .current
            .as_ref()
            .unwrap()
            .round_number,
        1
    );
    assert!(
        ask.bounded_continuation
            .current
            .as_ref()
            .unwrap()
            .reconciling
    );
    f.runtime.native.borrow_mut().insert(
        sent,
        scheduler::NativeMessageStatus::Persisted {
            turn_id: "turn-2".into(),
        },
    );
    f.runtime.set(TARGET, |status| {
        status.active_turn_id = Some("turn-2".into())
    });
    f.tick(63);
    f.tick(64);
    assert_eq!(f.runtime.submitted.borrow().len(), 2);
    assert_eq!(
        f.store.snapshot().unwrap().asks[&f.ask]
            .bounded_continuation
            .current
            .as_ref()
            .unwrap()
            .round_number,
        2
    );
}

#[test]
fn definite_native_refusal_uses_existing_retry_without_counting_a_round_twice() {
    let f = Fixture::new("bounded-runtime-busy");
    f.declare(1);
    f.finish(1, f.now, "completed");
    f.runtime
        .outcomes
        .borrow_mut()
        .push(scheduler::SubmitOutcome::Busy {
            reason: "active_turn".into(),
        });
    f.tick(60);
    assert_eq!(f.runtime.submitted.borrow().len(), 2);
    assert_eq!(
        f.store.snapshot().unwrap().asks[&f.ask]
            .bounded_continuation
            .current
            .as_ref()
            .unwrap()
            .round_number,
        1
    );
    f.tick(61);
    assert_eq!(f.runtime.submitted.borrow().len(), 3);
    let snapshot = f.store.snapshot().unwrap();
    assert_eq!(snapshot.asks[&f.ask].attempts.len(), 2);
    assert_eq!(
        snapshot.asks[&f.ask]
            .bounded_continuation
            .current
            .as_ref()
            .unwrap()
            .round_number,
        2
    );
}

#[test]
fn normal_turn_end_racing_stop_cannot_yield_or_reenter_scheduler() {
    let f = Fixture::new("bounded-runtime-stop");
    f.declare(1);
    f.store
        .with_state(|state| {
            state.update_ask(&f.ask, f.now, |ask| {
                ask.current_attempt_mut().unwrap().stop_requested = true
            });
        })
        .unwrap();
    f.finish(1, f.now, "completed");
    f.tick(60);
    let snapshot = f.store.snapshot().unwrap();
    assert!(snapshot.asks[&f.ask].bounded_continuation.stages.is_empty());
    assert_eq!(
        snapshot.asks[&f.ask]
            .bounded_continuation
            .current
            .as_ref()
            .unwrap()
            .state,
        Phase::Cancelled
    );
    assert_eq!(f.runtime.submitted.borrow().len(), 1);
}

#[test]
fn real_store_limit_outbox_is_once_and_reset_never_erases_stage_or_starts_a_runtime() {
    let f = Fixture::new("bounded-runtime-notice");
    f.declare(1);
    f.finish(1, f.now, "completed");
    f.store
        .with_state(|state| {
            state.update_ask(&f.ask, f.now, |ask| {
                ask.bounded_continuation
                    .enforce_limits(f.now + Duration::seconds(86_400), Some("current-director"));
            });
            changed_and_notice(
                state,
                &f.ask,
                Some("current-director"),
                f.now + Duration::seconds(86_400),
            );
        })
        .unwrap();
    let reloaded = AskStore::at(&f.dirs[1]);
    reloaded
        .with_state(|state| {
            changed_and_notice(
                state,
                &f.ask,
                Some("next-director"),
                f.now + Duration::seconds(86_401),
            )
        })
        .unwrap();
    let snapshot = reloaded.snapshot().unwrap();
    let notices: Vec<_> = snapshot
        .notices
        .iter()
        .filter(|notice| notice.method == "cutex/delegation/continuationLimited")
        .collect();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].sessions.contains(&"current-director".into()));
    assert!(notices[0]
        .sessions
        .contains(&crate::management::v2::services::HOST_EVENT_SESSION_ID.into()));
    assert_eq!(snapshot.asks[&f.ask].execution, ExecutionState::Running);
    assert!(snapshot.answers.is_empty());
}

#[test]
fn native_children_are_truthfully_refused_by_the_application_entry_point() {
    let f = Fixture::new("bounded-runtime-unsupported");
    let mut request = f.request(1);
    request.wake = Some(WakeCondition::Children {
        children: vec![ChildRef {
            kind: ChildKind::NativeAgent,
            id: "native-thread".into(),
        }],
        until: Until::All,
    });
    let err = f
        .store
        .with_state(|state| {
            declare_current(
                state,
                TARGET,
                &f.ask,
                &request,
                Guards {
                    online: true,
                    permission_valid: true,
                    occurrence_valid: true,
                    ..Default::default()
                },
                f.now,
                None,
            )
        })
        .unwrap()
        .unwrap_err();
    assert_eq!(err.code, "continuation_condition_unsupported");
    assert!(f.store.snapshot().unwrap().asks[&f.ask]
        .bounded_continuation
        .is_empty());
}

#[test]
fn original_declaration_replays_after_the_delegation_genuinely_answers() {
    let f = Fixture::new("bounded-runtime-late-replay");
    f.declare(1);
    f.finish(1, f.now, "completed");
    f.tick(60);
    f.finish(2, f.now + Duration::seconds(70), "completed");
    let before = f.store.snapshot().unwrap();
    assert_eq!(f.declare(1)["replayed"], true);
    assert_eq!(f.store.snapshot().unwrap(), before);
    assert_eq!(f.runtime.submitted.borrow().len(), 2);
}

#[test]
fn a_live_generation_change_rejects_a_stale_leases_declaration() {
    let f = Fixture::new("bounded-runtime-generation");
    f.store
        .with_state(|state| {
            state
                .presence
                .entry(TARGET.into())
                .or_default()
                .runtime_generation = Some(2);
        })
        .unwrap();
    let error = f
        .store
        .with_state(|state| {
            declare_current(
                state,
                TARGET,
                &f.ask,
                &f.request(1),
                Guards {
                    online: true,
                    permission_valid: true,
                    occurrence_valid: true,
                    ..Default::default()
                },
                f.now,
                None,
            )
        })
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code, "continuation_not_current_handler");
    assert!(f.store.snapshot().unwrap().asks[&f.ask]
        .bounded_continuation
        .is_empty());
}

#[test]
fn sticky_project_pause_is_inherited_by_actual_continuation_admission() {
    let f = Fixture::new("bounded-runtime-project-pause");
    f.declare(1);
    f.finish(1, f.now, "completed");
    f.store
        .with_state(|state| {
            state.project_control.projects.insert(
                "alpha".into(),
                crate::asks::project_control::RunControl {
                    project_id: "alpha".into(),
                    epoch: 1,
                    pause_id: Some("pause-1".into()),
                    requested_by: None,
                    requested_at: Some(f.now),
                    mode: crate::asks::project_control::Mode::AfterTurn,
                    scope: crate::asks::project_control::Scope::Members,
                    membership_revision: 1,
                    interrupt_unknown: Default::default(),
                    excluded_session_ids: Default::default(),
                    members: [TARGET.to_string()].into_iter().collect(),
                },
            );
        })
        .unwrap();
    f.tick(60);
    assert_eq!(f.runtime.submitted.borrow().len(), 1);
    assert!(f.store.snapshot().unwrap().asks[&f.ask].resume.is_none());
    f.store
        .with_state(|state| {
            state
                .project_control
                .projects
                .get_mut("alpha")
                .unwrap()
                .pause_id = None;
        })
        .unwrap();
    f.tick(61);
    assert_eq!(f.runtime.submitted.borrow().len(), 2);
}

#[test]
fn archive_cancels_a_persisted_timer_and_keeps_its_stage() {
    let mut f = Fixture::new("bounded-runtime-archive");
    f.declare(1);
    f.finish(1, f.now, "completed");
    f.world.directory.agents.get_mut(TARGET).unwrap().archived = true;
    f.tick(60);
    let snapshot = f.store.snapshot().unwrap();
    assert_eq!(
        snapshot.asks[&f.ask]
            .bounded_continuation
            .current
            .as_ref()
            .unwrap()
            .state,
        Phase::Cancelled
    );
    assert_eq!(snapshot.asks[&f.ask].bounded_continuation.stages.len(), 1);
    assert!(snapshot.asks[&f.ask].resume.is_none());
    assert_eq!(f.runtime.submitted.borrow().len(), 1);
}

#[test]
fn real_scheduler_enforces_elapsed_budget_while_the_target_is_paused() {
    let f = Fixture::new("bounded-runtime-wall-clock");
    f.declare(1);
    f.finish(1, f.now, "completed");
    f.store
        .with_state(|state| {
            crate::asks::agent_pause::pause(state, TARGET, "owner", None, f.now);
        })
        .unwrap();
    f.tick(86_400);
    f.tick(86_401);
    let snapshot = f.store.snapshot().unwrap();
    assert_eq!(snapshot.asks[&f.ask].execution, ExecutionState::Running);
    assert_eq!(
        snapshot.asks[&f.ask]
            .bounded_continuation
            .current
            .as_ref()
            .unwrap()
            .state,
        Phase::Limited
    );
    assert_eq!(
        snapshot
            .notices
            .iter()
            .filter(|n| n.method == "cutex/delegation/continuationLimited")
            .count(),
        1
    );
    assert!(snapshot.answers.is_empty());
    assert!(f.runtime.interrupts.borrow().is_empty());
    assert_eq!(f.runtime.submitted.borrow().len(), 1);
}
