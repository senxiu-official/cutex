use super::*;
use crate::asks::{project_control as p, routing_policy::{RoutingActor, RoutingContext}};
use chrono::Utc;
use serde_json::json;

fn pause(state: &mut AskState, project: &str, members: &[&str], mode: p::Mode) -> String {
    let context = RoutingContext { project_id: project.into(), authority_epoch: 0,
        current_director: None, actor: RoutingActor::Owner, operator_may_route: false };
    p::pause(state, &context, &p::PauseRequest { action_id: format!("pause-{project}"),
        expected_epoch: 0, mode, scope: p::Scope::Members }, &p::Membership {
        revision: 1, members: members.iter().map(|x| x.to_string()).collect(), excluded: Default::default(),
    }, "test-host", Utc::now()).unwrap()["runControl"]["pauseId"].as_str().unwrap().to_owned()
}

fn join(roster: &mut AgentManagementSnapshot, session: &str, project: &str) {
    roster.store_revision += 1;
    roster.current_project_memberships.insert(crate::role_revision::CutexSessionId::new(session.to_owned()).unwrap(),
        serde_json::from_value(json!({"cutex_session_id":session,"project_id":project,
            "revision":roster.store_revision,"updated_at":"2026-10-27T00:00:00Z"})).unwrap());
}

