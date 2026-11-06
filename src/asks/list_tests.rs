use super::*;
use crate::asks::scheduler::tests::world;
use crate::asks::store::tests::{temp_store, REQUESTER};

struct LinkedAuthorityWorld(crate::asks::scheduler::tests::FakeWorld);
impl crate::asks::scheduler::SchedulerWorld for LinkedAuthorityWorld {
    fn directory(&self) -> Result<crate::asks::directory::Directory,AskError> {Ok(self.0.directory.clone())}
    fn gate_inputs(&self, requester:Option<&str>, target:&str) -> Result<crate::asks::gate::GateInputs,AskError> {
        crate::asks::scheduler::SchedulerWorld::gate_inputs(&self.0,requester,target)
    }
    fn approvals(&self) -> Option<&crate::approvals::ApprovalStore> {Some(&self.0.approvals)}
    fn routing_fixture(&self) -> Option<crate::asks::routing_policy::test_support::AuthorityFixture> {
        let mut fixture=crate::asks::routing_policy::test_support::for_directory(&self.0.directory);
        // Name lookup never grants routing authority. Give this happy-path
        // caller an explicit existing Director role in the receiving project.
        let project=crate::agent_management::ProjectId::new("linked").unwrap();
        fixture.snapshot.projects.insert(project,serde_json::from_value(json!({"project_id":"linked",
            "authorized_director_session":REQUESTER,"authority_epoch":1,"updated_at":"2026-11-06T00:00:00Z"})).unwrap());
        Some(fixture)
    }
}

#[test]
fn linked_bare_name_tell_delegate_and_list_share_resolution() {
    let (mut world, dir) = world("linked-bare-name-reproducer");
    let (store, store_dir) = temp_store("linked-bare-name-reproducer");
    let linked = crate::asks::store::tests::TARGET;
    world.directory = crate::asks::directory::tests::directory(&[
        (REQUESTER, "caller", Some("own")), (linked, "linked-worker", Some("linked"))]);
    let security = serde_json::from_value(json!({"links":[["project:own","project:linked"]]})).unwrap();
    world.directory.boundary = crate::agent_bus::boundary::AgentBoundary::new(&security,
        world.directory.agents.iter().map(|(id,agent)|(id.clone(),agent.project.clone().unwrap())).collect(), Default::default());
    assert!(world.directory.may_interact(REQUESTER,linked));
    store.with_state(|state| {
        for id in [REQUESTER,linked] {
            state.presence.insert(id.into(),crate::asks::store::Presence {online:true,..Default::default()});
        }
    }).unwrap();
    let world=LinkedAuthorityWorld(world);
    for (tool,args) in [
        ("agent_tell",json!({"to":"linked-worker","text":"linked tell"})),
        ("agent_delegate",json!({"target":"linked-worker","text":"linked delegation"})),
        ("agent_list",json!({"name":"linked-worker"})),
    ] {
        let action=format!("linked-name-{tool}-{}",uuid::Uuid::new_v4());
        let send=|_:&TellSend<'_>|Ok(json!({"ok":true}));
        let chain=|_:&str,_:&str,_:&str|None;
        let ctx=ToolContext {store:&store,world:&world,caller:REQUESTER,now:Utc::now(),send:&send,
            wake_chain:&chain,clock:&Utc::now,sleep:&||(),wait:false,facade:FACADE_DELEGATIONS};
        let request=json!({"tool":tool,"arguments":args,"action_id":action});
        let result=handle(&ctx,request.to_string().as_bytes());
        println!("linked bare name {tool}: {result}");
        assert_eq!(result["ok"],true,"{tool}: {result}");
    }
    drop(store); drop(world);
    std::fs::remove_dir_all(store_dir).unwrap();
    if dir.exists() { std::fs::remove_dir_all(dir).unwrap(); }
}

