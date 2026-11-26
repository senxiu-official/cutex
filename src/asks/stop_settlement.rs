//! Durable stop settlement, separate from runtime interruption and work admission.
use chrono::{DateTime, Duration, Utc};
use super::{model::*, store::{AskState, AskStore}, scheduler::SchedulerWorld};

pub const STOP_DEADLINE_SECS: i64 = 600;

/// A terminal stage has no executing obligation only when its admission lease
/// has also gone. Never release a replacement occurrence's lease on this basis.
pub fn terminal_without_lease(state: &AskState, ask: &AskRecord) -> bool {
    ask.current_attempt().is_some_and(|r| r.state.is_terminal())
        && !state.leases.values().chain(state.next_step_leases.values()).any(|lease|
            matches!(&lease.holder, LeaseHolder::Ask { ask_id, .. } if ask_id == &ask.ask_id))
}

fn cancelled_stop(ask:&AskRecord)->bool {
    !ask.resume_pending() && ask.bounded_continuation.current.as_ref().is_some_and(|c|
        c.state==super::continuation::Phase::Cancelled && c.cancel_reason.as_deref()!=Some("handler_turn_failed"))
}

pub fn ensure_deadline(state: &mut AskState, id: &str, attempt: u32, now: DateTime<Utc>) {
    let Some(record) = state.asks.get(id).filter(|a| a.is_open())
        .and_then(|a| a.current_attempt()).filter(|r| r.n == attempt && r.stop_requested && r.stop_deadline.is_none()) else { return; };
    // Legacy stops have no timestamp of their own: the attempt submission is
    // the conservative durable lower bound, never a fresh rolling grace.
    let since = record.stop_intent.as_ref().and_then(|i| super::parse_time(&i.requested_at))
        .or_else(|| super::parse_time(&record.submitted_at)).unwrap_or(now);
    let deadline = (since + Duration::seconds(STOP_DEADLINE_SECS)).to_rfc3339();
    state.update_ask(id, now, |ask| ask.current_attempt_mut().unwrap().stop_deadline = Some(deadline));
}

pub fn expire(state: &mut AskState, id: &str, attempt: u32, now: DateTime<Utc>) {
    let due = state.asks.get(id).is_some_and(|ask| ask.is_open() && ask.current_attempt().is_some_and(|r|
        r.n == attempt && r.stop_requested && !r.stop_unconfirmed
        && r.stop_deadline.as_deref().and_then(super::parse_time).is_some_and(|at| now >= at)));
    if !due { return; }
    state.update_ask(id, now, |ask| {
        let record = ask.current_attempt_mut().unwrap();
        record.stop_unconfirmed = true;
        record.last_error = Some("stop_unconfirmed: no terminal evidence by the absolute stop deadline".into());
    });
}

/// Preserve the actual attempt and its stage; administrative closure creates
/// neither a synthetic native completion nor an ANSWER.
pub fn close_stage(state: &mut AskState, id: &str, now: DateTime<Utc>, cancel: bool) {
    state.update_ask(id, now, |ask| {
        ask.execution = ExecutionState::Interrupted;
        ask.closed = Some(CloseOutcome::Stopped);
        ask.closed_at = Some(super::now_rfc3339(now));
        ask.hold = None; ask.awaiting = None; ask.resume = None;
        if cancel { ask.bounded_continuation.cancel("requester_stop"); }
        if let Some(record) = ask.current_attempt_mut() { record.stop_requested = false; }
    });
}

pub fn close_cancelled_continuation(state: &mut AskState, id: &str, now: DateTime<Utc>) {
    if state.asks.get(id).is_some_and(|ask| ask.is_open() && terminal_without_lease(state, ask)
        && cancelled_stop(ask)) {
        close_stage(state, id, now, false);
    }
}

