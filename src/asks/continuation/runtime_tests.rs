use super::*;
use crate::asks::scheduler::tests::{world, FakeRuntime, FakeWorld};
use crate::asks::store::tests::{new_ask, temp_store, TARGET};
use crate::asks::{awaits, observe, scheduler};

// r290 P7: declared continuation used to absorb a failed turn as Running.
#[test]
fn handler_failure_cancels_automatic_continuation_and_notifies_controller() {
    let f = Fixture::new("handler-failure-continuation");
    f.declare(1);
    f.finish(1, f.now, "failed");
    let state = f.store.snapshot().unwrap();
    let ask = &state.asks[&f.ask];
    assert!(ask.is_open());
    assert_eq!(ask.execution, ExecutionState::Failed);
    assert_eq!(ask.bounded_continuation.current.as_ref().unwrap().state, Phase::Cancelled);
    assert_eq!(state.deliveries.values().filter(|d|d.ref_id.starts_with(crate::asks::handler_failure::PREFIX)).count(),1);
    drop(state);
    f.tick(61);
    assert_eq!(f.runtime.submitted.borrow().len(),1);
}
// r290 P14
#[test]
fn repeated_handler_failure_after_bounded_resume_is_not_a_silent_stop() {
    use crate::asks::{handler_failure,terminal_notices};
    use crate::asks::store::tests::REQUESTER;
    let f=Fixture::new("terminal-bounded-resume");
    f.declare(1);f.finish(1,f.now,"completed");f.tick(60);
    f.finish(2,f.now+Duration::seconds(61),"failed");
    f.store.with_state(|s|awaits::resume_now(s,&f.ask,Some(REQUESTER),Some("retry"),f.now+Duration::seconds(62))).unwrap().unwrap();
    f.tick(63);
    let resumed=f.store.snapshot().unwrap();
    assert!(resumed.asks[&f.ask].is_open());assert_eq!(resumed.asks[&f.ask].attempts.len(),3);
    assert_eq!(f.runtime.submitted.borrow().len(),3);
    f.finish(3,f.now+Duration::seconds(64),"failed");
    f.finish(3,f.now+Duration::seconds(65),"failed");
    let failed=f.store.snapshot().unwrap();
    assert!(failed.asks[&f.ask].is_open());assert!(failed.answers.is_empty());
    let notices:Vec<_>=failed.deliveries.values().filter(|d|d.ref_id.starts_with(handler_failure::PREFIX)).collect();
    assert_eq!(notices.len(),2,"one durable notice per actual failed attempt, not per callback");
    assert!(notices.iter().all(|d|terminal_notices::for_delivery(&failed.asks[&f.ask],d).unwrap().actor==terminal_notices::Actor::System{component:"handlerRuntime".into()}));
    drop(failed);
    crate::asks::tools::close_ask(&f.store,None,&f.ask,"stop",f.now+Duration::seconds(66)).unwrap();
    let reopened=AskStore::at(f.store.dir().to_owned()).snapshot().unwrap();
    let ask=&reopened.asks[&f.ask];assert_eq!(ask.closed,Some(model::CloseOutcome::Stopped));
    assert_eq!(terminal_notices::read_state(ask)["actor"]["kind"],"owner");
    assert_eq!(reopened.deliveries.values().filter(|d|terminal_notices::is_notice(d)).count(),1);
    let prior_failure=reopened.deliveries.values().find(|d|d.ref_id.starts_with(handler_failure::PREFIX)).unwrap();
    let item=crate::asks::returns::return_item(&f.store,&reopened,prior_failure).unwrap();
    let (_,text)=crate::asks::render::return_text(&[item]);
    assert!(text.contains("already closed"));assert!(!text.contains("Continue with delegation_resume"));
}

// r290 P14
#[path = "p14_tests.rs"]
mod p14;

#[test]
#[ignore = "EVA-sized continuation no-change scale probe"]
fn waste_scale_a06_continuation_not_due() {
    let f=Fixture::new("scale-continuation");f.declare(1);f.finish(1,f.now,"completed");
    let fixture=crate::test_support::waste_scale::asks();
    let bytes=serde_json::to_vec(&fixture).unwrap().len();
    f.store.with_state(|state|{
        state.asks.extend(fixture.asks);
        let ask=state.asks[&f.ask].clone();
        for n in 0..15 {let mut copy=ask.clone();copy.ask_id=format!("continuation-{n}");state.asks.insert(copy.ask_id.clone(),copy);}
    }).unwrap();
    let mut samples=Vec::new();let before=crate::asks::store::whole_state_copies_on_this_thread();
    for _ in 0..6 {let t=std::time::Instant::now();tick_due(&f.world,&f.runtime,&f.store,f.now+Duration::seconds(1)).unwrap();samples.push(t.elapsed());}
    let copies=crate::asks::store::whole_state_copies_on_this_thread()-before;
    crate::test_support::waste_scale::emit("a06_continuation_not_due",&samples,copies,json!({"asks_bytes":bytes,"continuations":16}));
}

