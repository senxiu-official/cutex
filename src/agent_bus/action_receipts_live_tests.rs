//! approval-live-state: awaitingApproval is not final. A delegation under the
//! owner's review answers its live state once reviewed (cache, durable read
//! and an identical retry); an agent_create waiting for an approval says what
//! the owner decided, also after a restart (empty cache).
use super::*;
use crate::approvals::{ApprovalRecord, ApprovalState};

fn approval(id: &str, state: &str, digest: &str) -> ApprovalRecord {
    serde_json::from_value(json!({
        "approvalId":id, "kind":"agent_profile", "reason":"profile_requires_approval",
        "requesterCutexSessionId":"caller", "summary":"create agent quokka with profile Ceobe",
        "details":{}, "requestSha256":digest, "state":state, "createdAt":"2026-11-04T00:00:00Z",
        "note": if state == "denied" { json!("not this profile") } else { Value::Null },
    })).unwrap()
}

fn refusal(action_id: &str, approval_id: &str) -> Value {
    serde_json::to_value(crate::agent_management::AgentManagementResponse {
        schema:crate::agent_management::AgentManagementSchema::V1,
        action_id:crate::agent_management::AgentActionId::new(action_id).unwrap(),
        outcome:crate::agent_management::AgentManagementOutcome::NoWrite {
            code:"approval_required".into(),
            detail:format!("Nothing was done yet: this needs the owner's approval (profile Ceobe). (approval_id {approval_id})")},
    }).unwrap()
}

#[test]
fn a_create_waiting_for_approval_reports_the_owners_decision() {
    let schema = crate::management::v2::schema_export::validator("tool-action-receipt", "receipt").unwrap();
    let q = Query {tool:"agent_create".into(),action_id:"needs-approval".into()};
    let cache: SharedCache = Default::default();
    let first = start_in(cache.clone(),Duration::from_secs(10),"caller",q.clone(),"digest".into(),{
        let reply = refusal("needs-approval","apr-1");
        move || Ok(reply)
    });
    assert_eq!(first["status"],"awaitingApproval","{first}");
    let read = |state: &'static str| query_with(&cache,"caller",&q,
        || Ok(envelope(&q,"notFound",true,None)),
        move |id| (id=="apr-1").then(|| approval("apr-1",state,"d")));
    for (state, status, next) in [
        ("pending","awaitingApproval","has not decided"),
        ("approved","awaitingApproval","repeat the identical agent_create call now"),
        ("denied","failed","do not retry"),
        ("expired","failed","asks the owner again"),
    ] {
        let value = read(state);
        assert_eq!(value["status"],status,"{state}: {value}");
        assert_eq!(value["result"]["approval"]["state"],state);
        assert!(value["result"]["approval"]["next"].as_str().unwrap().contains(next),"{value}");
        assert!(valid_response(&value,"agent_create","needs-approval"),"{value}");
        assert!(schema.is_valid(&value),"{value}");
    }
    assert_eq!(read("denied")["result"]["approval"]["note"],"not this profile");
    // The decision is not cached: a retry still runs the call again.
    assert_eq!(query_in(&cache,"caller",&q,||Ok(envelope(&q,"notFound",true,None)))["status"],"awaitingApproval");
    let retry = start_in(cache.clone(),Duration::from_secs(10),"caller",q.clone(),"digest".into(),
        || Ok(json!({"outcome":{"status":"complete","receipt":{"result":{"kind":"created"}}}})));
    assert_eq!(retry["status"],"completed","{retry}");
    // Once the created agent exists, its durable receipt wins.
    let fresh = Query {tool:"agent_create".into(),action_id:"approved-elsewhere".into()};
    start_in(cache.clone(),Duration::from_secs(10),"caller",fresh.clone(),"digest".into(),{
        let reply = refusal("approved-elsewhere","apr-2");
        move || Ok(reply)
    });
    let created = query_with(&cache,"caller",&fresh,
        || Ok(completed(&fresh,json!({"outcome":{"status":"complete","receipt":{"result":{"kind":"created"}}}}))),
        |_| panic!("an existing action needs no approval lookup"));
    assert_eq!(created["status"],"completed","{created}");
    assert_eq!(query_in(&cache,"caller",&fresh,||panic!("now cached"))["status"],"completed");
}