/// Runs at startup's first scheduler pass and each tick. Inventory all durable
/// open attempts, including no-lease survivors of a failed retirement hook.
pub fn reconcile(world: &dyn SchedulerWorld, store: &AskStore, now: DateTime<Utc>) -> anyhow::Result<Vec<String>> {
    let snapshot = store.shared()?;
    let candidates: Vec<_> = snapshot.asks.values().filter(|ask| ask.remote.is_none() && ask.is_open()
        && (ask.current_attempt().is_some_and(|r| r.stop_requested)
            || matches!(ask.execution, ExecutionState::Admitted | ExecutionState::Running)))
        .map(|ask| (ask.ask_id.clone(), ask.target_cutex_session_id.clone())).collect();
    if candidates.is_empty() { return Ok(Vec::new()); }
    let directory = world.directory().ok();
    let retired: std::collections::HashSet<_> = candidates.iter().filter(|(_, target)|
        directory.as_ref().is_some_and(|d| d.get(target).is_none_or(|info| info.archived)))
        .map(|(id, _)| id.clone()).collect();
    // Avoid drafting a large unchanged store on every scheduler tick.
    let dirty: Vec<_> = candidates.into_iter().filter(|(id, _)| {
        let ask = &snapshot.asks[id];
        retired.contains(id) || (terminal_without_lease(&snapshot, ask)
            && (ask.current_attempt().is_some_and(|r| r.stop_requested)
                || cancelled_stop(ask)))
            || ask.current_attempt().is_some_and(|r| r.stop_requested && (r.stop_deadline.is_none()
                || (!r.stop_unconfirmed && r.stop_deadline.as_deref().and_then(super::parse_time).is_some_and(|d| now >= d))))
    }).collect();
    if dirty.is_empty() { return Ok(Vec::new()); }
    let mut native_retired = Vec::new();
    let records:Vec<&str>=dirty.iter().map(|(id,_)|id.as_str()).collect();
    store.with_records(&records,|state| {
        for (id, target) in &dirty {
            let Some(ask) = state.asks.get(id).filter(|a| a.is_open()) else { continue; };
            let Some(record) = ask.current_attempt() else { continue; };
            let attempt = record.n;
            if retired.contains(id) {
                if terminal_without_lease(state, ask) {
                    super::scheduler::fail_ask(state, id, "recipient_archived", now);
                } else if !record.state.is_terminal() { native_retired.push((id.clone(), target.clone(), attempt)); }
            } else if terminal_without_lease(state, ask) && (record.stop_requested
                || cancelled_stop(ask)) {
                close_stage(state, id, now, true);
            } else {
                ensure_deadline(state, id, attempt, now);
                expire(state, id, attempt, now);
            }
        }
    })?;
    let mut interrupted=Vec::new();
    for (id, target, attempt) in native_retired {
        if super::scheduler::settle_stopped_attempt(store, &target, &id, attempt, now)? { interrupted.push(id); }
    }
    Ok(interrupted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use crate::asks::{scheduler::tests::{world, FakeRuntime}, store::tests::{temp_store,new_ask,REQUESTER,TARGET}};

    fn stage(store: &AskStore, now: DateTime<Utc>, lease: bool) -> String {
        store.with_state(|s| {
            let id=s.create_ask(new_ask("stage","stage"),now).unwrap().0.ask_id;
            s.update_ask(&id,now,|a|{
                a.execution=ExecutionState::Running;
                let mut r=AttemptRecord::submitting(1,"original-input".into(),1,now.to_rfc3339());
                r.state=AttemptState::Yielded;r.turn_id=Some("original-turn".into());r.ended_at=Some(now.to_rfc3339());
                a.attempts.push(r);
            });
            if lease {s.leases.insert(TARGET.into(),AdmissionLease{cutex_session_id:TARGET.into(),
                holder:LeaseHolder::Ask{ask_id:id.clone(),attempt:1},native_message_id:"original-input".into(),
                runtime_generation:1,turn_id:Some("original-turn".into()),since:now.to_rfc3339()});}
            id
        }).unwrap()
    }

    #[test]
    fn yielded_stop_without_lease_closes_immediately_and_late_final_is_ignored() {
        let (store,dir)=temp_store("yielded-stop");let now=Utc::now();let id=stage(&store,now,false);
        let out=crate::asks::tools::close_ask(&store,None,&id,"stop",now).unwrap();
        assert_eq!(out["stopping"],false);
        let state=store.snapshot().unwrap();let a=&state.asks[&id];
        assert_eq!(a.closed,Some(CloseOutcome::Stopped));assert_eq!(a.attempts[0].state,AttemptState::Yielded);
        assert!(!a.attempts[0].stop_requested);assert!(state.commands.is_empty());assert!(state.answers.is_empty());
        crate::asks::observe::finalize(&store,TARGET,&id,1,&json!({"id":"original-turn","status":"completed"}),None,now).unwrap();
        assert!(store.snapshot().unwrap().answers.is_empty());
        let _=std::fs::remove_dir_all(dir);
    }

    #[test]
    fn yielded_with_lease_waits_and_becomes_unconfirmed_without_interrupting() {
        let (store,dir)=temp_store("yielded-leased");let now=Utc::now();let id=stage(&store,now,true);
        let out=crate::asks::tools::close_ask(&store,None,&id,"stop",now).unwrap();assert_eq!(out["stopping"],true);
        let (world,wd)=world("yielded-leased");
        reconcile(&world,&store,now+Duration::seconds(600)).unwrap();
        let state=store.snapshot().unwrap();assert!(state.asks[&id].is_open());assert!(state.asks[&id].attempts[0].stop_unconfirmed);
        let runtime=FakeRuntime::idle(&[TARGET]);
        for c in &state.commands {crate::asks::commands::run(&runtime,&store,&c.command,now+Duration::seconds(600)).unwrap();}
        assert!(runtime.interrupts.borrow().is_empty());
        let _=std::fs::remove_dir_all(dir);let _=std::fs::remove_dir_all(wd);
    }

    #[test]
    fn orphan_yielded_stop_reconciles_after_reload_without_lease_and_is_idempotent() {
        let (store,dir)=temp_store("orphan-yield");let now=Utc::now();let id=stage(&store,now,false);
        store.with_state(|s|s.update_ask(&id,now,|a|a.current_attempt_mut().unwrap().stop_requested=true)).unwrap();
        let reopened=AskStore::at(&dir);let (world,wd)=world("orphan-yield");
        reconcile(&world,&reopened,now).unwrap();let first=reopened.snapshot().unwrap();
        reconcile(&world,&reopened,now+Duration::seconds(1)).unwrap();let second=reopened.snapshot().unwrap();
        assert_eq!(first.asks[&id].revision,second.asks[&id].revision);assert!(!second.asks[&id].is_open());
        assert!(second.answers.is_empty());assert!(second.commands.is_empty());
        let _=std::fs::remove_dir_all(dir);let _=std::fs::remove_dir_all(wd);
    }

    #[test]
    fn archived_yielded_target_with_failed_hook_settles_without_wake() {
        let (store,dir)=temp_store("retired-yield");let now=Utc::now();let id=stage(&store,now,false);
        let (mut world,wd)=world("retired-yield");world.directory.agents.get_mut(TARGET).unwrap().archived=true;
        reconcile(&world,&store,now).unwrap();let a=store.snapshot().unwrap().asks[&id].clone();
        assert_eq!(a.execution,ExecutionState::Failed);assert!(!a.is_open());assert_eq!(a.attempts[0].last_error.as_deref(),Some("recipient_archived"));
        let _=std::fs::remove_dir_all(dir);let _=std::fs::remove_dir_all(wd);
    }

    #[test]
    fn owner_follow_up_ended_is_idempotent_keeps_stop_evidence_and_never_interrupts_replacement() {
        let (store,dir)=temp_store("followup-end");let now=Utc::now();let id=stage(&store,now,true);
        store.with_state(|s|s.update_ask(&id,now,|a|a.attempts[0].state=AttemptState::Running)).unwrap();
        crate::asks::tools::close_ask(&store,None,&id,"stop",now).unwrap();
        let intent=store.snapshot().unwrap().asks[&id].attempts[0].stop_intent.clone();
        assert!(crate::asks::tools::close_ask(&store,Some(REQUESTER),&id,"followUpEnded",now).is_err());
        crate::asks::tools::close_ask(&store,None,&id,"followUpEnded",now).unwrap();
        let revision=store.snapshot().unwrap().asks[&id].revision;
        crate::asks::tools::close_ask(&store,None,&id,"followUpEnded",now).unwrap();
        let state=store.snapshot().unwrap();assert_eq!(state.asks[&id].revision,revision);
        assert!(state.asks[&id].follow_up_ended);assert_eq!(state.asks[&id].attempts[0].stop_intent,intent);
        let runtime=FakeRuntime::idle(&[TARGET]);
        for c in &state.commands {crate::asks::commands::run(&runtime,&store,&c.command,now).unwrap();}
        assert!(runtime.interrupts.borrow().is_empty());
        crate::asks::observe::finalize(&store,TARGET,&id,1,&json!({"id":"original-turn","status":"completed"}),None,now).unwrap();
        let state=store.snapshot().unwrap();assert!(state.asks[&id].follow_up_ended);assert!(state.answers.is_empty());assert!(state.leases.is_empty());
        let _=std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stop_deadline_does_not_renew_on_repeated_stop_or_dispatch_replay() {
        let (store,dir)=temp_store("deadline-repeat");let now=Utc::now();let id=stage(&store,now,true);
        store.with_state(|s|s.update_ask(&id,now,|a|a.attempts[0].state=AttemptState::Running)).unwrap();
        crate::asks::tools::close_ask(&store,None,&id,"stop",now).unwrap();
        let deadline=store.snapshot().unwrap().asks[&id].attempts[0].stop_deadline.clone();
        crate::asks::tools::close_ask(&store,None,&id,"stop",now+Duration::seconds(599)).unwrap();
        let runtime=FakeRuntime::idle(&[TARGET]);runtime.set(TARGET,|s|s.active_turn_id=Some("original-turn".into()));
        let command=crate::asks::store::RuntimeCommand::StopAttempt{ask_id:id.clone(),attempt:1};
        assert!(crate::asks::commands::run(&runtime,&store,&command,now+Duration::seconds(600)).unwrap());
        assert!(crate::asks::commands::run(&runtime,&store,&command,now+Duration::seconds(601)).unwrap());
        let state=store.snapshot().unwrap();assert_eq!(state.asks[&id].attempts[0].stop_deadline,deadline);
        assert!(state.asks[&id].attempts[0].stop_unconfirmed);assert!(runtime.interrupts.borrow().is_empty());
        assert_eq!(crate::asks::view::summary(&state,&state.asks[&id])["attempt"]["stopUnconfirmed"],true);
        let _=std::fs::remove_dir_all(dir);
    }

    #[test]
    fn every_unproven_stop_has_an_exit_without_lease_turn_or_live_transport() {
        for mode in ["missing-lease","replacement-lease","no-turn","offline","unknown","no-intent"] {
            let (store,dir)=temp_store("stop-exit-matrix");let now=Utc::now();let id=stage(&store,now,true);
            store.with_state(|s|s.update_ask(&id,now,|a|a.attempts[0].state=AttemptState::Running)).unwrap();
            crate::asks::tools::close_ask(&store,None,&id,"stop",now).unwrap();
            store.with_state(|s|match mode {
                "missing-lease"=>{s.leases.remove(TARGET);},
                "replacement-lease"=>{s.leases.get_mut(TARGET).unwrap().holder=LeaseHolder::Owner;},
                "no-turn"=>{
                    s.leases.get_mut(TARGET).unwrap().turn_id=None;
                    s.update_ask(&id,now,|a|{a.attempts[0].turn_id=None;a.attempts[0].stop_intent.as_mut().unwrap().turn_id=None;});
                },
                "unknown"=>{s.update_ask(&id,now,|a|a.attempts[0].stop_intent.as_mut().unwrap().outcome=StopDispatchOutcome::Unknown);},
                "no-intent"=>{s.update_ask(&id,now,|a|a.attempts[0].stop_intent=None);},
                _=>{},
            }).unwrap();
            let runtime=FakeRuntime::idle(&[TARGET]);runtime.set(TARGET,|s|{
                s.active_turn_id=if mode=="no-turn"{None}else{Some("replacement-turn".into())};
                if mode=="offline"{s.online=false;s.reconnectable=true;}
            });
            let command=crate::asks::store::RuntimeCommand::StopAttempt{ask_id:id.clone(),attempt:1};
            crate::asks::commands::run(&runtime,&store,&command,now).unwrap();
            let reopened=AskStore::at(&dir);let (world,wd)=world("stop-exit-matrix");
            reconcile(&world,&reopened,now+Duration::seconds(600)).unwrap();
            crate::asks::commands::run(&runtime,&reopened,&command,now+Duration::seconds(601)).unwrap();
            let state=reopened.snapshot().unwrap();let a=&state.asks[&id];
            assert!(a.is_open()&&a.attempts[0].stop_unconfirmed,"{mode}");
            assert!(runtime.interrupts.borrow().is_empty(),"never interrupt replacement: {mode}");
            assert!(state.answers.is_empty());
            crate::asks::tools::close_ask(&reopened,None,&id,"followUpEnded",now+Duration::seconds(602)).unwrap();
            assert!(reopened.snapshot().unwrap().asks[&id].follow_up_ended);
            let _=std::fs::remove_dir_all(dir);let _=std::fs::remove_dir_all(wd);
        }
    }

    #[test]
    fn repeating_legacy_stop_before_first_tick_uses_original_request_not_reconnect_time() {
        for original_intent in [true,false] {
            let (store,dir)=temp_store("legacy-stop-deadline");let original=Utc::now();let later=original+Duration::hours(1);
            let id=stage(&store,original,true);
            store.with_state(|s|s.update_ask(&id,original,|a|a.attempts[0].state=AttemptState::Running)).unwrap();
            crate::asks::tools::close_ask(&store,None,&id,"stop",original).unwrap();
            store.with_state(|s|s.update_ask(&id,later,|a|{
                a.attempts[0].stop_deadline=None;
                if !original_intent {a.attempts[0].stop_intent=None;}
            })).unwrap();
            crate::asks::tools::close_ask(&store,None,&id,"stop",later).unwrap();
            let deadline=store.snapshot().unwrap().asks[&id].attempts[0].stop_deadline.clone().unwrap();
            assert_eq!(super::super::parse_time(&deadline),Some(original+Duration::seconds(600)));
            let runtime=FakeRuntime::idle(&[TARGET]);
            crate::asks::commands::run(&runtime,&store,&crate::asks::store::RuntimeCommand::StopAttempt{ask_id:id.clone(),attempt:1},later).unwrap();
            assert!(store.snapshot().unwrap().asks[&id].attempts[0].stop_unconfirmed);
            assert!(runtime.interrupts.borrow().is_empty());
            let _=std::fs::remove_dir_all(dir);
        }
    }
}
