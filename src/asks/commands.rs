//! Runtime commands queued by the bus process or Management v2 and run by
//! the management process (asks design §5.9, §5.10).

use chrono::{DateTime, Utc};
use serde_json::json;

use super::model::*;
use super::scheduler::{AskRuntime, RespondOutcome};
use super::store::{AskStore, RuntimeCommand};
use super::truncate_chars;

/// Run one command; `Ok(true)` when it is done (or can never succeed).
pub fn run(runtime: &dyn AskRuntime, store: &AskStore, command: &RuntimeCommand, now: DateTime<Utc>) -> anyhow::Result<bool> {
    match command {
        RuntimeCommand::ProjectPauseInterrupt { project_id, pause_id, cutex_session_id, runtime_generation, turn_id } => {
            let lane=runtime.result_admission_lane(cutex_session_id)?;
            let _operation=match lane.as_ref().map(|lane|lane.try_lock()) {
                Some(Ok(guard))=>Some(guard), None=>None,
                Some(Err(super_error))=>{
                    if matches!(super_error,crate::management::v2::session_coordination::ReservationError::WouldBlock){return Ok(false);}
                    return Err(super_error.into());
                }
            };
            if store.inspect(|state| state.project_control.projects.get(project_id).is_some_and(|p|
                p.pause_id.as_deref()==Some(pause_id.as_str())&&p.interrupt_unknown.contains(cutex_session_id)))? {
                return Ok(true); // A prior dispatch claim survives offline/restart too.
            }
            let valid = store.inspect(|state| state.project_control.projects.get(project_id).is_some_and(|p|
                p.pause_id.as_deref() == Some(pause_id.as_str()) && p.members.contains(cutex_session_id)))?;
            if !valid { return Ok(true); }
            let status = runtime.target_status(cutex_session_id);
            if !status.online { return Ok(false); }
            if status.runtime_generation != *runtime_generation || status.active_turn_id.as_deref() != Some(turn_id.as_str()) {
                return Ok(true);
            }
            // Persist the dispatch claim before RPC. A crash at any subsequent
            // point must not resend an interruption whose outcome is unknown.
            let claimed=super::pause_admission::try_with_state(store, |state,_| {
                    let Some(p)=state.project_control.projects.get_mut(project_id).filter(|p|
                        p.pause_id.as_deref()==Some(pause_id.as_str())&&p.members.contains(cutex_session_id)
                        && !p.excluded_session_ids.contains(cutex_session_id)) else {return false;};
                    p.interrupt_unknown.insert(cutex_session_id.clone())
            })?;
            let claimed=match claimed {
                super::pause_admission::Admission::Ready(claimed)=>claimed,
                super::pause_admission::Admission::Deferred=>return Ok(false),
            };
            if !claimed{return Ok(true);}
            // A successful RPC still needs terminal observation; leave the claim
            // intact until this pause is resumed, including across restart.
            match runtime.interrupt_exact(cutex_session_id, *runtime_generation, turn_id) {
                Ok(()) => Ok(true), Err(_) => {
                    // An error can follow delivery. Preserve uncertainty and
                    // never replay the interrupt automatically.
                    store.with_state(|state| {
                        if let Some(p)=state.project_control.projects.get_mut(project_id)
                            .filter(|p|p.pause_id.as_deref()==Some(pause_id.as_str())) {
                            p.interrupt_unknown.insert(cutex_session_id.clone());
                        }
                    })?;
                    Ok(true)
                },
            }
        }
        RuntimeCommand::ResumeGate { .. } => Ok(false),
        RuntimeCommand::StopAttempt { ask_id, attempt } => stop_attempt(runtime, store, ask_id, *attempt, now),
        RuntimeCommand::RespondQuestion { question_id } => respond_question(runtime, store, question_id, now),
        RuntimeCommand::CancelJobs { ask_id, cutex_session_id, jobs } => {
            super::awaits::run_cancel(runtime, store, ask_id, cutex_session_id, jobs, now)
        }
    }
}

