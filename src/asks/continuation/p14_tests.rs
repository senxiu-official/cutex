//! r290 P14: real continuation/observer/scheduler sequence and tool boundary.
use super::*;
use crate::asks::{model::*, tools::{self, tests::call, DELEGATION_DELIVER}, view};

fn waiting(tag: &str) -> Fixture {
    let f = Fixture::new(tag);
    f.declare(1);
    f.finish(1, f.now, "completed");
    f
}

fn other_turn(f: &Fixture, turn: &str) {
    f.runtime.set(TARGET, |s| s.active_turn_id = Some(turn.into()));
    f.store.with_state(|state| {
        let presence = state.presence.entry(TARGET.into()).or_default();
        presence.online = true;
        presence.busy = true;
        presence.runtime_generation = Some(1);
        presence.turn_id = Some(turn.into());
        state.leases.insert(TARGET.into(), AdmissionLease {cutex_session_id:TARGET.into(),
            holder:LeaseHolder::Owner, native_message_id:format!("owner-{turn}"),
            runtime_generation:1, turn_id:Some(turn.into()), since:crate::asks::now_rfc3339(f.now)});
    }).unwrap();
}

fn deliver(f: &Fixture, action: &str) -> Value {
    call(&f.world, &f.store, TARGET, DELEGATION_DELIVER,
        json!({"delegation":f.ask,"text":"B3 finished; complete delivery notes"}), action)
}

#[test]
fn p14_claude_task_turn_can_deliver_specific_yielded_ask_and_timer_never_restarts_it() {
    let f = waiting("p14-claude-task");
    let before = f.store.shared().unwrap().asks[&f.ask].current_attempt().unwrap().clone();
    other_turn(&f, "claude-task-turn");
    let result = deliver(&f, "deliver-task");
    assert_eq!(result["ok"], true, "{result}");
    let first = f.store.snapshot().unwrap();
    let ask = &first.asks[&f.ask];
    assert_eq!(ask.execution, ExecutionState::Answered);
    assert_eq!(ask.closed, Some(CloseOutcome::Completed));
    assert!(view::continuation_wait(ask).is_null());
    let current = ask.current_attempt().unwrap();
    assert_eq!(current.state, AttemptState::Answered);
    assert_eq!(current.turn_id, before.turn_id);
    assert_eq!(current.ended_at, before.ended_at);
    assert_eq!(current.stage_note, before.stage_note);
    assert_eq!(ask.bounded_continuation.current.as_ref().unwrap().cancel_reason.as_deref(), Some("explicit_delivery"));
    assert_eq!(ask.bounded_continuation.current.as_ref().unwrap().state, Phase::Cancelled);
    let answer = &first.answers[ask.answer_id.as_ref().unwrap()];
    assert_eq!(answer.metadata.turn_id, before.turn_id);
    let source = answer.metadata.delivered_from.as_ref().unwrap();
    assert_eq!(source.turn_id, "claude-task-turn");
    assert_eq!(source.runtime_generation, 1);
    let (_, bytes) = f.store.read_artifact(answer.artifact_id.as_ref().unwrap(), None).unwrap().unwrap();
    assert_eq!(String::from_utf8(bytes).unwrap(), "B3 finished; complete delivery notes");
    assert!(matches!(first.leases[TARGET].holder, LeaseHolder::Owner));
    assert_eq!(first.answers.len(), 1);
    assert_eq!(first.deliveries.values().filter(|d| d.kind == DeliveryKind::Answer).count(), 1);
    assert!(first.notices.iter().any(|n|n.method=="cutex/ask/answered" && n.params["metadata"]["deliveredFrom"]["turnId"]=="claude-task-turn"));
    drop(first);
    // The unrelated current turn ends normally; it cannot write another result.
    observe::observe(&f.store, TARGET, "turn/completed", &json!({"threadId":"t","turn":{
        "id":"claude-task-turn","status":"completed","items":[{"type":"agentMessage","phase":"final_answer","text":"ordinary current-turn final"}]}}),f.now).unwrap();
    f.runtime.set(TARGET, |s|s.active_turn_id=None);
    f.tick(61); f.tick(601);
    assert_eq!(f.runtime.submitted.borrow().len(), 1);
    assert_eq!(f.store.shared().unwrap().answers.len(), 1);
    assert_eq!(deliver(&f,"deliver-task"), result, "same-action replay after closure and turn end");
    let conflict=call(&f.world,&f.store,TARGET,DELEGATION_DELIVER,json!({"delegation":f.ask,"text":"different"}),"deliver-task");
    assert_eq!(conflict["code"],"idempotency_conflict");
}