#[test]
fn agent_list_explicit_links_expand_but_grants_and_host_access_only_count() {
    let ids: Vec<_> = (1..=8).map(|n|format!("cutex.40000000-0000-0000-0000-{n:012}")).collect();
    for mode in ["isolated","host"] {
        let entries: Vec<_> = [(REQUESTER,"caller",Some("own"))].into_iter().chain(ids.iter().enumerate().map(|(n,id)|
            (id.as_str(),id.as_str(),Some(match n {0|1=>"own",2|3=>"linked",4|5=>"granted",6=>"host-only",_=>"indirect"})))).collect();
        let mut directory=crate::asks::directory::tests::directory(&entries);
        let security=serde_json::from_value(json!({"cross_project":mode,
            "links":[["project:own","project:linked"],["project:linked","project:indirect"]],
            "external_agents":[{"project":"own","agent":ids[4]}]})).unwrap();
        let memberships=if mode == "host" {Default::default()} else {
            directory.agents.iter().map(|(id,a)|(id.clone(),a.project.clone().unwrap())).collect()
        };
        directory.boundary=crate::agent_bus::boundary::AgentBoundary::new(&security,memberships,Default::default());
        let rows: Vec<_>=directory.reachable(REQUESTER).map(|a|(a.project.as_deref()!=Some("own"),false,a.label(),
            json!({"id":a.cutex_session_id,"name":a.name,"project":a.project,
                "status":if a.cutex_session_id==ids[1] || a.cutex_session_id==ids[3] {"offline"} else {"idle"}}))).collect();
        let page=|args:Value,cursor|list_output::local_page(rows.clone(),&parse(args).unwrap(),Some("own"),&directory,REQUESTER,3,cursor).unwrap();
        let default=page(json!({}),0);
        assert_eq!((default.total,default.offline,default.rows.len()),(4,2,2),"{mode}");
        assert!(default.rows[0].get("project").is_none());
        assert_eq!(default.rows[1]["project"],"linked");
        assert!(default.other_projects.iter().any(|p|p["project"]=="granted" && p["agents_total"]==if mode=="host" {2} else {1}));
        if mode=="host" {
            assert!(default.other_projects.iter().any(|p|p["project"]=="host-only"));
            assert!(default.other_projects.iter().any(|p|p["project"]=="indirect"),"links are not transitive");
        }
        let first=page(json!({"include_offline":true}),0);
        assert_eq!(first.next,Some(3));
        assert_eq!(page(json!({"include_offline":true}),3).rows[0]["status"],"offline");
        let grant=page(json!({"project":"granted"}),0);
        assert_eq!(grant.rows.len(),if mode=="host" {2} else {1});
        assert!(grant.rows.iter().all(|row|row["project"]=="granted"));
        assert!(directory.may_interact(REQUESTER,&ids[4]),"existing communication remains allowed");
    }
}

