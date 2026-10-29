use super::*;
use crate::asks::store::{AskState,tests::new_ask};

#[test]
fn reopening_auto_completed_root_permanently_invalidates_its_old_grants(){
    use crate::asks::{routing_policy::*,model::{ExecutionState,CloseOutcome}};
    let mut state=AskState::default();let now=Utc::now();
    let (ask,_)=state.create_ask(new_ask("completed-root","work"),now).unwrap();
    let host=integration::local_host_id();
    let reference=RoutingRef::new(&host,&ask.ask_id).unwrap();
    let ctx=RoutingContext{project_id:"alpha".into(),authority_epoch:0,current_director:None,actor:RoutingActor::Owner,operator_may_route:false};
    let input=CreateGrantRequest{action_id:"grant-before-close".into(),requester_ref:RoutingRef::new(&host,"member").unwrap(),
        target_refs:vec![RoutingRef::new(&host,"reviewer").unwrap()],root_delegation_ref:reference.clone(),purpose:GrantPurpose::Review,max_uses:1,expires_at:None};
    state.routing.create_grant("old-grant",&ctx,&input,&RootStatus::VerifiedOpen(reference),now).unwrap();
    state.update_ask(&ask.ask_id,now,|a|{a.execution=ExecutionState::Answered;a.closed=Some(CloseOutcome::Completed);a.closed_at=Some(crate::asks::now_rfc3339(now));});
    crate::asks::awaits::reopen(&mut state,&ask.ask_id,None,Some("finish the remaining work"),now).unwrap();
    assert!(state.asks[&ask.ask_id].is_open());
    assert_eq!(state.routing.grants["old-grant"].root_closed_at,Some(now));
}

#[test]
fn real_store_child_creation_racing_stop_cannot_leave_a_runnable_descendant(){
    use crate::asks::{scheduler::{self,tests::{world,FakeRuntime}},store::tests::{temp_store,TARGET,REQUESTER},model::Requester};
    let (world,world_dir)=world("lineage-stop-race");
    let (store,store_dir)=temp_store("lineage-stop-race");
    let runtime=FakeRuntime::idle(&[TARGET]);
    let now=Utc::now();
    let root=store.with_state(|s|s.create_ask(new_ask("race-root","work"),now).unwrap().0.ask_id).unwrap();
    scheduler::tick(&world,&runtime,&store,now).unwrap();
    assert!(store.snapshot().unwrap().leases.contains_key(TARGET));
    let barrier=std::sync::Arc::new(std::sync::Barrier::new(2));
    let (created,stopped)=std::thread::scope(|scope|{
        let child_store=store.clone();let child_barrier=barrier.clone();
        let child=scope.spawn(move||{
            let mut request=new_ask("race-child","review");
            request.requester=Requester::Agent{cutex_session_id:TARGET.into(),name:None};
            request.target_cutex_session_id=REQUESTER.into();
            child_barrier.wait();
            child_store.with_state(|s|s.create_ask(request,now)).unwrap()
        });
        barrier.wait();
        let stopped=stop(&store,None,&root,Cascade::Descendants,false,"race-stop",None,now);
        (child.join().unwrap(),stopped)
    });
    stopped.unwrap();
    let state=store.snapshot().unwrap();
    if let Ok((child,_))=created {
        assert!(!state.asks[&child.ask_id].is_open());
        assert!(check_admission(&state,&crate::asks::routing_policy::integration::local_host_id(),&child.ask_id).is_err());
    } else {
        assert!(!state.asks.values().any(|a|a.action_id=="race-child"));
    }
    drop(state);drop(store);
    let _=std::fs::remove_dir_all(store_dir);
    let _=std::fs::remove_dir_all(world_dir);
}