#[test]
fn p14_without_explicit_deliver_unbound_final_is_not_adopted_and_timer_still_resumes() {
    let f = waiting("p14-no-deliver");
    other_turn(&f,"claude-task-unbound");
    observe::observe(&f.store,TARGET,"turn/completed",&json!({"threadId":"t","turn":{
        "id":"claude-task-unbound","status":"completed","items":[{"type":"agentMessage","phase":"final_answer","text":"finished but not delivered"}]}}),f.now).unwrap();
    assert!(f.store.shared().unwrap().answers.is_empty());
    assert_eq!(f.store.shared().unwrap().asks[&f.ask].current_attempt().unwrap().state,AttemptState::Yielded);
    f.runtime.set(TARGET,|s|s.active_turn_id=None);
    f.tick(60);
    assert_eq!(f.runtime.submitted.borrow().len(),2);
    f.finish(2,f.now+Duration::seconds(61),"completed");
    assert_eq!(f.store.shared().unwrap().answers.len(),1);
}

#[test]
fn p14_wait_projection_is_identical_for_owner_agent_and_desktop_and_invalidates_old_clients() {
    let f=waiting("p14-wait-view");
    let snapshot=f.store.shared().unwrap(); let ask=&snapshot.asks[&f.ask];
    let wait=view::continuation_wait(ask);
    assert_eq!(wait["state"],"waiting"); assert_eq!(wait["kind"],"after");
    assert_eq!(wait["nextWakeAt"],json!(f.now+Duration::seconds(60)));
    assert_eq!(view::summary(&snapshot,ask)["continuationWait"],wait);
    assert_eq!(view::requester_view(&snapshot,ask)["continuationWait"],wait);
    assert_eq!(view::owner_view(&snapshot,ask)["continuationWait"],wait);
    assert_eq!(wait.as_object().unwrap().len(),4);
    assert!(snapshot.notices.iter().any(|n| n.method=="cutex/ask/updated"
        && n.params["changed"].as_array().is_some_and(|a|a.contains(&json!("continuationWait")))));
    drop(snapshot);
    other_turn(&f,"owner-turn-blocks-resume");
    f.tick(61);
    assert_eq!(view::continuation_wait(&f.store.shared().unwrap().asks[&f.ask])["state"],"ready");
    f.runtime.set(TARGET,|s|s.active_turn_id=None);
    f.store.with_state(|s|{s.leases.remove(TARGET);s.presence.get_mut(TARGET).unwrap().busy=false;}).unwrap();
    f.tick(62);
    assert!(view::continuation_wait(&f.store.shared().unwrap().asks[&f.ask]).is_null());
}

#[test]
fn p14_deliver_refuses_running_closed_foreign_generation_and_forged_source() {
    let f=Fixture::new("p14-running-refusal");
    let result=deliver(&f,"running"); assert_eq!(result["code"],"delegation_not_waiting","{result}");
    let f=waiting("p14-deliver-guards"); other_turn(&f,"alarm-turn");
    let foreign=call(&f.world,&f.store,crate::asks::store::tests::REQUESTER,DELEGATION_DELIVER,
        json!({"delegation":f.ask,"text":"wrong handler"}),"foreign");
    assert_eq!(foreign["ok"],false); assert!(f.store.shared().unwrap().answers.is_empty());
    for field in ["turnId","runtimeGeneration","sourceTurnId"] {
        let mut args=json!({"delegation":f.ask,"text":"forged"});args[field]=json!("fake");
        assert_eq!(call(&f.world,&f.store,TARGET,DELEGATION_DELIVER,args,&format!("forged-{field}"))["code"],"invalid_arguments");
    }
    f.store.with_state(|s|s.presence.get_mut(TARGET).unwrap().runtime_generation=Some(2)).unwrap();
    assert_eq!(deliver(&f,"generation")["code"],"delegation_not_current_handler");
    f.store.with_state(|s|{s.presence.get_mut(TARGET).unwrap().runtime_generation=Some(1);
        s.update_ask(&f.ask,f.now,|a|{a.closed=Some(CloseOutcome::Stopped);a.bounded_continuation.cancel("stopped");});}).unwrap();
    assert_eq!(deliver(&f,"closed")["code"],"delegation_not_waiting");
    assert!(f.store.shared().unwrap().answers.is_empty());
}