#[test]
fn waste_guard_continuation_waiting_has_zero_store_drafts_but_stop_and_limit_commit(){
    let f=Fixture::new("guard-continuation");f.declare(1);f.finish(1,f.now,"completed");
    f.store.with_state(|state|state.asks.extend(crate::test_support::waste_scale::asks().asks)).unwrap();
    let before=crate::asks::store::whole_state_copies_on_this_thread();
    for _ in 0..4 {tick_due(&f.world,&f.runtime,&f.store,f.now+Duration::seconds(1)).unwrap();}
    assert_eq!(crate::asks::store::whole_state_copies_on_this_thread()-before,0,"not-due continuations must not draft the whole store");
    tick_due(&f.world,&f.runtime,&f.store,f.now+Duration::seconds(i64::from(MAX_ELAPSED_SECONDS)+1)).unwrap();
    assert_eq!(f.store.shared().unwrap().asks[&f.ask].bounded_continuation.current.as_ref().unwrap().state,Phase::Limited);
    let stopped=Fixture::new("guard-continuation-stop");stopped.declare(1);stopped.finish(1,stopped.now,"completed");
    stopped.store.with_state(|state|{state.asks.get_mut(&stopped.ask).unwrap().closed=Some(model::CloseOutcome::Stopped);}).unwrap();
    tick_due(&stopped.world,&stopped.runtime,&stopped.store,stopped.now+Duration::seconds(1)).unwrap();
    assert_eq!(stopped.store.shared().unwrap().asks[&stopped.ask].bounded_continuation.current.as_ref().unwrap().state,Phase::Cancelled);
}