#[test]
fn agent_list_fixture_66_agents_output() {
    let (mut world, dir) = world("list-fixture-66");
    let (store, store_dir) = temp_store("list-fixture-66");
    let ids: Vec<_> = (0..66).map(|n| format!("cutex.10000000-0000-0000-0000-{n:012}")).collect();
    let names: Vec<_> = (0..66).map(|n| format!("worker-{n:02}")).collect();
    let mut rows = vec![(REQUESTER, "caller", Some("cutex-e"))];
    for (n, (id, name)) in ids.iter().zip(&names).enumerate() {
        rows.push((id.as_str(), name.as_str(), Some(if n < 25 { "cutex-e" } else { "cutex-desktop-side" })));
    }
    world.directory = crate::asks::directory::tests::directory(&rows);
    let security = serde_json::from_value(json!({"links":[["project:cutex-desktop-side","project:cutex-e"]]})).unwrap();
    world.directory.boundary = crate::agent_bus::boundary::AgentBoundary::new(&security,
        world.directory.agents.iter().map(|(id, agent)|(id.clone(), agent.project.clone().unwrap())).collect(), Default::default());
    store.with_state(|state| {
        for (n, id) in ids.iter().enumerate() {
            state.presence.insert(id.clone(), super::super::store::Presence { online:n < 20 || (25..50).contains(&n), ..Default::default() });
        }
    }).unwrap();
    let send = |_: &TellSend<'_>| Ok(json!({"ok":true}));
    let wake_chain = |_: &str, _: &str, _: &str| None;
    let clock = Utc::now;
    let sleep = || {};
    let ctx = ToolContext { store:&store, world:&world, caller:REQUESTER,
        now:crate::asks::parse_time("2026-11-06T00:00:00Z").unwrap(), send:&send, wake_chain:&wake_chain,
        clock:&clock, sleep:&sleep, wait:true, facade:FACADE_DELEGATIONS };
    let reviews = || Ok(json!({"handover":[{"id":"pending-review","state":"pending"}],"start":[],
        "changes":[{"changeId":"old-change","state":"finalized","recipients":ids[..38]}]}));
    let default = agent_list_with_reviews(&ctx,json!({}),reviews).unwrap();
    let all = agent_list_with_reviews(&ctx,json!({"include_offline":true,"limit":200}),reviews).unwrap();
    if let Some(path) = std::env::var_os("AGENT_LIST_FIXTURE_OUTPUT") {
        std::fs::write(path, serde_json::to_vec_pretty(&json!({"default":default,"all":all})).unwrap()).unwrap();
    }
    println!("fixture_66 default_bytes={} default_rows={} all_bytes={} all_rows={}", default.to_string().len(),
        default["agents"].as_array().unwrap().len(), all.to_string().len(), all["agents"].as_array().unwrap().len());
    assert_eq!(default["agents_total"],66);
    assert_eq!(default["agents_offline"],21);
    assert_eq!(default["agents"].as_array().unwrap().len(),45);
    assert_eq!(all["agents"].as_array().unwrap().len(),66);
    assert!(default["offline_hint"].as_str().unwrap().contains("include_offline:true"));
    assert_eq!(default["other_projects"],json!([]));
    assert_eq!(default["projects"],json!([
        {"project":"cutex-e","agents_total":25,"agents_offline":5},
        {"project":"cutex-desktop-side","agents_total":41,"agents_offline":16}]));
    assert!(default["director_reviews"]["changes"].as_array().unwrap().is_empty());
    assert_eq!(default["director_reviews"]["handover"][0]["id"],"pending-review");
    for (n,row) in default["agents"].as_array().unwrap().iter().enumerate() {
        assert_eq!(row.as_object().unwrap().len(),if n < 20 {6} else {7},"{row}");
        if n < 20 {assert!(row.get("project").is_none(),"{row}");}
        else {assert_eq!(row["project"],"cutex-desktop-side","{row}");}
    }
    let query = |args|agent_list_with_reviews(&ctx,args,reviews).unwrap();
    let detail = query(json!({"name":"worker-24"}));
    assert_eq!(detail["agents"].as_array().unwrap().len(),1);
    assert_eq!(detail["agents"][0]["status"],"offline");
    assert_eq!(detail["agents"][0]["profile"],"deepseek");
    assert!(detail["agents"][0].get("provider_runtime").is_some());
    assert!(detail["agents"][0].get("activityStatus").is_some());
    assert_eq!(query(json!({"name":ids[24]}))["agents"],detail["agents"]);
    let foreign = query(json!({"project":"cutex-desktop-side","include_offline":true,"limit":200}));
    assert_eq!(foreign["agents"].as_array().unwrap().len(),41);
    assert!(foreign["agents"].as_array().unwrap().iter().all(|row|row["project"] == "cutex-desktop-side"));
    let own=query(json!({"project":"cutex-e","include_offline":true}));
    assert_eq!(own["agents"].as_array().unwrap().len(),25);
    assert!(own["agents"].as_array().unwrap().iter().all(|row|row.get("project").is_none()));
    let boundary1=query(json!({"include_offline":true,"limit":25}));
    assert!(boundary1["agents"].as_array().unwrap().iter().all(|row|row.get("project").is_none()));
    let boundary2=query(json!({"include_offline":true,"limit":25,"cursor":25}));
    assert_eq!(boundary2["agents"][0]["project"],"cutex-desktop-side");
    assert_eq!(boundary2["next_cursor"],50);
    // Existing profile redaction across domains is not widened by full lookup.
    assert!(query(json!({"name":names[30]}))["agents"][0]["profile"].is_null());
    assert!(world.directory.may_interact(REQUESTER,&ids[30]),"the configured link still authorizes communication");
    let page1 = query(json!({"limit":7,"include_offline":true}));
    assert_eq!(page1["next_cursor"],7);
    let page2 = query(json!({"limit":7,"include_offline":true,"cursor":7}));
    assert_eq!(page2["next_cursor"],14);
    assert_ne!(page1["agents"][0]["id"],page2["agents"][0]["id"]);
    assert!(agent_list_with_reviews(&ctx,json!({"project":"invisible-project"}),reviews).is_err());
    assert!(agent_list_with_reviews(&ctx,json!({"name":"missing-agent"}),reviews).is_err());
    let before = store.snapshot().unwrap();
    query(json!({"project":"cutex-desktop-side"}));
    assert_eq!(store.snapshot().unwrap(),before,"list does not mutate source records");
    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(store_dir);
}