/// Capture in the stop transaction before queueing StopAttempt. Never replace
/// an existing intent: repeated actions must preserve dispatch uncertainty.
pub fn capture_stop_intent(state: &mut super::store::AskState, ask_id: &str, attempt: u32, now: DateTime<Utc>) -> bool {
    let Some(ask) = state.asks.get(ask_id) else { return false; };
    let Some(record) = ask.current_attempt().filter(|r| r.n == attempt && !r.state.is_terminal()) else { return false; };
    if let Some(existing) = &record.stop_intent {
        if matches!(existing.outcome, StopDispatchOutcome::Unsupported | StopDispatchOutcome::Stale) {
            state.update_ask(ask_id, now, |ask| ask.current_attempt_mut().unwrap().stop_requested = false);
        }
        return true;
    }
    let intent = AttemptStopIntent {
        cutex_session_id: ask.target_cutex_session_id.clone(), attempt,
        runtime_generation: record.runtime_generation, native_message_id: record.native_message_id.clone(),
        turn_id: record.turn_id.clone(), requested_at: now.to_rfc3339(), claimed_at: None,
        outcome: StopDispatchOutcome::Pending,
    };
    state.update_ask(ask_id, now, |ask| {
        let record = ask.current_attempt_mut().expect("checked attempt");
        record.stop_requested = true;
        record.stop_intent = Some(intent);
    });
    true
}

fn stop_occurrence(state: &super::store::AskState, ask_id: &str, intent: &AttemptStopIntent) -> bool {
    let Some(ask) = state.asks.get(ask_id) else { return false; };
    let Some(record) = ask.current_attempt() else { return false; };
    ask.target_cutex_session_id == intent.cutex_session_id && record.n == intent.attempt
        && record.runtime_generation == intent.runtime_generation && record.native_message_id == intent.native_message_id
        && !record.state.is_terminal() && record.stop_requested
        && (intent.turn_id.is_none() || record.turn_id == intent.turn_id)
        && state.leases.get(&intent.cutex_session_id).is_some_and(|lease|
            lease.cutex_session_id == intent.cutex_session_id
            && lease.holder == (LeaseHolder::Ask { ask_id: ask_id.into(), attempt: intent.attempt })
            && lease.runtime_generation == intent.runtime_generation && lease.native_message_id == intent.native_message_id
            && lease.turn_id == record.turn_id)
}

fn finish_stop(state: &mut super::store::AskState, ask_id: &str, expected: &AttemptStopIntent,
    outcome: StopDispatchOutcome, error: Option<String>, now: DateTime<Utc>) {
    state.update_ask(ask_id, now, |ask| {
        if let Some(record) = ask.attempts.iter_mut().find(|r| r.n == expected.attempt) {
            if record.stop_intent.as_ref() != Some(expected) { return; }
            record.stop_intent.as_mut().unwrap().outcome = outcome;
            if let Some(error) = error { record.last_error = Some(error); }
            if matches!(outcome, StopDispatchOutcome::Unsupported | StopDispatchOutcome::Stale) {
                record.stop_requested = false;
            }
        }
    });
}

/// Acquire a fresh nonblocking authority/TLS scope before AskStore. This is
/// maintenance: no pause predicate or membership reconciliation denies a stop.
fn stop_state<T>(store: &AskStore, run: impl FnOnce(&mut super::store::AskState) -> T)
    -> anyhow::Result<super::pause_admission::Admission<T>> {
    super::pause_admission::try_with_authority(|_| store.with_state(run))
}