#[test]
fn p14_explicit_deliver_preserves_review_and_selects_only_named_waiting_delegation() {
    let f=waiting("p14-review"); other_turn(&f,"owner-turn");
    let other=f.store.with_state(|s|{
        s.update_ask(&f.ask,f.now,|a|a.options.review=true);
        let mut copy=(**s.asks.get(&f.ask).unwrap()).clone();copy.ask_id="ask-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into();
        s.asks.insert(copy.ask_id.clone(),copy.into());"ask-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()
    }).unwrap();
    let missing=call(&f.world,&f.store,TARGET,DELEGATION_DELIVER,json!({"text":"must name"}),"missing");
    assert_eq!(missing["code"],"invalid_arguments");
    assert_eq!(deliver(&f,"review")["ok"],true);
    let s=f.store.shared().unwrap();
    assert_eq!(s.asks[&f.ask].review,Some(ReviewState::AwaitingAcceptance));
    assert!(s.asks[&f.ask].closed.is_none());
    assert_eq!(s.asks[&other].current_attempt().unwrap().state,AttemptState::Yielded);
    assert!(s.asks[&other].answer_id.is_none());
}

#[test]
fn p14_scheduler_admission_winning_the_race_refuses_delivery() {
    let f=waiting("p14-admission-wins");
    f.tick(60);
    assert_eq!(f.runtime.submitted.borrow().len(),2);
    assert_eq!(deliver(&f,"too-late")["code"],"delegation_not_waiting");
    assert!(f.store.shared().unwrap().answers.is_empty());
}

#[test]
fn p14_tool_definition_and_short_id_delivery_use_the_same_receipt() {
    let f=waiting("p14-short-id"); other_turn(&f,"alarm-turn");
    let schema=tools::definitions().into_iter().find(|v|v["name"]==DELEGATION_DELIVER).unwrap();
    assert_eq!(schema["inputSchema"]["required"],json!(["delegation","text"]));
    let short=crate::short_ids::display(&f.ask);
    let args=json!({"delegation":short,"text":"complete"});
    let first=call(&f.world,&f.store,TARGET,DELEGATION_DELIVER,args,"short-delivery");
    assert_eq!(first["ok"],true,"{first}");
    assert_eq!(call(&f.world,&f.store,TARGET,DELEGATION_DELIVER,json!({"delegation":f.ask,"text":"complete"}),"short-delivery"),first);
}

#[test]
fn p14_scheduler_reserving_during_delivery_io_wins_atomically() {
    let f=waiting("p14-io-race"); other_turn(&f,"task-before-io");
    let send=|_:&tools::TellSend<'_>| -> anyhow::Result<Value> {panic!("no bus send")};
    let wake=|_:&str,_:&str,_:&str|None;
    let clock=Utc::now; let sleep=||panic!("no wait");
    let ctx=tools::ToolContext{store:&f.store,world:&f.world,caller:TARGET,now:f.now,send:&send,
        wake_chain:&wake,clock:&clock,sleep:&sleep,wait:true,facade:tools::FACADE_DELEGATIONS};
    let before_commit=||{
        // The task turn ends while artifact persistence is in flight, and
        // the existing scheduler reserves/submits the next attempt first.
        f.runtime.set(TARGET,|s|s.active_turn_id=None);
        f.store.with_state(|s|{s.leases.remove(TARGET);s.presence.get_mut(TARGET).unwrap().busy=false;}).unwrap();
        f.tick(60);
    };
    let error=crate::asks::deliver::handle_with(&ctx,json!({"delegation":f.ask,"text":"completed"}),"race-delivery",&before_commit).unwrap_err();
    assert_eq!(error.code,"ask_not_waiting");
    let s=f.store.shared().unwrap();
    assert!(s.answers.is_empty());
    assert_eq!(s.asks[&f.ask].attempts.len(),2);
    assert!(s.actions.get(&format!("{TARGET}\u{1f}race-delivery")).is_none(),"failed preconditions leave no success receipt");
    assert_eq!(f.runtime.submitted.borrow().len(),2);
}