#[test]
fn final_reservation_reconciles_new_join_and_persists_held_reason() {
    let (store, dir) = crate::asks::store::tests::temp_store("pause-new-join");
    store.with_state(|state| { pause(state, "p", &["cutex.old"], p::Mode::AfterTurn); }).unwrap();
    assert!(!store.inspect(|s| p::is_paused(s, "cutex.new")).unwrap()); // stale preflight
    let mut roster = AgentManagementSnapshot::empty();
    join(&mut roster, "cutex.new", "p");
    join(&mut roster, "cutex.other", "q");
    let claimed = with_state_in(&store, &roster, |state, _| {
        if crate::asks::agent_pause::is_paused(state, "cutex.new") { return false; }
        state.owner_offline.insert("must-not-reserve".into()); true
    }).unwrap();
    assert!(!claimed);
    let s = store.snapshot().unwrap();
    assert!(!s.owner_offline.contains("must-not-reserve"));
    assert!(s.presence["cutex.new"].paused);
    assert_eq!(p::reasons(&s, "cutex.new").len(), 1);
    assert!(!p::is_paused(&s, "cutex.other"));
    assert!(!crate::asks::agent_pause::is_paused(&s, "cutex.other"));
    assert!(store.snapshot().unwrap().project_control.projects["p"].members.contains("cutex.new"));
    drop(s); let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn pause_created_after_preflight_is_seen_by_final_transaction() {
    let (store, dir) = crate::asks::store::tests::temp_store("pause-final-not-preflight");
    assert!(!store.inspect(|s| p::is_paused(s, "cutex.member")).unwrap());
    store.with_state(|state| { pause(state, "p", &[], p::Mode::AfterTurn); }).unwrap();
    let mut roster = AgentManagementSnapshot::empty(); join(&mut roster, "cutex.member", "p");
    assert!(with_state_in(&store, &roster, |state, _| p::is_paused(state, "cutex.member")).unwrap());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn moved_member_stays_held_and_resuming_one_project_preserves_another() {
    let mut state = AskState::default();
    let id = pause(&mut state, "p", &["cutex.member"], p::Mode::AfterTurn);
    pause(&mut state, "q", &["cutex.member"], p::Mode::AfterTurn);
    let mut roster = AgentManagementSnapshot::empty(); join(&mut roster, "cutex.member", "elsewhere");
    reconcile_in(&mut state, &roster);
    assert_eq!(p::reasons(&state, "cutex.member").len(), 2);
    let ctx = RoutingContext { project_id: "p".into(), authority_epoch: 0,
        current_director: None, actor: RoutingActor::Owner, operator_may_route: false };
    p::resume(&mut state, &ctx, &p::ResumeRequest { action_id: "resume".into(), pause_id:id, expected_epoch:1 }, "test-host", Utc::now()).unwrap();
    assert!(state.presence["cutex.member"].paused);
    assert_eq!(p::reasons(&state, "cutex.member").len(), 1);
    assert!(!p::is_paused(&state, "cutex.unrelated"));
}

#[test]
fn temporary_role_exclusion_retains_history_and_recomputes_presence() {
    let mut state = AskState::default(); pause(&mut state, "p", &["cutex.member"], p::Mode::AfterTurn);
    let mut membership = p::Membership { revision:2, members:["cutex.member".into()].into(), excluded:["cutex.member".into()].into() };
    p::reconcile(&mut state, "p", &membership);
    assert!(!p::is_paused(&state, "cutex.member")); assert!(!state.presence["cutex.member"].paused);
    assert!(state.project_control.projects["p"].members.contains("cutex.member"));
    membership.members.clear(); membership.excluded.clear(); membership.revision=3;
    p::reconcile(&mut state, "p", &membership);
    assert!(p::is_paused(&state, "cutex.member")); assert!(state.presence["cutex.member"].paused);
}

#[test]
fn after_turn_reconciliation_never_queues_interrupt_or_starts_runtime() {
    let mut state = AskState::default(); pause(&mut state, "p", &[], p::Mode::AfterTurn);
    state.presence.entry("cutex.member".into()).or_default().busy=true;
    let mut roster = AgentManagementSnapshot::empty(); join(&mut roster,"cutex.member","p");
    reconcile_in(&mut state,&roster);
    assert!(state.commands.is_empty()); assert!(!state.presence["cutex.member"].online);
    assert!(p::is_paused(&state,"cutex.member"));
}

#[test]
fn interrupt_new_join_uses_exact_occurrence_once_and_missing_proof_can_recover() {
    use crate::asks::store::{Presence,RuntimeCommand};
    let mut state=AskState::default(); pause(&mut state,"p",&[],p::Mode::Interrupt);
    state.presence.insert("cutex.member".into(),Presence{busy:true,..Default::default()});
    let mut roster=AgentManagementSnapshot::empty(); join(&mut roster,"cutex.member","p");
    reconcile_in(&mut state,&roster);
    assert!(state.commands.is_empty());
    assert!(!state.project_control.projects["p"].interrupt_unknown.contains("cutex.member"));
    let presence=state.presence.get_mut("cutex.member").unwrap();
    presence.runtime_generation=Some(7);presence.turn_id=Some("turn-7".into());
    reconcile_in(&mut state,&roster); reconcile_in(&mut state,&roster);
    assert_eq!(state.commands.len(),1);
    assert!(matches!(&state.commands[0].command,RuntimeCommand::ProjectPauseInterrupt{runtime_generation:7,turn_id,..} if turn_id=="turn-7"));
    state.project_control.projects.get_mut("p").unwrap().interrupt_unknown.insert("cutex.member".into());
    state.commands.clear();
    state.presence.get_mut("cutex.member").unwrap().turn_id=Some("unrelated-next-turn".into());
    reconcile_in(&mut state,&roster);
    assert!(state.commands.is_empty(),"a claimed interruption must not target a later turn");
}

#[test]
fn interrupt_can_bind_exact_lease_when_presence_is_missing() {
    use crate::asks::model::{AdmissionLease,LeaseHolder};
    let mut state=AskState::default();pause(&mut state,"p",&[],p::Mode::Interrupt);
    state.leases.insert("cutex.member".into(),AdmissionLease{cutex_session_id:"cutex.member".into(),holder:LeaseHolder::Owner,
        native_message_id:"owner-message".into(),runtime_generation:4,turn_id:Some("live-turn".into()),since:Utc::now().to_rfc3339()});
    let mut roster=AgentManagementSnapshot::empty();join(&mut roster,"cutex.member","p");
    reconcile_in(&mut state,&roster);
    assert_eq!(state.commands.len(),1);
    assert!(p::is_paused(&state,"cutex.member"));
}

#[test]
fn pure_pause_predicate_does_not_wait_on_the_roster_state_lock() {
    let roster=crate::agent_management::AgentManagementStore::open_default().unwrap();
    let lock=std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .open(roster.root().join("agent-management-v1.lock")).unwrap();
    fs2::FileExt::lock_exclusive(&lock).unwrap();
    let mut state=AskState::default();pause(&mut state,"p",&["cutex.member"],p::Mode::AfterTurn);
    let (tx,rx)=std::sync::mpsc::channel();
    let worker=std::thread::spawn(move||{tx.send((p::is_paused(&state,"cutex.member"),p::is_paused(&state,"cutex.other"))).unwrap();});
    let result=rx.recv_timeout(std::time::Duration::from_secs(2));
    drop(lock);worker.join().unwrap();
    assert_eq!(result.unwrap(),(true,false));
}

#[test]
fn lifecycle_can_join_admission_worker_while_holding_mutation_and_ask_locks() {
    let (store,dir)=crate::asks::store::tests::temp_store("pause-fence-order");
    store.with_state(|_|()).unwrap();
    let roster=crate::agent_management::AgentManagementStore::open_default().unwrap();
    let fence=roster.lock_mutations().unwrap();
    let other=AskStore::at(&dir);
    let (done_tx,done_rx)=std::sync::mpsc::channel();
    // Model the lifecycle's join: it holds authority while the bridge worker
    // enters admission. Even a held AskStore must not be touched on contention.
    let worker=store.with_state(|_|{
        let worker=std::thread::spawn(move||{
            done_tx.send(try_with_state(&other,|_,_|panic!("deferred callback must not run"))).unwrap();
        });
        assert!(matches!(done_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap().unwrap(),Admission::Deferred));
        worker.join().unwrap();
    });
    drop(fence);
    worker.unwrap();
    assert!(!store.inspect(|s|s.owner_offline.contains("claim")).unwrap());
    assert!(matches!(try_with_state(&store,|state,_|{state.owner_offline.insert("claim".into());}).unwrap(),Admission::Ready(())));
    assert!(store.snapshot().unwrap().owner_offline.contains("claim"));
    let _=std::fs::remove_dir_all(dir);
}

#[test]
fn repeated_fresh_gates_make_no_whole_state_copies_or_persisted_changes() {
    let (store,dir)=crate::asks::store::tests::temp_store("pause-read-only-gates");
    store.with_state(|state| {pause(state,"p",&["cutex.member"],p::Mode::AfterTurn);}).unwrap();
    let before=store.inspect(|s|(s.project_control.clone(),s.presence.clone(),s.commands.clone(),s.revision)).unwrap();
    let copies=crate::asks::store::whole_state_copies_on_this_thread();
    let started=std::time::Instant::now();
    for _ in 0..32 {
        assert_eq!(gate(&store,"cutex.member").unwrap(),Gate::Paused);
        assert_eq!(gate(&store,"cutex.unrelated").unwrap(),Gate::Open);
    }
    let elapsed=started.elapsed();
    assert_eq!(crate::asks::store::whole_state_copies_on_this_thread(),copies);
    assert_eq!(store.inspect(|s|(s.project_control.clone(),s.presence.clone(),s.commands.clone(),s.revision)).unwrap(),before);
    eprintln!("pause_read_only_gate calls=64 whole_state_copies=0 elapsed_us={}",elapsed.as_micros());
    let _=std::fs::remove_dir_all(dir);
}

#[test]
fn fresh_preflight_sees_join_without_installing_reason_and_final_claim_rechecks() {
    let (store,dir)=crate::asks::store::tests::temp_store("pause-pure-new-join");
    store.with_state(|state|{pause(state,"p",&[],p::Mode::AfterTurn);}).unwrap();
    let mut roster=AgentManagementSnapshot::empty();join(&mut roster,"cutex.new","p");
    let copies=crate::asks::store::whole_state_copies_on_this_thread();
    assert!(store.inspect(|s|is_paused_in(s,&roster,"cutex.new")).unwrap());
    assert!(!store.inspect(|s|p::is_paused(s,"cutex.new")).unwrap());
    assert_eq!(crate::asks::store::whole_state_copies_on_this_thread(),copies);
    assert!(!with_state_in(&store,&roster,|s,_|!crate::asks::agent_pause::is_paused(s,"cutex.new")).unwrap());
    assert!(store.inspect(|s|p::is_paused(s,"cutex.new")).unwrap());
    let _=std::fs::remove_dir_all(dir);
}