#[test]
fn after_a_restart_the_approval_of_the_recorded_request_answers() {
    use crate::agent_management::envelope::{ApprovalStrictness, SandboxLevel};
    use crate::agent_management::{CallerDefaults, ProfileFacts, SparseAgentManagementRequest, SparseRequestRecord};
    let sparse: SparseAgentManagementRequest = serde_json::from_value(json!({
        "schema":crate::agent_management::SPARSE_SCHEMA,"action_id":"restarted","operation":"create","agent":{"name":"quokka"}})).unwrap();
    let defaults = CallerDefaults {
        profile: Some("Ceobe".into()),
        cwd: if cfg!(windows) { "F:\\test-1".into() } else { "/work/test-1".into() },
        sandbox: Some(SandboxLevel::WorkspaceWrite), approval: Some(ApprovalStrictness::OnRequest),
        ..Default::default()
    };
    let request = crate::agent_management::resolve_request(&sparse,&defaults,
        |name| Ok(ProfileFacts {name:name.into(),default_model:Some("m".into()),..Default::default()})).unwrap();
    let digest = crate::agent_bus::server::agent_profile_approval_digest(&request);
    let mut snapshot = crate::agent_management::AgentManagementSnapshot::empty();
    snapshot.sparse_requests.insert(request.action_id.clone(), SparseRequestRecord {
        sparse_sha256: sparse.digest().unwrap(),
        caller_cutex_session: crate::role_revision::CutexSessionId::new("caller").unwrap(),
        request, recorded_at: crate::role_revision::Rfc3339::new("2026-11-04T00:00:00Z").unwrap(),
    });
    let q = Query {tool:"agent_create".into(),action_id:"restarted".into()};
    let found = |state: &'static str| {
        let digest = digest.clone();
        move |wanted: &str| (wanted==digest).then(|| approval("apr-9",state,&digest))
    };
    let approved = restart_approval(&snapshot,"caller",&q,found("approved")).unwrap();
    assert_eq!((approved["status"].as_str(),approved["result"]["approval"]["state"].as_str()),(Some("awaitingApproval"),Some("approved")));
    assert_eq!(approved["result"]["approval_id"],"apr-9");
    assert!(valid_response(&approved,"agent_create","restarted"),"{approved}");
    assert_eq!(restart_approval(&snapshot,"caller",&q,found("denied")).unwrap()["status"],"failed");
    assert!(restart_approval(&snapshot,"someone-else",&q,found("approved")).is_none(),"only the caller's request");
    assert!(restart_approval(&snapshot,"caller",&q,|_| None).is_none());
    let delegate = Query {tool:"agent_delegate".into(),action_id:"restarted".into()};
    assert!(restart_approval(&snapshot,"caller",&delegate,found("approved")).is_none());
    // An empty cache (after a restart) reads the same through the durable path.
    let cache: SharedCache = Default::default();
    let value = query_with(&cache,"caller",&q,|| Ok(restart_approval(&snapshot,"caller",&q,found("pending")).unwrap()),|_| None);
    assert_eq!((value["status"].as_str(),value["result"]["approval"]["state"].as_str()),(Some("awaitingApproval"),Some("pending")));
    let _ = ApprovalState::Pending;
}

#[test]
fn a_reviewed_delegation_answers_its_live_state() {
    use crate::asks::model::ExecutionState;
    use crate::asks::scheduler::tests::world;
    use crate::asks::store::tests::{temp_store, REQUESTER};
    use crate::asks::tools::tests::call;
    let (world, dir) = world("receipt-review");
    let (store, store_dir) = temp_store("receipt-review");
    // A read-only requester delegating to a workspace-write target: owner review.
    world.inputs.borrow_mut().requester = Some(crate::asks::gate::tests::party(
        REQUESTER, crate::agent_management::envelope::SandboxLevel::ReadOnly, &[], "chatgpt"));
    let args = json!({"target": "Kimi", "text": "deploy"});
    let gated = call(&world,&store,REQUESTER,crate::asks::tools::AGENT_DELEGATE,args.clone(),"act-review");
    assert_eq!(gated["state"],"pendingReview","{gated}");
    let q = Query {tool:"agent_delegate".into(),action_id:"act-review".into()};
    let cache: SharedCache = Default::default();
    start_in(cache.clone(),Duration::from_secs(10),REQUESTER,q.clone(),"digest".into(),{
        let gated = gated.clone();
        move || Ok(gated)
    });
    let read = || query_with(&cache,REQUESTER,&q,|| delegation_query(&store,REQUESTER,&q),|_| None);
    assert_eq!(read()["status"],"awaitingApproval");
    assert!(crate::asks::tools::reviewed_replay(&store,REQUESTER,"act-review").unwrap().is_none(),"still under review: the stored reply stands");
    assert_eq!(delegation_query(&store,REQUESTER,&q).unwrap()["status"],"awaitingApproval");
    // The owner approves: the scheduler moves it on (here: queued).
    let requester = crate::asks::model::Requester::Agent {cutex_session_id:REQUESTER.into(),name:None};
    let ask_id = store.snapshot().unwrap().find_by_action(&requester,"act-review").unwrap().ask_id.clone();
    store.with_state(|state| state.update_ask(&ask_id,chrono::Utc::now(),|ask| ask.execution=ExecutionState::Queued)).unwrap();
    // Cache, durable read (empty cache: after a restart) and an identical retry.
    let live = read();
    assert_eq!((live["status"].as_str(),live["result"]["state"].as_str()),(Some("completed"),Some("queued")),"{live}");
    assert_eq!(query_in(&cache,REQUESTER,&q,||panic!("now cached"))["result"]["state"],"queued");
    let restarted = query_in(&Default::default(),REQUESTER,&q,|| delegation_query(&store,REQUESTER,&q));
    assert_eq!(restarted["result"]["state"],"queued","{restarted}");
    // The plain replay path (a non-Director's once()) rebuilds the same way.
    let replay = crate::asks::tools::reviewed_replay(&store,REQUESTER,"act-review").unwrap().unwrap();
    assert_eq!((replay["state"].as_str(),replay["created"].as_bool()),(Some("queued"),Some(false)),"{replay}");
    assert!(replay["detail"].as_str().unwrap().contains("review is done"),"{replay}");
    let retry = call(&world,&store,REQUESTER,crate::asks::tools::AGENT_DELEGATE,args,"act-review");
    assert_eq!((retry["state"].as_str(),retry["created"].as_bool()),(Some("queued"),Some(false)),"{retry}");
    assert_eq!(retry["delegation"],gated["delegation"]);
    assert!(retry["detail"].as_str().unwrap().contains("(state queued)"),"{retry}");
    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(store_dir);
}