#[test]
fn cascade_fences_children_and_reopening_root_does_not_revive_them(){
    let mut state=AskState::default();
    let now=Utc::now();
    let host=crate::asks::routing_policy::integration::local_host_id();
    let (root,_)=state.create_ask(new_ask("cascade-root","root"),now).unwrap();
    let (child,_)=state.create_ask(new_ask("cascade-child","child"),now).unwrap();
    let root_ref=DelegationRef::new(&host,&root.ask_id).unwrap();
    let child_ref=DelegationRef::new(&host,&child.ask_id).unwrap();
    // Fixture models metadata previously captured from an authenticated lease.
    let record=state.lineage.records.get_mut(&reference_key(&child_ref)).unwrap();
    record.parent=ParentFact::Verified(ParentOccurrence{reference:root_ref.clone(),attempt_id:format!("{}#1",root.ask_id),owner_session_id:root.target_cutex_session_id.clone(),runtime_generation:1,turn_id:"parent-turn".into(),source_event_id:"parent-event".into()});
    record.root=Some(root_ref.clone());
    let scope=ReadScope{caller:"owner".into(),project:None,permission_revision:1,
        resources:[root_ref.clone(),child_ref.clone()].into_iter().map(|r|(r,NodeAccess{visible:true,read_attempts:true,available:true,stop:true,stop_jobs:true,resume_continuation:false})).collect()};
    let plan=prepare_stop(&state,&state.lineage,&host,&scope,&root_ref,Cascade::default(),false,"cascade",now).unwrap();
    let result=apply_stop(&mut state,plan,1,now).unwrap();
    assert_eq!(result["coverage"],"complete");
    assert_eq!(result["nodeOutcomes"].as_array().unwrap().len(),2);
    assert!(!state.asks[&child.ask_id].is_open());
    let stopped_child=state.asks[&child.ask_id].clone();
    let late_delivery=crate::asks::delivery::record_owner_stopped_delivery(&mut state,&stopped_child,now).unwrap();
    assert!(!crate::asks::delivery::actionable(&state,&state.deliveries[&late_delivery]));
    assert_eq!(state.deliveries[&late_delivery].state,crate::asks::model::DeliveryState::Pending);
    assert!(check_admission(&state,&host,&child.ask_id).is_err());
    let late=LineageRecord{reference:DelegationRef::new(&host,"late-child").unwrap(),parent:ParentFact::Verified(ParentOccurrence{reference:root_ref.clone(),attempt_id:format!("{}#1",root.ask_id),owner_session_id:root.target_cutex_session_id, runtime_generation:1,turn_id:"parent-turn".into(),source_event_id:"late-event".into()}),root:Some(root_ref),follow_up:None};
    assert!(state.lineage.check_creation_fences(&late).is_err());
    explicit_reopen(&mut state,&host,&root.ask_id);
    assert!(check_admission(&state,&host,&root.ask_id).is_ok());
    assert!(check_admission(&state,&host,&child.ask_id).is_err());
}

#[test]
fn creation_root_is_proven_but_old_rows_stay_unknown(){
    let mut state=AskState::default();
    let (ask,_)=state.create_ask(new_ask("root","work"),Utc::now()).unwrap();
    let reference=DelegationRef::new(crate::asks::routing_policy::integration::local_host_id(),&ask.ask_id).unwrap();
    let record=state.lineage.get(&reference).unwrap();
    assert!(matches!(record.parent,ParentFact::None));assert_eq!(record.root.as_ref(),Some(&reference));
    let old=LineageState::default();assert!(old.get(&reference).is_none());
}

#[test]
fn opaque_redaction_does_not_expose_parent_identity(){
    let mut state=AskState::default();
    let (ask,_)=state.create_ask(new_ask("child","visible"),Utc::now()).unwrap();
    let host=crate::asks::routing_policy::integration::local_host_id();
    let reference=DelegationRef::new(&host,&ask.ask_id).unwrap();
    let hidden=DelegationRef::new("hidden-host","hidden-id").unwrap();
    let record=state.lineage.records.get_mut(&reference_key(&reference)).unwrap();
    record.parent=ParentFact::Verified(ParentOccurrence{reference:hidden.clone(),attempt_id:"hidden-attempt".into(),owner_session_id:"hidden-session".into(),runtime_generation:1,turn_id:"hidden-turn".into(),source_event_id:"hidden-event".into()});
    record.root=Some(hidden);
    let scope=ReadScope{caller:"reader".into(),project:None,permission_revision:1,resources:[(reference,NodeAccess{visible:true,available:true,..Default::default()})].into()};
    let mut snap=TreeSnapshot::capture(&state,&state.lineage,&host,&scope,None).unwrap();
    let page=snap.page("reader",1,None,100).unwrap();
    let text=page.to_string();assert!(!text.contains("hidden-"));
    let placeholders:Vec<_>=page["nodes"].as_array().unwrap().iter().filter(|n|n["kind"]=="redactedAncestor").collect();
    assert_eq!(placeholders.len(),2);
    assert!(placeholders.iter().all(|p|p.as_object().unwrap().len()==6));
    assert_ne!(placeholders[0]["placeholderId"],placeholders[1]["placeholderId"]);
    assert!(snap.page("another",1,None,100).is_err());
    assert!(snap.page("reader",2,None,100).is_err());
}