#[test]
fn agent_list_standalone_creator_family_and_invisible_lookup() {
    let (mut world, dir) = world("list-standalone");
    let (store, store_dir) = temp_store("list-standalone");
    let child="cutex.20000000-0000-0000-0000-000000000001";
    let grandchild="cutex.20000000-0000-0000-0000-000000000002";
    let hidden="cutex.20000000-0000-0000-0000-000000000003";
    world.directory=crate::asks::directory::tests::directory(&[(REQUESTER,"caller",None),
        (child,"child",None),(grandchild,"grandchild",None),(hidden,"hidden",Some("hidden-project"))]);
    world.directory.boundary=crate::agent_bus::boundary::AgentBoundary::new(&Default::default(),
        [(hidden.into(),"hidden-project".into())].into(),[(child.into(),REQUESTER.into()),(grandchild.into(),child.into())].into());
    store.with_state(|state| {state.presence.insert(child.into(),super::super::store::Presence{online:true,..Default::default()});}).unwrap();
    let call=|args|super::tests::call(&world,&store,REQUESTER,AGENT_LIST,args,"standalone-list");
    let listed=call(json!({}));
    assert_eq!(listed["agents_total"],2);
    assert_eq!(listed["agents_offline"],1);
    assert_eq!(listed["agents"][0]["name"],"child");
    assert_eq!(call(json!({"include_offline":true}))["agents"].as_array().unwrap().len(),2);
    assert_eq!(call(json!({"name":"grandchild"}))["agents"][0]["status"],"offline");
    assert_eq!(call(json!({"name":hidden}))["code"],"recipient_outside_boundary");
    assert_eq!(call(json!({"project":"hidden-project"}))["code"],"project_not_found");
    assert!(!world.directory.may_interact(REQUESTER,hidden));
    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(store_dir);
}

#[test]
fn agent_list_project_scopes_duplicate_names_without_changing_reach() {
    let own="cutex.30000000-0000-0000-0000-000000000001";
    let other="cutex.30000000-0000-0000-0000-000000000002";
    let mut directory=crate::asks::directory::tests::directory(&[(REQUESTER,"caller",Some("own")),
        (own,"worker",Some("own")),(other,"worker",Some("other"))]);
    let security=serde_json::from_value(json!({"links":[["project:own","project:other"]]})).unwrap();
    directory.boundary=crate::agent_bus::boundary::AgentBoundary::new(&security,
        directory.agents.iter().map(|(id,agent)|(id.clone(),agent.project.clone().unwrap())).collect(),Default::default());
    assert_eq!(list_output::resolve_name(&directory,REQUESTER,"worker",Some("own")).unwrap(),own);
    assert_eq!(list_output::resolve_name(&directory,REQUESTER,"worker",Some("other")).unwrap(),other);
    let expected = directory.resolve(Some(REQUESTER),"worker").map(|agent|agent.cutex_session_id.clone()).map_err(|error|error.code);
    let actual = list_output::resolve_name(&directory,REQUESTER,"worker",None).map_err(|error|error.code);
    assert_eq!(actual,expected,"unscoped name lookup must follow delegate resolution, including after rename integration");
    assert!(directory.may_interact(REQUESTER,other));
    assert_eq!(directory.get(REQUESTER).unwrap().project.as_deref(),Some("own"));
}