#[test]
fn p14_delivery_refuses_reserved_unknown_and_empty_text_without_reopening_pause() {
    for (index,submission) in [Submission::Reserved{attempt_id:"future#2".into(),runtime_generation:1,native_message_id:"reserved".into()},
        Submission::Unknown{attempt_id:"future#2".into(),runtime_generation:1,native_message_id:"unknown".into()}].into_iter().enumerate() {
        let f=waiting(&format!("p14-reserved-{index}")); other_turn(&f,"alarm");
        f.store.with_state(|s|s.update_ask(&f.ask,f.now,|a|a.bounded_continuation.current.as_mut().unwrap().wake.as_mut().unwrap().submission=submission)).unwrap();
        assert_eq!(deliver(&f,"reserved")["code"],"delegation_not_waiting");
        assert!(f.store.shared().unwrap().answers.is_empty());
    }
    let f=waiting("p14-paused");other_turn(&f,"owner");
    for text in ["".to_string(),"  ".into(),"x".repeat(65537)] {
        assert_eq!(call(&f.world,&f.store,TARGET,DELEGATION_DELIVER,json!({"delegation":f.ask,"text":text}),"empty-or-large")["code"],"invalid_arguments");
    }
    f.store.with_state(|s|s.presence.get_mut(TARGET).unwrap().paused=true).unwrap();
    assert_eq!(deliver(&f,"paused")["ok"],true);
    assert!(f.store.shared().unwrap().presence[TARGET].paused,"delivery must not resume admission");
}

#[test]
fn p14_children_and_limited_projections_have_no_fake_timer_and_jobs_can_deliver() {
    let f=waiting("p14-projection-kinds");other_turn(&f,"background");
    f.store.with_state(|s|{
        s.update_ask(&f.ask,f.now,|a|{
            let record=a.bounded_continuation.current.as_mut().unwrap();
            record.declaration.wake=Some(WakeCondition::Children{children:vec![ChildRef{kind:ChildKind::Delegation,id:"private-child-id".into()}],until:Until::All});
        });
    }).unwrap();
    let s=f.store.shared().unwrap();let wait=view::continuation_wait(&s.asks[&f.ask]);
    assert_eq!(wait["kind"],"children"); assert!(wait["nextWakeAt"].is_null());
    assert!(!wait.to_string().contains("private-child-id"));drop(s);
    f.store.with_state(|s|s.update_ask(&f.ask,f.now,|a|a.bounded_continuation.current.as_mut().unwrap().state=Phase::Limited)).unwrap();
    assert_eq!(view::continuation_wait(&f.store.shared().unwrap().asks[&f.ask])["state"],"limited");
    assert_eq!(deliver(&f,"deliver-limited")["ok"],true);
    let f=waiting("p14-legacy-jobs");other_turn(&f,"jobs-done");
    f.store.with_state(|s|s.update_ask(&f.ask,f.now,|a|{
        a.bounded_continuation=ContinuationData::default();
        a.awaiting=Some(AwaitRecord{attempt:1,jobs:vec!["job-private".into()],until:AwaitUntil::All,
            resume:AwaitResume::All,note:String::new(),since:crate::asks::now_rfc3339(f.now),timeout_at:None});
    })).unwrap();
    assert_eq!(view::continuation_wait(&f.store.shared().unwrap().asks[&f.ask])["kind"],"jobs");
    assert_eq!(deliver(&f,"jobs")["ok"],true);
    assert!(f.store.shared().unwrap().asks[&f.ask].awaiting.is_none());
}

#[test]
fn p14_concurrent_duplicate_and_changed_current_turn_are_rechecked_after_io() {
    for duplicate in [true,false] {
        let f=waiting(&format!("p14-commit-race-{duplicate}"));other_turn(&f,"task-turn");
        let send=|_:&tools::TellSend<'_>| -> anyhow::Result<Value> {panic!("no bus send")};
        let wake=|_:&str,_:&str,_:&str|None;let clock=Utc::now;let sleep=||panic!("no wait");
        let ctx=tools::ToolContext{store:&f.store,world:&f.world,caller:TARGET,now:f.now,send:&send,
            wake_chain:&wake,clock:&clock,sleep:&sleep,wait:true,facade:tools::FACADE_DELEGATIONS};
        let args=json!({"delegation":f.ask,"text":"one result"});
        let before_commit=||{
            if duplicate {
                assert_eq!(crate::asks::deliver::handle(&ctx,args.clone(),"same-action").unwrap()["ok"],true);
            } else {
                other_turn(&f,"new-unrelated-turn");
            }
        };
        let result=crate::asks::deliver::handle_with(&ctx,args.clone(),"same-action",&before_commit);
        let s=f.store.shared().unwrap();
        if duplicate {
            assert_eq!(result.unwrap()["ok"],true);
            assert_eq!(s.answers.len(),1);
            assert_eq!(s.deliveries.values().filter(|d|d.kind==DeliveryKind::Answer).count(),1);
        } else {
            assert_eq!(result.unwrap_err().code,"ask_delivery_conflict");
            assert!(s.answers.is_empty());
            assert_eq!(s.asks[&f.ask].current_attempt().unwrap().state,AttemptState::Yielded);
        }
    }
}