#[test]
fn verified_delegation_child_finished_before_declaration_wakes_once(){
    let f=Fixture::new("bounded-child-before");
    let child=f.store.with_state(|state|{
        let mut new=new_ask("child-before","review");
        new.options.notify=model::Notify::Passive; // Isolate the declared child wake from its separate terminal notice.
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
    assert!(snapshot.asks[&f.ask].is_open(), "Limited retains the explicit reset entry");
    assert!(snapshot.answers.is_empty());
}

#[test]
fn reloaded_timed_wait_reports_quota_and_resumes_once_after_release() {
    let f = Fixture::new("continuation-blocker-reload");
    let mut request = f.request(1);
    request.wake = Some(WakeCondition::After {
        delay_seconds: MIN_WAKE_SECONDS,
    });
    f.store.with_state(|state| {
        let guards = Guards { online: true, permission_valid: true, occurrence_valid: true, ..Default::default() };
        declare_current(state, TARGET, &f.ask, &request, guards, f.now, None)
    }).unwrap().unwrap();
    f.finish(1, f.now, "completed");
    let store = AskStore::at(f.store.dir());
    let due = f.now + Duration::seconds(60);
    f.runtime.status.borrow_mut().get_mut(TARGET).unwrap().blocked =
        Some(model::BlockedReason::Quota);
    tick_due(&f.world, &f.runtime, &store, due - Duration::seconds(1)).unwrap();
    assert!(crate::asks::view::continuation_wait(&store.snapshot().unwrap().asks[&f.ask])
        .get("blockedReason").is_none());

    tick_due(&f.world, &f.runtime, &store, due).unwrap();
    let snapshot = store.snapshot().unwrap();
    let ask = &snapshot.asks[&f.ask];
    let revision = ask.revision;
    let wait = crate::asks::view::continuation_wait(ask);
    assert_eq!(wait["state"], "ready");
    assert_eq!(wait["blockedReason"], "quota");
    assert_eq!(wait["nextWakeAt"], json!(due));
    assert!(ask.resume.is_none());

    tick_due(&f.world, &f.runtime, &store, due + Duration::seconds(1)).unwrap();
    assert_eq!(store.snapshot().unwrap().asks[&f.ask].revision, revision);
    f.runtime.status.borrow_mut().get_mut(TARGET).unwrap().blocked = None;
    tick_due(&f.world, &f.runtime, &store, due + Duration::seconds(2)).unwrap();
    let snapshot = store.snapshot().unwrap();
    let ask = &snapshot.asks[&f.ask];
    assert!(ask.resume.is_some());
    assert!(crate::asks::view::continuation_wait(ask).get("blockedReason").is_none());

    let wake_id = bounded_wake(ask).unwrap();
    store.with_state(|state| record_routing_wait(state, &f.ask, &wake_id, due + Duration::seconds(2))).unwrap();
    tick_due(&f.world, &f.runtime, &store, due + Duration::seconds(3)).unwrap();
    assert_eq!(crate::asks::view::continuation_wait(&store.snapshot().unwrap().asks[&f.ask])
        ["blockedReason"], "routing");

    f.runtime.set(TARGET, |status| status.exclusive_admission = false);
    tick_due(&f.world, &f.runtime, &store, due + Duration::seconds(4)).unwrap();
    assert_eq!(crate::asks::view::continuation_wait(&store.snapshot().unwrap().asks[&f.ask])
        ["blockedReason"], "runtimeUnsupported");
    f.runtime.set(TARGET, |status| status.exclusive_admission = true);
    tick_due(&f.world, &f.runtime, &store, due + Duration::seconds(5)).unwrap();
    assert!(crate::asks::view::continuation_wait(&store.snapshot().unwrap().asks[&f.ask])
        .get("blockedReason").is_none());

    f.tick(66);
    f.tick(67);
    assert_eq!(f.runtime.submitted.borrow().len(), 2);
}

#[test]
fn yielded_stop_cancels_waiting_and_ready_plans_and_keeps_outer_closed() {
    for ready in [false,true] {
        let f=Fixture::new("yielded-stop-continuation");f.declare(1);f.finish(1,f.now,"completed");
        if ready { tick_due(&f.world,&f.runtime,&f.store,f.now+Duration::seconds(60)).unwrap(); }
        let result=crate::asks::tools::close_ask(&f.store,None,&f.ask,"stop",f.now+Duration::seconds(60)).unwrap();
        assert_eq!(result["stopping"],false);
        f.tick(61);f.tick(120);
        let state=f.store.snapshot().unwrap();let ask=&state.asks[&f.ask];
        assert!(!ask.is_open());assert!(ask.resume.is_none());assert!(!ask.attempts[0].stop_requested);
        assert_eq!(ask.bounded_continuation.current.as_ref().unwrap().state,Phase::Cancelled);
        assert_eq!(f.runtime.submitted.borrow().len(),1);assert!(state.answers.is_empty());
    }
}

#[test]
fn cancelled_plan_settles_outer_in_the_same_scheduler_transaction_but_limit_resets() {
    let f=Fixture::new("continuation-cancel-outer");f.declare(1);f.finish(1,f.now,"completed");
    f.store.with_state(|s| {
        s.asks.get_mut(&f.ask).unwrap().bounded_continuation.cancel("ancestorStopped");
        crate::asks::stop_settlement::close_cancelled_continuation(s,&f.ask,f.now);
        assert!(!s.asks[&f.ask].is_open());
    }).unwrap();
    f.tick(61);assert_eq!(f.runtime.submitted.borrow().len(),1);

    let limited=Fixture::new("continuation-limit-reset");limited.declare(1);limited.finish(1,limited.now,"completed");
    let at=limited.now+Duration::seconds(MAX_ELAPSED_SECONDS);
    tick_due(&limited.world,&limited.runtime,&limited.store,at).unwrap();
    let state=limited.store.snapshot().unwrap();let a=&state.asks[&limited.ask];assert!(a.is_open());
    let revision=a.bounded_continuation.current.as_ref().unwrap().revision;
    let actor=Actor::Owner;
    let request=ResetRequest{action_id:"owner-reset".into(),expected_revision:revision};
    limited.store.with_state(|s|resume_limited(s,&limited.ask,Authority{actor:&actor,current_director:None},&request,
        Guards{online:true,permission_valid:true,occurrence_valid:true,..Default::default()},at)).unwrap().unwrap();
    limited.store.with_state(|s|resume_limited(s,&limited.ask,Authority{actor:&actor,current_director:None},&request,
        Guards{online:true,permission_valid:true,occurrence_valid:true,..Default::default()},at)).unwrap().unwrap();
    assert!(limited.store.snapshot().unwrap().asks[&limited.ask].is_open());
    assert_eq!(limited.runtime.submitted.borrow().len(),1,"reset grants a later wake, never immediate IO");
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

fn continuation_approval_context(
    actor: crate::asks::routing_policy::RoutingActor,
    director: Option<&str>,
    epoch: u64,
) -> crate::asks::routing_policy::RoutingContext {
    use crate::asks::routing_policy::{RoutingContext, RoutingRef};
    RoutingContext {
        project_id: "alpha".into(),
        authority_epoch: epoch,
        current_director: director.map(|id| RoutingRef::new("local", id).unwrap()),
        actor,
        operator_may_route: false,
    }
}

fn expired_approval_fixture(label: &str) -> (Fixture, DateTime<Utc>) {
    let f = Fixture::new(label);
    f.declare(1);
    f.finish(1, f.now, "completed");
    let at = f.now + Duration::seconds(MAX_ELAPSED_SECONDS);
    tick_due(&f.world, &f.runtime, &f.store, at).unwrap();
    (f, at)
}

fn decision_input(
    a: &super::approval::Approval,
    decision: crate::asks::routing_policy::RoutingDecision,
) -> super::approval::DecisionRequest {
    super::approval::DecisionRequest {
        action_id: "decide-expired".into(),
        expected_revision: a.revision,
        expected_epoch: a.authority_epoch,
        expected_digest: a.digest.clone(),
        decision,
        decision_note: Some("Reviewed the stage".into()),
    }
}

#[test]
fn expired_continuation_approval_survives_reload_and_starts_exactly_once() {
    use crate::asks::routing_policy::{RoutingActor, RoutingDecision, RoutingRef};
    use crate::asks::operations::{self, EffectState};
    let (f, at) = expired_approval_fixture("approval-reload");
    let director = "director-a";
    let actor = Actor::Agent { session_id: director.into() };
    let ctx = continuation_approval_context(
        RoutingActor::Agent { reference: RoutingRef::new("local", director).unwrap(), name: None },
        Some(director), 1,
    );
    f.store.with_state(|state| {
        super::approval::sync_one(state, &f.ask, Some(&ctx), at);
        super::approval::sync_one(state, &f.ask, Some(&ctx), at);
    }).unwrap();
    let reopened = AskStore::at(f.store.dir());
    let request = reopened.inspect(|state| {
        let ask = &state.asks[&f.ask];
        let a = ask.bounded_continuation.approval.as_ref().unwrap();
        assert_eq!(a.director_session_id.as_deref(), Some(director));
        assert!(a.brief_preview.contains("implement"));
        assert!(a.progress_preview.as_deref().unwrap().contains("full stage"));
        assert_eq!(state.deliveries.values().filter(|d| d.ref_id.starts_with("continuationExpired@")).count(), 1);
        assert!(state.notices.iter().any(|n| n.params["resourceId"] == a.id && n.sessions.iter().any(|s| s == director)));
        decision_input(a, RoutingDecision::Approve)
    }).unwrap();
    let wire = serde_json::to_value(&request).unwrap();
    let decide = || reopened.with_state(|state| operations::execute(
        state, director, "continuation.resume", &request.action_id, &wire, at, |state| {
            super::approval::decide(state, &f.ask, Some(&ctx), &actor, &request, at)
                .map(|result| (result, EffectState::Committed))
        },
    )).unwrap().unwrap();
    assert_eq!(decide(), decide());
    reopened.inspect(|state| {
        let data = &state.asks[&f.ask].bounded_continuation;
        assert_eq!(data.current.as_ref().unwrap().budget_epoch, 2);
        assert_eq!(data.budget_resets.len(), 1);
        assert_eq!(state.deliveries.values().filter(|d| d.ref_id.starts_with("continuationApproved@")).count(), 1);
    }).unwrap();
    f.tick(MAX_ELAPSED_SECONDS + i64::from(MIN_WAKE_SECONDS) - 1);
    assert_eq!(f.runtime.submitted.borrow().len(), 1);
    f.tick(MAX_ELAPSED_SECONDS + i64::from(MIN_WAKE_SECONDS));
    f.tick(MAX_ELAPSED_SECONDS + i64::from(MIN_WAKE_SECONDS) + 1);
    assert_eq!(f.runtime.submitted.borrow().len(), 2);
}

#[test]
fn rotated_director_invalidates_old_decisions_and_owner_can_deny_without_a_director() {
    use crate::asks::routing_policy::{RoutingActor, RoutingDecision, RoutingRef};
    let (f, at) = expired_approval_fixture("approval-rotation");
    let old_director = "director-a";
    let old_actor = Actor::Agent { session_id: old_director.into() };
    let agent = |id: &str| RoutingActor::Agent {reference: RoutingRef::new("local", id).unwrap(), name: None};
    let original = continuation_approval_context(agent(old_director), Some(old_director), 1);
    let replaced = continuation_approval_context(agent(old_director), Some("director-b"), 2);
    let owner = continuation_approval_context(RoutingActor::Owner, None, 3);
    f.store.with_state(|state| {
        super::approval::sync_one(state, &f.ask, Some(&original), at);
        let first = state.asks[&f.ask].bounded_continuation.approval.clone().unwrap();
        let request = decision_input(&first, RoutingDecision::Deny);
        assert!(super::approval::list(state, Some(&replaced), true).is_empty());
        assert!(super::approval::decide(state, &f.ask, Some(&replaced), &old_actor, &request, at).is_err());
        assert!(super::approval::decide(state, &f.ask, Some(&owner), &Actor::Owner, &request, at).is_err());
        let new_director = "director-b";
        let fresh = continuation_approval_context(agent(new_director), Some(new_director), 2);
        let preview = super::approval::list(state, Some(&fresh), true).remove(0);
        assert_eq!(preview["revision"], first.revision + 1);
        assert_eq!(preview["allowedActions"]["approve"], true);
        assert_eq!(state.asks[&f.ask].bounded_continuation.approval.as_ref().unwrap().revision, first.revision);
        super::approval::sync_one(state, &f.ask, Some(&fresh), at);
        let rotated = state.asks[&f.ask].bounded_continuation.approval.clone().unwrap();
        assert_ne!(rotated.digest, first.digest);
        assert_eq!(preview["digest"], rotated.digest);
        assert_eq!(super::approval::list(state, Some(&fresh), true)[0]["allowedActions"]["approve"], true);
        assert!(state.notices.iter().any(|n| n.params["resourceId"] == rotated.id
            && n.params["revision"] == rotated.revision && n.sessions.iter().any(|s| s == new_director)));
        super::approval::sync_one(state, &f.ask, Some(&owner), at);
        let latest = state.asks[&f.ask].bounded_continuation.approval.clone().unwrap();
        assert_eq!(latest.id, first.id);
        assert_ne!(latest.digest, first.digest);
        assert_eq!(latest.revision, first.revision + 2);
        let request = decision_input(&latest, RoutingDecision::Deny);
        super::approval::decide(state, &f.ask, Some(&owner), &Actor::Owner, &request, at).unwrap();
        let ask = &state.asks[&f.ask];
        assert_eq!(ask.closed, Some(model::CloseOutcome::Stopped));
        assert_eq!(ask.attempts.len(), 1);
        assert_eq!(ask.terminal_notices.last().unwrap().reason, "Reviewed the stage");
        assert!(state.deliveries.values().any(|d| d.ask_id == f.ask && d.terminal_notice.is_some()));
        assert!(super::approval::list(state, Some(&owner), true).is_empty());
    }).unwrap();
    f.tick(MAX_ELAPSED_SECONDS + 120);
    assert_eq!(f.runtime.submitted.borrow().len(), 1);
}

#[test]
fn owner_can_approve_a_current_card_while_the_project_has_a_director() {
    use crate::asks::routing_policy::{RoutingActor, RoutingDecision};
    let (f, at) = expired_approval_fixture("approval-owner");
    let owner = continuation_approval_context(RoutingActor::Owner, Some("director-a"), 1);
    f.store.with_state(|state| {
        super::approval::sync_one(state, &f.ask, Some(&owner), at);
        let approval = state.asks[&f.ask].bounded_continuation.approval.clone().unwrap();
        let request = decision_input(&approval, RoutingDecision::Approve);
        super::approval::decide(state, &f.ask, Some(&owner), &Actor::Owner, &request, at).unwrap();
        let ask = &state.asks[&f.ask];
        assert!(ask.is_open());
        assert_eq!(ask.bounded_continuation.current.as_ref().unwrap().budget_epoch, 2);
        assert_eq!(ask.bounded_continuation.approval.as_ref().unwrap().decided_by, Some(Actor::Owner));
    }).unwrap();
}

#[test]
fn legacy_owner_body_approves_an_unscoped_request_and_uses_the_continuation_revision() {
    let (f, at) = expired_approval_fixture("unscoped-legacy");
    f.store.with_state(|state| {
        state.update_ask(&f.ask, at, |ask| ask.project_id = None);
    }).unwrap();
    super::approval::tick(&f.store, at).unwrap();
    let revision = f.store.inspect(|state| {
        let ask = &state.asks[&f.ask];
        let a = ask.bounded_continuation.approval.as_ref().unwrap();
        assert!(a.director_session_id.is_none());
        assert_eq!(a.authority_epoch, 0);
        assert_eq!(super::approval::list(state, None, true).len(), 1);
        let event = state.notices.iter().find(|n| n.params["resourceId"] == a.id
            && n.params["projectId"].is_null()
            && n.sessions == [crate::management::v2::services::HOST_EVENT_SESSION_ID]).unwrap();
        crate::management::v2::contract_validation::validate_cutex_event_message(&json!({
            "method":"cutex/delegation/routingApprovalChanged","params":event.params,
        })).unwrap();
        let detail = json!({"ask":crate::asks::view::owner_view(state, ask)});
        crate::management::v2::schema_export::assert_valid("continuation-approvals", "unscopedAskDetail", &detail);
        assert_eq!(detail["ask"]["continuationApproval"]["id"], a.id);
        assert!(crate::asks::view::requester_view(state, ask).get("continuationApproval").is_none());
        ask.bounded_continuation.current.as_ref().unwrap().revision
    }).unwrap();
    let legacy = |revision| serde_json::from_value(json!({
        "actionId": "legacy-owner", "expectedRevision": revision,
    })).unwrap();
    f.store.with_state(|state| {
        let err = super::approval::decide_owner(state, &f.ask, None, &legacy(revision + 1), at).unwrap_err();
        assert_eq!(err.code, "approval_changed");
        let result = super::approval::decide_owner(state, &f.ask, None, &legacy(revision), at).unwrap();
        assert_eq!(result["continuationApproval"]["decision"], "approve");
        assert_eq!(state.asks[&f.ask].bounded_continuation.current.as_ref().unwrap().budget_epoch, 2);
    }).unwrap();
}

#[test]
fn requester_stop_after_approval_card_prevents_a_new_segment() {
    use crate::asks::routing_policy::{RoutingActor, RoutingDecision};
    let (f, at) = expired_approval_fixture("approval-requester-stop");
    let ctx = continuation_approval_context(RoutingActor::Owner, Some("director-a"), 1);
    let request = f.store.with_state(|state| {
        super::approval::sync_one(state, &f.ask, Some(&ctx), at);
        decision_input(state.asks[&f.ask].bounded_continuation.approval.as_ref().unwrap(), RoutingDecision::Approve)
    }).unwrap();
    crate::asks::lineage::stop(&f.store, Some(crate::asks::store::tests::REQUESTER), &f.ask,
        crate::asks::lineage::Cascade::None, false, "requester-stop", None, at).unwrap();
    f.store.with_state(|state| {
        let err = super::approval::decide(state, &f.ask, Some(&ctx), &Actor::Owner, &request, at).unwrap_err();
        assert_eq!(err.code, "approval_not_pending");
        assert_eq!(state.asks[&f.ask].closed, Some(model::CloseOutcome::Stopped));
        assert!(state.asks[&f.ask].bounded_continuation.budget_resets.is_empty());
    }).unwrap();
    f.tick(MAX_ELAPSED_SECONDS + 120);
    assert_eq!(f.runtime.submitted.borrow().len(), 1);
}

#[test]
fn continuation_approval_persists_while_runtime_admission_is_held() {
    use crate::asks::routing_policy::{RoutingActor, RoutingDecision, RoutingRef};

    for hold in ["busy", "offline", "owner-offline", "paused", "generation"] {
        let (f, at) = expired_approval_fixture(&format!("approval-hold-{hold}"));
        match hold {
            "busy" => f.runtime.set(TARGET, |s| s.active_turn_id = Some("other-turn".into())),
            "offline" => f.runtime.set(TARGET, |s| s.online = false),
            "owner-offline" => f.store.with_state(|s| {
                s.owner_offline.insert(TARGET.into());
            }).unwrap(),
            "paused" => f.store.with_state(|s| {
                crate::asks::agent_pause::pause(s, TARGET, "owner", None, at);
            }).unwrap(),
            "generation" => f.store.with_state(|s| {
                s.presence.entry(TARGET.into()).or_default().runtime_generation = Some(2);
            }).unwrap(),
            _ => unreachable!(),
        }
        let director = "director-a";
        let actor = Actor::Agent { session_id: director.into() };
        let ctx = continuation_approval_context(
            RoutingActor::Agent { reference: RoutingRef::new("local", director).unwrap(), name: None },
            Some(director),
            1,
        );
        f.store.with_state(|state| {
            super::approval::sync_one(state, &f.ask, Some(&ctx), at);
            let card = super::approval::action_card(&state.asks[&f.ask], Some(&ctx)).unwrap();
            assert_eq!(card["allowedActions"], json!({"approve":true,"deny":true}), "{hold}");
            crate::management::v2::schema_export::assert_valid("continuation-approvals", "treeApprovalCard", &card);
            let approval = state.asks[&f.ask].bounded_continuation.approval.as_ref().unwrap();
            let request = decision_input(approval, RoutingDecision::Approve);
            let receipt = super::approval::decide(state, &f.ask, Some(&ctx), &actor, &request, at).unwrap();
            assert_eq!(receipt["continuationApproval"]["decision"], "approve", "{hold}");
            assert_eq!(state.asks[&f.ask].bounded_continuation.current.as_ref().unwrap().budget_epoch, 2);
        }).unwrap();
        let due = MAX_ELAPSED_SECONDS + i64::from(MIN_WAKE_SECONDS);
        f.tick(due);
        assert_eq!(f.runtime.submitted.borrow().len(), 1, "{hold}");
        let wait = f.store.inspect(|state| crate::asks::view::continuation_wait(&state.asks[&f.ask])).unwrap();
        let reason = match hold {
            "busy" => "targetBusy",
            "offline" | "owner-offline" => "runtimeOffline",
            "paused" => "paused",
            "generation" => "admission",
            _ => unreachable!(),
        };
        assert_eq!(wait["blockedReason"], reason, "{hold}");
        match hold {
            "busy" => f.runtime.set(TARGET, |s| s.active_turn_id = None),
            "offline" => f.runtime.set(TARGET, |s| {
                s.online = true;
                s.runtime_generation = 2;
            }),
            "owner-offline" => f.store.with_state(|s| {
                s.owner_offline.remove(TARGET);
            }).unwrap(),
            "paused" => f.store.with_state(|s| {
                crate::asks::agent_pause::resume(s, TARGET);
            }).unwrap(),
            "generation" => f.runtime.set(TARGET, |s| s.runtime_generation = 2),
            _ => unreachable!(),
        }
        f.tick(due + 1);
        f.tick(due + 2);
        assert_eq!(f.runtime.submitted.borrow().len(), 2, "{hold}");
        if matches!(hold, "offline" | "generation") {
            let generation = f.store.inspect(|s| s.asks[&f.ask].current_attempt().unwrap().runtime_generation).unwrap();
            assert_eq!(generation, 2, "{hold}");
        }
    }
}

#[test]
fn approved_continuation_reports_a_late_runtime_refusal() {
    use crate::asks::routing_policy::{RoutingActor, RoutingDecision};
    use crate::asks::scheduler::SubmitOutcome;
    let (f, at) = expired_approval_fixture("approval-late-refusal");
    let ctx = continuation_approval_context(RoutingActor::Owner, None, 1);
    f.store.with_state(|state| {
        super::approval::sync_one(state, &f.ask, Some(&ctx), at);
        let approval = state.asks[&f.ask].bounded_continuation.approval.as_ref().unwrap();
        let request = decision_input(approval, RoutingDecision::Approve);
        super::approval::decide(state, &f.ask, Some(&ctx), &Actor::Owner, &request, at).unwrap();
    }).unwrap();
    f.runtime.outcomes.borrow_mut().push(SubmitOutcome::Unsupported("exclusive input".into()));
    f.tick(MAX_ELAPSED_SECONDS + i64::from(MIN_WAKE_SECONDS));
    f.store.inspect(|state| {
        let ask = &state.asks[&f.ask];
        assert_eq!(ask.current_attempt().unwrap().state, AttemptState::Refused);
        assert_eq!(crate::asks::view::continuation_wait(ask)["blockedReason"], "runtimeUnsupported");
    }).unwrap();
}

#[test]
fn continuation_approval_readdresses_when_the_handler_leaves_the_director_seat() {
    use crate::asks::routing_policy::{RoutingActor, RoutingDecision, RoutingRef};
    let (f, at) = expired_approval_fixture("approval-director-handler");
    let agent = |id: &str| RoutingActor::Agent {
        reference: RoutingRef::new("local", id).unwrap(),
        name: None,
    };
    let actor = |id: &str| Actor::Agent { session_id: id.into() };
    let incoming = "director-incoming";
    let current = continuation_approval_context(agent(TARGET), Some(TARGET), 2);
    let owner = continuation_approval_context(RoutingActor::Owner, Some(TARGET), 2);
    let replaced = continuation_approval_context(agent(TARGET), Some(incoming), 3);
    let next = continuation_approval_context(agent(incoming), Some(incoming), 3);
    f.store.with_state(|state| {
        let original = continuation_approval_context(agent(incoming), Some(incoming), 1);
        super::approval::sync_one(state, &f.ask, Some(&original), at);
        let old = state.asks[&f.ask].bounded_continuation.approval.clone().unwrap();
        assert_eq!(old.director_session_id.as_deref(), Some(incoming));
        let old_request = decision_input(&old, RoutingDecision::Approve);
        let err = super::approval::decide(state, &f.ask, Some(&current), &actor(TARGET), &old_request, at).unwrap_err();
        assert_eq!(err.code, "forbidden");

        super::approval::sync_one(state, &f.ask, Some(&current), at);
        let a = state.asks[&f.ask].bounded_continuation.approval.clone().unwrap();
        assert_eq!(a.revision, old.revision + 1);
        assert!(a.director_session_id.is_none());
        assert!(state.notices.iter().any(|n| n.params["resourceId"] == a.id
            && n.params["revision"] == a.revision
            && n.sessions == [crate::management::v2::services::HOST_EVENT_SESSION_ID]));
        assert!(super::approval::list(state, Some(&current), true).is_empty());
        assert_eq!(super::approval::list(state, Some(&owner), true).len(), 1);
        let request = decision_input(&a, RoutingDecision::Approve);
        for ctx in [&current, &replaced] {
            let err = super::approval::decide(state, &f.ask, Some(ctx), &actor(TARGET), &request, at).unwrap_err();
            assert_eq!(err.code, "forbidden");
        }

        super::approval::sync_one(state, &f.ask, Some(&next), at);
        let a = state.asks[&f.ask].bounded_continuation.approval.as_ref().unwrap();
        assert_eq!(a.director_session_id.as_deref(), Some(incoming));
        let request = decision_input(a, RoutingDecision::Approve);
        super::approval::decide(state, &f.ask, Some(&next), &actor(incoming), &request, at).unwrap();
    }).unwrap();
}

#[test]
fn archived_project_continuation_falls_back_to_owner_and_delivers_expiry() {
    use crate::agent_management::{AgentManagementSnapshot, ProjectId};
    use crate::asks::routing_policy::RoutingDecision;
    let (f, at) = expired_approval_fixture("approval-archived-project");
    let mut roster = AgentManagementSnapshot::empty();
    roster.project_states.insert(ProjectId::new("alpha").unwrap(),
        serde_json::from_value(json!({
            "project_id":"alpha","lifecycle":"archived","revision":3,
            "created_at":at,"updated_at":at,"archived_at":at,"last_director_epoch":2
        })).unwrap());
    f.store.with_state(|state| {
        let dirty = super::approval::project_changes(&f.store, state, &roster, "local");
        assert_eq!(dirty.len(), 1);
        let (id, ctx) = dirty.into_iter().next().unwrap();
        assert_eq!(id, f.ask);
        super::approval::sync_one(state, &id, Some(&ctx), at);
        let a = state.asks[&id].bounded_continuation.approval.as_ref().unwrap();
        assert_eq!(a.authority_epoch, 2);
        assert!(a.director_session_id.is_none());
        assert_eq!(super::approval::list(state, Some(&ctx), true).len(), 1);
        assert!(state.notices.iter().any(|n| n.params["resourceId"] == a.id
            && n.sessions == [crate::management::v2::services::HOST_EVENT_SESSION_ID]));
        assert_eq!(state.deliveries.values().filter(|d| d.ref_id.starts_with("continuationExpired@")).count(), 1);
        let request = decision_input(a, RoutingDecision::Deny);
        super::approval::decide(state, &id, Some(&ctx), &Actor::Owner, &request, at).unwrap();
    }).unwrap();
}