/// Exact-occurrence gate-preserving stop. No runtime RPC runs under AskStore
/// or the roster authority fence. Deferred writes preserve original identity.
fn stop_attempt(runtime: &dyn AskRuntime, store: &AskStore, ask_id: &str, attempt: u32, now: DateTime<Utc>) -> anyhow::Result<bool> {
    use super::pause_admission::Admission::{Ready, Deferred};
    let intent = store.inspect(|state| state.asks.get(ask_id)
        .and_then(|ask| ask.attempts.iter().find(|r| r.n == attempt))
        .filter(|r| !r.state.is_terminal()).and_then(|r| r.stop_intent.clone()))?;
    let Some(mut intent) = intent else {
        // No original occurrence proof: never infer today's runtime binding.
        return Ok(matches!(stop_state(store, |state| { state.update_ask(ask_id, now, |ask| {
            if let Some(record) = ask.attempts.iter_mut().find(|r| r.n == attempt && !r.state.is_terminal()) {
                record.stop_requested = false;
                record.last_error = Some("stop_unsupported: original stop intent unavailable".into());
            }
        }); })?, Ready(())));
    };
    if intent.claimed_at.is_some() || intent.outcome != StopDispatchOutcome::Pending {
        // A previous run may have crashed, or lost the authority fence for its
        // result write, after claiming dispatch. Only reconcile uncertainty;
        // never probe or resend that original interrupt.
        if intent.outcome == StopDispatchOutcome::Claimed {
            return Ok(matches!(stop_state(store, |state| finish_stop(state, ask_id, &intent,
                StopDispatchOutcome::Unknown, Some("stop_outcome_unknown: dispatch was claimed; result not durably confirmed".into()), now))?, Ready(())));
        }
        return Ok(true);
    }
    let lane = crate::management::v2::session_coordination::session_lane(&intent.cutex_session_id)?;
    let _operation = match lane.try_lock() {
        Ok(guard) => guard,
        Err(crate::management::v2::session_coordination::ReservationError::WouldBlock) => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    // Late receipt binding is allowed only for the original native key + lease.
    let bound = match stop_state(store, |state| {
        if !stop_occurrence(state, ask_id, &intent) {
            finish_stop(state, ask_id, &intent, StopDispatchOutcome::Stale, Some("stop_stale: original occurrence lost its lease".into()), now);
            return None;
        }
        if intent.turn_id.is_none() {
            let turn = state.asks[ask_id].current_attempt().and_then(|r| r.turn_id.clone());
            if let Some(turn) = turn {
                state.update_ask(ask_id, now, |ask| {
                    let record = ask.current_attempt_mut().unwrap();
                    if record.stop_intent.as_ref() == Some(&intent) { record.stop_intent.as_mut().unwrap().turn_id = Some(turn); }
                });
            }
        }
        state.asks[ask_id].current_attempt().and_then(|r| r.stop_intent.clone())
    })? { Ready(bound) => bound, Deferred => return Ok(false) };
    let Some(bound) = bound else { return Ok(true); };
    intent = bound;
    if intent.claimed_at.is_some() || intent.outcome != StopDispatchOutcome::Pending { return Ok(false); }
    let Some(turn_id) = intent.turn_id.clone() else { return Ok(false); };
    let status = runtime.target_status(&intent.cutex_session_id);
    if !status.online { return Ok(false); }
    if status.runtime_generation != intent.runtime_generation
        || status.active_turn_id.as_deref().is_some_and(|active| active != turn_id) {
        return Ok(matches!(stop_state(store, |state| finish_stop(state, ask_id, &intent,
            StopDispatchOutcome::Stale, Some("stop_stale: runtime occurrence changed".into()), now))?, Ready(())));
    }
    // Missing activity is not affirmative replacement/terminal evidence.
    if status.active_turn_id.is_none() { return Ok(false); }
    let claimed = match stop_state(store, |state| {
        if !stop_occurrence(state, ask_id, &intent) {
            finish_stop(state, ask_id, &intent, StopDispatchOutcome::Stale,
                Some("stop_stale: original occurrence lost its lease before dispatch".into()), now);
            return None;
        }
        let mut claimed = intent.clone();
        claimed.claimed_at = Some(now.to_rfc3339());
        claimed.outcome = StopDispatchOutcome::Claimed;
        state.update_ask(ask_id, now, |ask| {
            let record = ask.current_attempt_mut().unwrap();
            if record.stop_intent.as_ref() != Some(&intent) { return false; }
            record.stop_intent = Some(claimed.clone());
            true
        }).filter(|won| *won).map(|_| claimed)
    })? { Ready(claimed) => claimed, Deferred => return Ok(false) };
    let Some(claimed) = claimed else { return Ok(true); };
    let (outcome, error) = match runtime.interrupt_exact(&intent.cutex_session_id, intent.runtime_generation, &turn_id) {
        Ok(()) => (StopDispatchOutcome::Submitted, None),
        Err(error) => {
            let message = format!("{error:#}");
            if message.contains("exact_interrupt_unsupported") {
                (StopDispatchOutcome::Unsupported, Some("stop_unsupported: exact gate-preserving interrupt unavailable".into()))
            } else {
                (StopDispatchOutcome::Unknown, Some(format!("stop_outcome_unknown: {}", truncate_chars(&message, 200))))
            }
        }
    };
    // If this final write is deferred, the command stays queued with durable
    // Claimed. Next tick reconciles Unknown without another runtime effect.
    Ok(matches!(stop_state(store, |state| finish_stop(state, ask_id, &claimed, outcome, error, now))?, Ready(())))
}

fn respond_question(runtime: &dyn AskRuntime, store: &AskStore, question_id: &str, now: DateTime<Utc>) -> anyhow::Result<bool> {
    let state = store.snapshot()?;
    let Some(question) = state.questions.get(question_id).cloned() else {
        return Ok(true);
    };
    let Some(resolution) = question.resolution.clone().filter(|resolution| !resolution.answers.is_null()) else {
        return Ok(true);
    };
    let Some(ask) = state.asks.get(&question.ask_id) else {
        return Ok(true);
    };
    let result = json!({ "answers": resolution.answers });
    let outcome = runtime.respond_question(
        &ask.target_cutex_session_id,
        question.runtime_generation,
        &format!("ask-{question_id}"),
        &question.native.request_id,
        &result,
    );
    let done = !matches!(outcome, RespondOutcome::NotSent(_));
    if let RespondOutcome::AlreadyResolved | RespondOutcome::Unknown(_) = &outcome {
        let note = match &outcome {
            RespondOutcome::Unknown(error) => format!("delivery outcome unknown: {}", truncate_chars(error, 200)),
            _ => "answered by someone else first".to_string(),
        };
        store.with_state(|state| {
            if let Some(ask) = state.asks.get(&question.ask_id).cloned() {
                state.notice(
                    "cutex/ask/question",
                    super::store::AskState::ask_sessions(&ask),
                    json!({"reason": "responseNotApplied", "questionId": question_id, "note": note}),
                );
            }
        })?;
        let _ = now;
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::scheduler::tests::{world, FakeRuntime};
    use crate::asks::store::tests::{new_ask, temp_store, REQUESTER, TARGET};

    fn interrupt_fixture(label:&str)->(AskStore,std::path::PathBuf,RuntimeCommand) {
        use crate::asks::{project_control as p,routing_policy::{RoutingContext,RoutingActor}};
        let (store,dir)=temp_store(label);
        let command=store.with_state(|state|{
            state.presence.insert(TARGET.into(),crate::asks::store::Presence{online:true,busy:true,
                runtime_generation:Some(1),turn_id:Some("paused-turn".into()),..Default::default()});
            let ctx=RoutingContext{project_id:"pause-fixture".into(),authority_epoch:0,current_director:None,actor:RoutingActor::Owner,operator_may_route:false};
            p::pause(state,&ctx,&p::PauseRequest{action_id:"pause".into(),expected_epoch:0,mode:p::Mode::Interrupt,scope:p::Scope::Members},
                &p::Membership{revision:0,members:[TARGET.into()].into(),excluded:Default::default()},"test-host",Utc::now()).unwrap();
            RuntimeCommand::ProjectPauseInterrupt{project_id:"pause-fixture".into(),pause_id:state.project_control.projects["pause-fixture"].pause_id.clone().unwrap(),
                cutex_session_id:TARGET.into(),runtime_generation:1,turn_id:"paused-turn".into()}
        }).unwrap();
        (store,dir,command)
    }

    #[test]
    fn pause_claim_before_rpc_survives_reload_and_does_not_wait_for_offline_runtime() {
        let (store,dir,command)=interrupt_fixture("pause-claim-before-rpc");
        store.with_state(|s|{s.project_control.projects.get_mut("pause-fixture").unwrap().interrupt_unknown.insert(TARGET.into());}).unwrap();
        let runtime=FakeRuntime::idle(&[TARGET]);runtime.set(TARGET,|s|s.online=false);
        let reopened=AskStore::at(&dir);
        assert!(run(&runtime,&reopened,&command,Utc::now()).unwrap());
        assert!(runtime.interrupts.borrow().is_empty());
        assert!(runtime.target_probes.borrow().is_empty());
        let _=std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pause_resume_during_status_probe_prevents_dispatch_claim() {
        let (store,dir,command)=interrupt_fixture("pause-resume-probe");
        let runtime=FakeRuntime::idle(&[TARGET]);runtime.set(TARGET,|s|s.active_turn_id=Some("paused-turn".into()));
        let other=AskStore::at(&dir);
        *runtime.status_hook.borrow_mut()=Some(Box::new(move||{
            other.with_state(|s|{s.project_control.projects.get_mut("pause-fixture").unwrap().pause_id=None;}).unwrap();
        }));
        assert!(run(&runtime,&store,&command,Utc::now()).unwrap());
        assert!(runtime.interrupts.borrow().is_empty());
        assert!(!store.snapshot().unwrap().project_control.projects["pause-fixture"].interrupt_unknown.contains(TARGET));
        let _=std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pause_exact_rpc_failure_is_claimed_and_never_replayed_after_restart() {
        let (store,dir,command)=interrupt_fixture("pause-exact-error");
        let runtime=std::rc::Rc::new(FakeRuntime::idle(&[TARGET]));
        runtime.set(TARGET,|s|s.active_turn_id=Some("paused-turn".into()));
        let changed=runtime.clone();
        *runtime.status_hook.borrow_mut()=Some(Box::new(move||changed.set(TARGET,|s|s.runtime_generation=2)));
        assert!(run(runtime.as_ref(),&store,&command,Utc::now()).unwrap());
        assert!(store.snapshot().unwrap().project_control.projects["pause-fixture"].interrupt_unknown.contains(TARGET));
        runtime.set(TARGET,|s|s.runtime_generation=1);
        assert!(run(runtime.as_ref(),&AskStore::at(&dir),&command,Utc::now()).unwrap());
        assert!(runtime.interrupts.borrow().is_empty());
        let _=std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pause_interrupt_claim_survives_replay_and_never_targets_replacement_generation(){
        use crate::asks::{project_control as p,routing_policy::{RoutingContext,RoutingActor}};
        let (store,dir)=temp_store("pause-interrupt-claim");
        let runtime=FakeRuntime::idle(&[TARGET]);
        runtime.set(TARGET,|s|s.active_turn_id=Some("paused-turn".into()));
        let now=Utc::now();
        let command=store.with_state(|state|{
            state.presence.insert(TARGET.into(),super::super::store::Presence{online:true,busy:true,runtime_generation:Some(1),turn_id:Some("paused-turn".into()),..Default::default()});
            let ctx=RoutingContext{project_id:"pause-fixture".into(),authority_epoch:0,current_director:None,actor:RoutingActor::Owner,operator_may_route:false};
            p::pause(state,&ctx,&p::PauseRequest{action_id:"pause".into(),expected_epoch:0,mode:p::Mode::Interrupt,scope:p::Scope::Members},
                &p::Membership{revision:0,members:[TARGET.into()].into(),excluded:Default::default()},"test-host",now).unwrap();
            RuntimeCommand::ProjectPauseInterrupt{project_id:"pause-fixture".into(),pause_id:state.project_control.projects["pause-fixture"].pause_id.clone().unwrap(),cutex_session_id:TARGET.into(),runtime_generation:1,turn_id:"paused-turn".into()}
        }).unwrap();
        assert!(run(&runtime,&store,&command,now).unwrap());
        assert!(run(&runtime,&store,&command,now).unwrap());
        assert_eq!(runtime.interrupts.borrow().len(),1);
        runtime.set(TARGET,|s|s.runtime_generation=2);
        assert!(run(&runtime,&store,&command,now).unwrap());
        assert_eq!(runtime.interrupts.borrow().len(),1);
        let _=std::fs::remove_dir_all(dir);
    }

    /// Acceptance §12.3 item 5 (part): a requester stop of a running ask
    /// interrupts only that turn with the gate-preserving interrupt; the
    /// ask ends interrupted(stopped) and closed; other asks still run.
    #[test]
    fn requester_stop_interrupts_only_that_turn() {
        let (world, dir) = world("stop");
        let (store, store_dir) = temp_store("stop");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        let (first, second) = store
            .with_state(|state| {
                (state.create_ask(new_ask("a", "x"), now).unwrap().0.ask_id, state.create_ask(new_ask("b", "y"), now).unwrap().0.ask_id)
            })
            .unwrap();
        crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
        let stopped = crate::asks::tools::close_ask(&store, Some(REQUESTER), &first, "stop", now).unwrap();
        assert_eq!(stopped["stopping"], true);
        crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
        assert_eq!(runtime.interrupts.borrow().as_slice(), [format!("{TARGET}:turn-1")]);
        crate::asks::observe::observe(&store, TARGET, "turn/completed", &json!({"turn": {"id": "turn-1", "status": "interrupted"}}), now).unwrap();
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&first].attempts[0].interrupt_reason, Some(InterruptReason::Stopped));
        assert_eq!(state.asks[&first].closed, Some(CloseOutcome::Stopped));
        assert!(state.commands.is_empty());
        runtime.set(TARGET, |status| status.active_turn_id = None);
        assert_eq!(crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap().admitted, vec![second]);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    #[test]
    fn requester_answers_reach_the_native_request() {
        let (world, dir) = world("respond");
        let (store, store_dir) = temp_store("respond");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        store.with_state(|state| state.create_ask(new_ask("a", "x"), now).unwrap()).unwrap();
        crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
        let question = store
            .with_state(|state| {
                crate::asks::questions::record_question(state, TARGET, 1, &json!(42), &json!({"turnId": "turn-1", "questions": []}), now)
            })
            .unwrap()
            .unwrap();
        crate::asks::questions::respond_in_store(
            &store,
            &question,
            &crate::asks::questions::Responder::Agent(REQUESTER.into()),
            &json!({"q0": {"answers": ["yes"]}}),
            now,
        )
        .unwrap();
        crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
        let responses = runtime.responses.borrow();
        assert_eq!(responses[0].1, json!(42));
        assert_eq!(responses[0].2["answers"]["q0"]["answers"][0], "yes");
        assert!(store.snapshot().unwrap().commands.is_empty());
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }
}

#[cfg(test)]
#[path = "commands_stop_tests.rs"]
mod stop_tests;
