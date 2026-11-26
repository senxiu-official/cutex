//! The access gate through the production Management dispatcher, on a
//! private instance (no live state, no daemon).
use crate::config::instance::{self, Selection};
use crate::management::{
    client_tokens::ClientTokenStore,
    listeners::{self, ConnectionInfo},
    server::{handle_management_request, ManagementRequestContext},
    users::{Grant, Projects, Scope, UserStore, OWNER},
};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    time::Duration,
};

const ROOT: &str = "access-http-test-root-credential";

#[test]
fn interaction_history_http_reads_submitted_replies_after_disconnect_with_session_scope() {
    use crate::app_server::manager::AppServerManagedRuntimeStatus;
    use crate::management::server::ManagementNativeForwardError;
    use crate::management::v2::server_requests::{clear_session, record_request, resolve_request};

    const SESSION: &str = "cutex.interaction-history";
    const HISTORY: &str = "/v2/sessions/cutex.interaction-history/history/interactions";
    const RESPOND: &str = "/v2/sessions/cutex.interaction-history/app-server/server-request-responses";
    let _home_lock = crate::config::paths::test_home_env_lock();
    let instance = PrivateInstance::new();
    let mut record = crate::session::model::CutexSessionRecord::new(
        SESSION.into(), Some("interaction-thread".into()), crate::platform::host::current_host_name(),
        instance.root.to_string_lossy().into_owned(), None,
    ).unwrap();
    record.runtime_generation = 7;
    record.registration_class = crate::agent_bus::model::AgentRegistrationClass::Persistent;
    let mut sessions = crate::session::model::CutexSessionStore::default();
    sessions.sessions.insert(SESSION.into(), record);
    crate::session::store::save_cutex_session_store(&sessions).unwrap();

    let users = UserStore::open_default().unwrap();
    let tokens = ClientTokenStore::open_default().unwrap();
    let user = users.create_user("History reader", None, Grant {
        standalone_agents: [SESSION.into()].into(), send: true, ..Grant::default()
    }, "root").unwrap();
    let token = tokens.issue("history-reader").unwrap();
    users.bind_token(&token.token_id, Some(&user.id), "root").unwrap();
    let outsider = users.create_user("Outside history", None, Grant::default(), "root").unwrap();
    let outsider_token = tokens.issue("outside-history").unwrap();
    users.bind_token(&outsider_token.token_id, Some(&outsider.id), "root").unwrap();

    let mut ctx = p2_context();
    ctx.load_runtime_status = |session| Ok((session == SESSION).then(|| AppServerManagedRuntimeStatus {
        background_work: None,
        claude_diagnostics: None,
        cutex_session_id: SESSION.into(),
        thread_id: "interaction-thread".into(),
        runtime_generation: 7,
        connected: true,
        active_turn_id: Some("question-turn".into()),
        active_turn_observed_at: None,
        thread_status: None,
        thread_status_observed_at: None,
        thread_settings: None,
        thread_settings_source: None,
        thread_settings_complete: false,
        thread_settings_observed_at: None,
        runtime_workspace_roots: None,
        instruction_sources: None,
        resume_snapshot_observed_at: None,
        event_method_counts: Default::default(),
        initialized_user_agent: None,
        last_event_at: None,
        last_error: None,
    }));
    ctx.respond_native_server_request = |session, generation, reply| {
        assert_eq!((session, generation), (SESSION, 7));
        match reply["id"].as_str() {
            Some("failed") => Err(ManagementNativeForwardError::BeforeForward("fixture".into())),
            Some("uncertain") => Err(ManagementNativeForwardError::OutcomeUnknown("fixture".into())),
            Some("1") => {
                // A disconnect can clear the claim before the writer returns.
                clear_session(session).unwrap();
                Ok(())
            }
            _ => {
                // Resolution may arrive before the Management write returns.
                assert!(resolve_request(session, generation, &reply["id"]).unwrap());
                Ok(())
            }
        }
    };
    let post = |token: &str, body: Value| {
        exchange_with(ctx, "POST", RESPOND, Some(token), body.to_string().into_bytes(), "application/json",
            "If-Match: \"cutex-runtime-generation:7\"\r\n").0
    };
    let question = json!({"id":1,"method":"item/tool/requestUserInput","params":{
        "turnId":"question-turn","questions":[{"id":"q","question":"Which?","options":null},
            {"id":"secret","question":"Private input","isSecret":true,"options":null}] }});
    record_request(SESSION, 7, question.clone()).unwrap();
    let answer = json!({"responseId":"question-reply","native":{"message":{"id":1,"result":{"answers":{
        "q":{"answers":["Windows"]},"secret":{"answers":["fixture-password"]}}}}}});
    assert_eq!(post(&token.token, answer.clone()), 200);
    assert_eq!(post(ROOT, answer), 200, "a retry must keep the original answerer");

    for (id, status) in [("1", 200), ("failed", 502), ("uncertain", 409), ("cancelled", 200), ("abandoned", 0)] {
        record_request(SESSION, 7, json!({"id":id,"method":"item/commandExecution/requestApproval","params":{
            "turnId":"approval-turn","command":"git status","reason":"Inspect worktree"}})).unwrap();
        if id == "abandoned" {
            assert!(resolve_request(SESSION, 7, &json!(id)).unwrap());
            continue;
        }
        let reply = if id == "cancelled" {
            json!({"id":id,"error":{"code":-32800,"message":"cancelled"}})
        } else {
            json!({"id":id,"result":{"decision":"decline"}})
        };
        assert_eq!(post(ROOT, json!({"responseId":id,"native":{"message":reply}})), status);
    }
    clear_session(SESSION).unwrap();

    // The offline context has a registry but panics if asked for native history.
    let get = |path: &str, token: Option<&str>| {
        let (status, _, bytes) = exchange_with(p2_context(), "GET", path, token, vec![], "application/json", "");
        (status, serde_json::from_slice::<Value>(&bytes).unwrap_or_default())
    };
    REMOTE.with(|remote| remote.set(true));
    let unauthenticated = get(HISTORY, None);
    REMOTE.with(|remote| remote.set(false));
    assert_eq!(unauthenticated.0, 401);
    let (status, first) = get(&format!("{HISTORY}?limit=1"), Some(&token.token));
    assert_eq!(status, 200, "{first}");
    assert_eq!(first["contractVersion"], 2);
    assert_eq!(first["interactions"][0]["native"]["id"], "1");
    assert_eq!(first["interactions"][0]["result"], json!({"decision":"decline"}));
    assert_eq!(first["interactions"][0]["actor"], json!({"kind":"owner"}));
    let cursor = first["nextCursor"].as_str().unwrap();
    let (_, older) = get(&format!("{HISTORY}?limit=1&cursor={cursor}"), Some(ROOT));
    assert_eq!(older["interactions"][0]["native"], question);
    assert_eq!(older["interactions"][0]["actor"], json!({"kind":"user","userId":user.id,"name":"History reader"}));
    assert_eq!(older["interactions"][0]["result"], json!({"answers":{"q":{"answers":["Windows"]}}}));
    assert_eq!(older["interactions"][0]["redactedQuestionIds"], json!(["secret"]));
    assert!(chrono::DateTime::parse_from_rfc3339(older["interactions"][0]["answeredAt"].as_str().unwrap()).is_ok());
    assert!(older["nextCursor"].is_null());
    let (_, turn) = get(&format!("{HISTORY}?turnIds=%5B%22question-turn%22%2C%22a-different-turn%22%5D"), Some(ROOT));
    assert_eq!(turn["interactions"], older["interactions"]);
    assert_eq!(get(&format!("{HISTORY}?cursor=broken"), Some(ROOT)).0, 400);
    assert_eq!(get("/v2/sessions/cutex.missing/history/interactions", Some(ROOT)).0, 404);
    let (status, denied) = get(HISTORY, Some(&outsider_token.token));
    assert_eq!(status, 404, "{denied}");
    assert!(denied.get("interactions").is_none());
    crate::management::v2::interaction_history::forget_deleted_session(SESSION).unwrap();
    let (_, empty) = get(HISTORY, Some(ROOT));
    assert_eq!(empty["interactions"], json!([]));
    assert!(empty["nextCursor"].is_null());
}

#[test]
fn scoped_offline_submit_http_never_receives_owner_runtime_review_authority() {
    use std::sync::atomic::{AtomicUsize,Ordering};
    static OWNER_STARTS:AtomicUsize=AtomicUsize::new(0);
    static SCOPED_STARTS:AtomicUsize=AtomicUsize::new(0);
    let _home_lock=crate::config::paths::test_home_env_lock();let instance=PrivateInstance::new();
    let ids=["cutex.offline-http-owner","cutex.offline-http-scoped"];
    let mut sessions=crate::session::model::CutexSessionStore::default();
    for sid in ids {
        let mut record=crate::session::model::CutexSessionRecord::new(sid.into(),Some(uuid::Uuid::new_v4().to_string()),
            crate::platform::host::current_host_name(),instance.root.to_string_lossy().into_owned(),None).unwrap();
        record.registration_class=crate::agent_bus::model::AgentRegistrationClass::Persistent;
        sessions.sessions.insert(sid.into(),record);
    }
    crate::session::store::save_cutex_session_store(&sessions).unwrap();
    let users=UserStore::open_default().unwrap();let tokens=ClientTokenStore::open_default().unwrap();
    let user=users.create_user("Offline sender",None,Grant{standalone_agents:ids.iter().map(|id|(*id).into()).collect(),send:true,..Grant::default()},"root").unwrap();
    let token=tokens.issue_scoped("offline-sender",crate::management::client_tokens::DeviceScope::Chat).unwrap();
    users.bind_token(&token.token_id,Some(&user.id),"root").unwrap();
    OWNER_STARTS.store(0,Ordering::SeqCst);SCOPED_STARTS.store(0,Ordering::SeqCst);
    let mut ctx=p2_context();
    ctx.handle_user_input=|command|{
        use crate::management::v2::user_input::{self,QueueEnqueueDecision,UserInputSubmitExecution,UserInputDisposition};
        let decision=user_input::user_input_repository().unwrap().enqueue(&command.cutex_session_id,&command.thread_id,
            &command.management_request_id,&command.params).unwrap();
        let queue=match decision {QueueEnqueueDecision::Queued(item)|QueueEnqueueDecision::Deduplicated(item)=>item,
            _=>panic!("private input identity unexpectedly conflicted")};
        Ok(UserInputSubmitExecution{disposition:UserInputDisposition::Queued,app_server_accepted:false,
            native_request_id:None,native_method:None,turn_id:None,queue:Some(queue),queue_paused: false, passive_notes: None,join_refusal:None,queued_reason:None})
    };
    ctx.online_session=|owner,_,_|{
        if owner.is_some(){OWNER_STARTS.fetch_add(1,Ordering::SeqCst);}else{SCOPED_STARTS.fetch_add(1,Ordering::SeqCst);}
        Err(crate::management::v2::user_input::UserInputExecutionError{stage:"route".into(),code:"fixture_no_runtime_effect".into(),
            message:"record authority only; no runtime launched".into(),retryable:false,outcome_unknown:false,details:json!({})})
    };
    for (n,bearer) in [ROOT,token.token.as_str()].into_iter().enumerate() {
        let sid=ids[n];
        let body=json!({"requestId":format!("offline-http-{n}"),"method":"cutex/userInput/submit","params":{
            "clientUserMessageId":format!("offline-input-{n}"),"origin":{"kind":"other","clientId":"private-test"},
            "strategy":"auto","startIfOffline":true,"input":[{"type":"text","text":"private fixture"}]}});
        let(status,_,bytes)=exchange_with(ctx,"POST",&format!("/v2/sessions/{sid}/cutex/requests"),Some(bearer),
            body.to_string().into_bytes(),"application/json","");
        assert_eq!(status,200,"{}",String::from_utf8_lossy(&bytes));
        let deadline=std::time::Instant::now()+Duration::from_secs(5);
        while !matches!(super::super::super::offline_submit::start_outcome(sid),Some(super::super::super::offline_submit::StartOutcome::Failed{..})) {
            assert!(std::time::Instant::now()<deadline,"background authority callback did not settle");std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(OWNER_STARTS.load(Ordering::SeqCst),1,"scoped request inherited an owner-only runtime review proof");
        assert_eq!(SCOPED_STARTS.load(Ordering::SeqCst),n);
    }
    // A scoped offline steer must not acquire the owner-only automatic
    // review/run path either. Keeping the item queued is safe when no live
    // runtime can be reconnected.
    let _ledger=crate::management::v2::input_ledger::pin_enabled(Some(true));
    ctx.steer_user_input_queue=|_,_,_|Err(crate::management::v2::user_input::UserInputExecutionError{
        stage:"route".into(),code:"session_offline".into(),message:"private offline fixture".into(),
        retryable:true,outcome_unknown:false,details:json!({})});
    let sid=ids[1];let queue=crate::management::v2::user_input::user_input_repository().unwrap();
    let item=queue.list(sid).unwrap().1.remove(0);
    let body=json!({"requestId":"scoped-offline-steer","method":"cutex/userInput/queue/steer","params":{
        "queueId":item.queue_id,"expectedRevision":item.revision,"ifIdle":"start"}});
    let(status,_,bytes)=exchange_with(ctx,"POST",&format!("/v2/sessions/{sid}/cutex/requests"),Some(&token.token),
        body.to_string().into_bytes(),"application/json","");
    let response:Value=serde_json::from_slice(&bytes).unwrap();assert_ne!(status,200,"{response}");
    assert_eq!(response["error"]["code"],"session_offline");
    assert_ne!(response["error"]["details"]["runtimeStarting"],true);
    assert_eq!(OWNER_STARTS.load(Ordering::SeqCst),1);
    assert_eq!(SCOPED_STARTS.load(Ordering::SeqCst),1);
    assert_eq!(queue.list(sid).unwrap().1.len(),1);
}

#[test]
fn scoped_stop_http_provenance_survives_client_token_bearer_normalization() {
    use crate::management::client_tokens::DeviceScope;
    let _home_lock=crate::config::paths::test_home_env_lock();
    let instance=PrivateInstance::new();
    let sid="cutex.stop-http-principal";
    let mut sessions=crate::session::model::CutexSessionStore::default();
    let mut record=crate::session::model::CutexSessionRecord::new(sid.into(),None,
        crate::platform::host::current_host_name(),instance.root.to_string_lossy().into_owned(),None).unwrap();
    record.registration_class=crate::agent_bus::model::AgentRegistrationClass::Persistent;
    record.exposed_to_backend=false;
    sessions.sessions.insert(sid.into(),record);
    crate::session::store::save_cutex_session_store(&sessions).unwrap();
    let users=UserStore::open_default().unwrap();let tokens=ClientTokenStore::open_default().unwrap();
    let sender=users.create_user("Scoped Stop sender",None,Grant{standalone_agents:vec![sid.into()],send:true,..Grant::default()},"root").unwrap();
    let chat=tokens.issue_scoped("scoped-stop-chat",DeviceScope::Chat).unwrap();
    let full=tokens.issue("scoped-stop-full-device").unwrap();
    let owner=tokens.issue_scoped("owner-stop-chat",DeviceScope::Chat).unwrap();
    for token in [&chat,&full] {users.bind_token(&token.token_id,Some(&sender.id),"root").unwrap();}
    users.bind_token(&owner.token_id,Some(OWNER),"root").unwrap();
    let store=crate::asks::store::AskStore::open_default().unwrap();
    // Alternate owner/scoped requests on one session: the scoped request
    // must revoke an earlier owner's proof, including on a full device.
    for (index,(bearer,is_owner)) in [(ROOT,true),(chat.token.as_str(),false),
        (owner.token.as_str(),true),(full.token.as_str(),false)].into_iter().enumerate() {
        let (status,_,bytes)=exchange_with(p2_context(),"GET","/v2/capabilities",Some(bearer),vec![],"application/json","");
        let capabilities:Value=serde_json::from_slice(&bytes).unwrap();assert_eq!(status,200,"{capabilities}");
        assert_eq!(capabilities["user"]["isOwner"],is_owner);
        let body=json!({"requestId":format!("stop-principal-{index}"),"method":"cutex/turn/stop","params":{}});
        let (status,_,bytes)=exchange_with(p2_context(),"POST",&format!("/v2/sessions/{sid}/cutex/requests"),
            Some(bearer),body.to_string().into_bytes(),"application/json","");
        let response:Value=serde_json::from_slice(&bytes).unwrap();assert_eq!(status,200,"{response}");
        let state=store.snapshot().unwrap();let pause=&state.pauses[sid];
        assert!(pause.stop_hold);assert_eq!(pause.stop_answer_authorized,is_owner,"{response}");
        assert_eq!(pause.origin.as_deref(),Some(crate::asks::control::ORIGIN_OWNER_STOP));
    }
}

#[test]
fn path_reapproval_requires_full_owner_and_persists_a_fresh_snapshot() {
    use crate::management::client_tokens::DeviceScope;
    use crate::session::model::{CutexSessionRecord, CutexSessionStore, SessionCreatedBy};
    let _home_lock = crate::config::paths::test_home_env_lock();
    let instance = PrivateInstance::new();
    let cwd = instance.root.join("workspace"); std::fs::create_dir_all(&cwd).unwrap();
    let mut child = CutexSessionRecord::new("cutex.reapprove".into(), None, "test".into(), cwd.to_str().unwrap().into(), None).unwrap();
    child.created_by = Some(SessionCreatedBy::Agent { cutex_session_id: crate::role_revision::CutexSessionId::new("cutex.creator").unwrap(), role: None });
    let id = child.cutex_session_id.clone();
    let mut sessions = CutexSessionStore::default(); sessions.sessions.insert(id.clone(), child);
    crate::session::store::save_cutex_session_store(&sessions).unwrap();
    let path = format!("/v2/sessions/{id}/reapprove-paths");
    let tokens = ClientTokenStore::open_default().unwrap();
    let users = UserStore::open_default().unwrap();
    for scope in [DeviceScope::Read, DeviceScope::Chat] {
        let token = tokens.issue_scoped("reapprove-narrowed", scope).unwrap();
        users.bind_token(&token.token_id, Some(OWNER), "root").unwrap();
        assert_eq!(exchange("POST", &path, Some(&token.token), &json!({})).0, 403);
    }
    let member = users.create_user("Reapprove member", None, Grant::default(), "root").unwrap();
    let token = tokens.issue("reapprove-member").unwrap();
    users.bind_token(&token.token_id, Some(&member.id), "root").unwrap();
    assert_eq!(exchange("POST", &path, Some(&token.token), &json!({})).0, 403);
    assert!(matches!(exchange("POST", &path, Some("ordinary-agent-bus-credential"), &json!({})).0, 401 | 403));
    // A valid ordinary Management/Agent Bus bridge bearer is insufficient,
    // even though that same bearer can read a normal bridge resource.
    const BUS: &str = "valid-agent-bus-bridge-credential";
    let bus_call = |method: &str, path: &str| exchange_with_tokens(context(), method, path, Some(BUS), b"{}".to_vec(), "application/json", "", Some(BUS));
    assert_eq!(bus_call("GET", "/v2/capabilities").0, 200);
    assert_eq!(bus_call("POST", &path).0, 403);
    assert!(crate::session::store::load_cutex_session_store().unwrap().sessions[&id].creator_path_snapshot.is_none());
    let (status, result) = exchange("POST", &path, Some(ROOT), &json!({}));
    assert_eq!(status, 200, "{result}");
    assert_eq!(result["cutexSessionId"], id);
    assert_eq!(result["ownerApproved"], true);
    crate::management::v2::schema_export::assert_valid("session-paths", "response", &result);
    assert_eq!(result["creatorPathSnapshot"]["owner_approved"], true);
    let saved = crate::session::store::load_cutex_session_store().unwrap().sessions[&id].clone();
    assert!(saved.creator_path_snapshot.unwrap().owner_approved);
    let owner = tokens.issue("reapprove-full-owner").unwrap();
    users.bind_token(&owner.token_id, Some(OWNER), "root").unwrap();
    assert_eq!(exchange("POST", &path, Some(&owner.token), &json!({})).0, 200);
    assert_eq!(exchange("POST", &path, Some(ROOT), &json!({"ownerApproved":true})).0, 400);
    assert_eq!(exchange("GET", &path, Some(ROOT), &json!({})).0, 405);
    let capabilities = crate::management::v2::capabilities::capabilities_resource();
    assert!(capabilities["featureList"].as_array().unwrap().iter().any(|value| value == "session-path-reapproval"));
}
#[path = "archive_api_http_tests.rs"]
mod archive_api_http_tests;
// r290 P10 (A1)
#[path = "project_action_receipts_http_tests.rs"]
mod project_action_receipts_http_tests;
#[path = "user_layout_http_tests.rs"]
mod user_layout_http_tests;

#[test]
fn media_attachments_and_pending_requests_follow_session_read_grants() {
    use crate::management::client_tokens::DeviceScope;
    let _home_lock=crate::config::paths::test_home_env_lock();
    let instance=PrivateInstance::new();
    let host=crate::platform::host::current_host_name();
    let mut sessions=crate::session::model::CutexSessionStore::default();
    for id in ["cutex.blob-a","cutex.blob-b","cutex.blob-hidden"] {
        let mut session=crate::session::model::CutexSessionRecord::new(id.into(),None,host.clone(),instance.root.to_string_lossy().into_owned(),None).unwrap();
        session.exposed_to_backend=true;
        session.registration_class=crate::agent_bus::model::AgentRegistrationClass::Persistent;
        sessions.sessions.insert(id.into(),session);
    }
    crate::session::store::save_cutex_session_store(&sessions).unwrap();
    let tokens=ClientTokenStore::open_default().unwrap();
    let users=UserStore::open_default().unwrap();
    let reader=users.create_user("Blob reader",None,Grant {standalone_agents:vec!["cutex.blob-a".into(),"cutex.blob-b".into()],..Grant::default()},"root").unwrap();
    let token=tokens.issue_scoped("blob-reader",DeviceScope::Read).unwrap();
    users.bind_token(&token.token_id,Some(&reader.id),"root").unwrap();
    let blobs=crate::management::v2::event_blobs::EventBlobStore::open_default().unwrap();
    let descriptor=blobs.put("cutex.blob-a","application/octet-stream",b"image bytes").unwrap();
    blobs.put("cutex.blob-hidden","application/octet-stream",b"image bytes").unwrap();
    let id=descriptor["blobId"].as_str().unwrap();
    let get=|session:&str,bearer:Option<&str>|exchange_with(p2_context(),"GET",&format!("/v2/sessions/{session}/blobs/{id}"),bearer,b"{}".to_vec(),"application/json","");
    let (status,_,body)=get("cutex.blob-a",Some(&token.token));
    assert_eq!(status,200,"{}",String::from_utf8_lossy(&body));
    assert_eq!(body,b"image bytes");
    assert_eq!(get("cutex.blob-a",None).0,401);
    assert_eq!(get("cutex.blob-b",Some(&token.token)).0,404,"no grant for the same hash in this visible session");
    assert_eq!(get("cutex.blob-hidden",Some(&token.token)).0,404,"hidden session stays hidden even with a blob grant");
    for (session, status) in [("cutex.blob-a",200),("cutex.blob-b",404),("cutex.blob-hidden",404)] {
        let (actual, _, bytes) = exchange_with(p2_context(),"GET",&format!("/v2/sessions/{session}/media/{id}"),Some(&token.token),vec![],"application/json","");
        assert_eq!(actual,status,"{}",String::from_utf8_lossy(&bytes));
        if status == 200 { assert_eq!(bytes,b"image bytes"); }
    }
    let png = b"\x89PNG\r\n\x1a\nimage fixture";
    let root = crate::management::v2::attachments::attachments_root().unwrap();
    let (_, attachment) = crate::management::v2::attachments::store_upload(&root,"cutex.blob-a",Some("image/png"),png,Default::default()).unwrap();
    let attachment_id = attachment["attachmentId"].as_str().unwrap();
    for bearer in [token.token.as_str(), ROOT] {
        let read_context = if bearer == ROOT { context() } else { p2_context() };
        let (status, _, bytes) = exchange_with(read_context,"GET",&format!("/v2/sessions/cutex.blob-a/attachments/{attachment_id}"),Some(bearer),vec![],"application/json","");
        assert_eq!(status,200,"{}",String::from_utf8_lossy(&bytes));
        assert_eq!(bytes,png);
        let (status, _, bytes) = exchange_with(p2_context(),"GET","/v2/sessions/cutex.blob-a/server-requests",Some(bearer),vec![],"application/json","");
        assert_eq!(status,200,"{}",String::from_utf8_lossy(&bytes));
        let body:Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["requests"],json!([])); // offline; no bootstrap/native callback
    }
    for suffix in [format!("attachments/{attachment_id}"),"server-requests".to_owned()] {
        let (status, _, _) = exchange_with(p2_context(),"GET",&format!("/v2/sessions/cutex.blob-hidden/{suffix}"),Some(&token.token),vec![],"application/json","");
        assert_eq!(status,404);
    }
}

#[test]
fn guides_preferences_dispatch_shares_owner_state_and_enforces_device_scope() {
    use crate::management::client_tokens::DeviceScope;
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let tokens = ClientTokenStore::open_default().unwrap();
    let users = UserStore::open_default().unwrap();
    let path = crate::management::v2::guides_preferences::PATH;
    let body = json!({"schema":1,"skippedThrough":"0.2.0","updatedAt":"client-time","future":true});
    assert_eq!(exchange("GET", path, None, &json!({})).0, 401);
    let member = users.create_user("Guides member", None, Grant::default(), "root").unwrap();
    let member_token = tokens.issue("guides-member").unwrap();
    users.bind_token(&member_token.token_id, Some(&member.id), "root").unwrap();
    let pending = tokens.issue("guides-pending").unwrap();
    for token in [&member_token.token, &pending.token] {
        assert_eq!(exchange("GET", path, Some(token), &json!({})).0, 403);
        assert_eq!(exchange("PUT", path, Some(token), &body).0, 403);
    }
    let default = exchange("GET", path, Some(ROOT), &json!({}));
    assert_eq!(default, (200, json!({"schema":1,"skippedThrough":null,"updatedAt":null})));
    let storage = crate::config::paths::runtime_dir().unwrap().join("users/guides-preferences-v1.json");
    assert!(!storage.exists());
    for scope in [DeviceScope::Read, DeviceScope::Chat] {
        let narrowed = tokens.issue_scoped("guides-narrowed", scope).unwrap();
        users.bind_token(&narrowed.token_id, Some(OWNER), "root").unwrap();
        assert_eq!(exchange("GET", path, Some(&narrowed.token), &json!({})).0, 200);
        assert_eq!(exchange("PUT", path, Some(&narrowed.token), &body).0, 403);
    }
    let (paired, _) = tokens.issue_device("guides-owner", "Synthetic desktop", "other").unwrap();
    users.bind_token(&paired.token_id, Some(OWNER), "root").unwrap();
    REMOTE.with(|remote| remote.set(true));
    let saved = exchange("PUT", path, Some(&paired.token), &body);
    REMOTE.with(|remote| remote.set(false));
    assert_eq!(saved.0, 200, "{saved:?}");
    assert_eq!(saved.1["skippedThrough"], "0.2.0");
    assert_ne!(saved.1["updatedAt"], "client-time");
    assert_eq!(exchange("GET", path, Some(ROOT), &json!({})), saved);
    assert_eq!(exchange("PUT", path, Some(ROOT), &json!({"schema":2,"skippedThrough":null})).0, 400);
    let reset = exchange("PUT", path, Some(ROOT), &json!({"schema":1,"skippedThrough":null}));
    assert_eq!(reset.0, 200);
    assert!(reset.1["skippedThrough"].is_null());
    assert!(reset.1["updatedAt"].is_string());
    assert!(storage.exists());
}

#[test]
fn feature_profiles_http_is_owner_only_and_never_projects_credentials() {
    use crate::management::v2::{feature_profiles, schema_export::assert_valid};

    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let users = UserStore::open_default().unwrap();
    let tokens = ClientTokenStore::open_default().unwrap();
    let member = users.create_user("Feature Profile member", None, Grant::default(), "root").unwrap();
    let member_token = tokens.issue("feature-profile-member").unwrap();
    users.bind_token(&member_token.token_id, Some(&member.id), "root").unwrap();
    let path = feature_profiles::PATH;
    let create = json!({
        "name":"Rewrite provider", "protocol":"openaiResponses", "baseUrl":"http://127.0.0.1:9/v1",
        "model":"rewrite-fixture", "quotaType":"balance", "apiKey":"synthetic-fixture-key"
    });
    assert_valid("feature-profiles", "create", &create);
    assert_eq!(exchange("POST", path, None, &create).0, 401);
    assert_eq!(exchange("POST", path, Some(&member_token.token), &create).0, 403);
    let (status, created) = exchange("POST", path, Some(ROOT), &create);
    assert_eq!(status, 201, "{created}");
    assert_valid("feature-profiles", "detail", &created);
    assert!(!created.to_string().contains("synthetic-fixture-key"));
    let id = created["profile"]["id"].as_str().unwrap();
    let item = format!("{path}/{id}");
    assert_eq!(exchange("GET", &item, Some(&member_token.token), &json!({})).0, 403);
    let narrowed = tokens.issue_scoped("feature-rewrite-read", crate::management::client_tokens::DeviceScope::Read).unwrap();
    users.bind_token(&narrowed.token_id, Some(OWNER), "root").unwrap();
    let rewrite = |route| exchange("POST", "/v2/text-rewrite", Some(&narrowed.token),
        &json!({"templateId":"prompt-enhance-v1","text":"fixture","route":route}));
    for route in [
        json!({"mode":"featureProfiles","featureProfileIds":[id]}),
        json!({"mode":"profile","profileIds":[id]}),
        json!({"mode":"agent","sessionId":"cutex.missing","fallbackFeatureProfileIds":[id]}),
        json!({"mode":"agent","sessionId":"cutex.missing","fallbackProfileIds":[id]}),
    ] {
        let (status, error) = rewrite(route);
        assert_eq!((status,code(&error)), (403,"device_scope_denied"), "{error}");
    }
    let (status, error) = exchange("POST", "/v2/text-rewrite", Some(ROOT), &json!({
        "templateId":"prompt-enhance-v1", "text":"fixture",
        "route":{"mode":"agent","sessionId":"cutex.missing"},
    }));
    assert_eq!((status,code(&error)), (422,"invalid_rewrite_route"), "{error}");

    let (status, listed) = exchange("GET", path, Some(ROOT), &json!({}));
    assert_eq!(status, 200, "{listed}");
    assert_valid("feature-profiles", "list", &listed);
    assert_eq!(listed["profiles"], json!([created["profile"]]));
    let (status, ordinary) = exchange("GET", "/v2/profiles", Some(ROOT), &json!({}));
    assert_eq!(status, 200, "{ordinary}");
    assert!(!ordinary.to_string().contains(id));
    assert!(!ordinary.to_string().contains("Rewrite provider"));
    let capability = exchange("GET", "/v2/capabilities", Some(ROOT), &json!({})).1;
    assert_eq!(capability["features"]["featureProfiles"], true);

    let (status, patched) = exchange("PATCH", &item, Some(ROOT), &json!({"quotaType":"periodicAllowance"}));
    assert_eq!(status, 200, "{patched}");
    assert_eq!(patched["profile"]["quotaType"], "periodicAllowance");
    let recorded = feature_profiles::provider(id).unwrap().unwrap();
    assert_eq!(recorded.key, crate::profiles::stored_keys::SecretKey::new("synthetic-fixture-key").unwrap());
    assert_eq!(exchange("PATCH", &item, Some(ROOT), &json!({"apiKey":null})).0, 400);
    let (status, _) = exchange("PATCH", &item, Some(ROOT), &json!({"apiKey":"synthetic-replacement"}));
    assert_eq!(status, 200);
    let recorded = feature_profiles::provider(id).unwrap().unwrap();
    assert_eq!(recorded.key, crate::profiles::stored_keys::SecretKey::new("synthetic-replacement").unwrap());

    let (status, deleted) = exchange("DELETE", &item, Some(ROOT), &json!({}));
    assert_eq!(status, 200, "{deleted}");
    assert_valid("feature-profiles", "deleted", &deleted);
    assert_eq!(exchange("GET", &item, Some(ROOT), &json!({})).0, 404);
    assert!(feature_profiles::provider(id).unwrap().is_none());
}

#[test]
fn feature_profiles_http_reports_control_port_refusals() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let config = crate::profiles::model::CodezConfig {
        management_port: Some(35470),
        agent_bus_port: Some(35460),
        ..Default::default()
    };
    crate::config::store::save_codez_config(&config).unwrap();

    let path = crate::management::v2::feature_profiles::PATH;
    let mut create = json!({
        "name":"Rewrite provider", "protocol":"openaiResponses",
        "baseUrl":"http://127.0.0.1:35470/v1", "model":"rewrite-fixture",
        "quotaType":"balance", "apiKey":"synthetic-fixture-key"
    });
    let (status, error) = exchange("POST", path, Some(ROOT), &create);
    assert_eq!((status, code(&error)), (400, "feature_profile_control_port"));

    create["baseUrl"] = json!("invalid URL");
    let (status, error) = exchange("POST", path, Some(ROOT), &create);
    assert_eq!((status, code(&error)), (400, "invalid_feature_profile"));

    create["baseUrl"] = json!("https://example.invalid/v1");
    let (status, created) = exchange("POST", path, Some(ROOT), &create);
    assert_eq!(status, 201, "{created}");
    let id = created["profile"]["id"].as_str().unwrap();
    let item = format!("{path}/{id}");
    let patch = json!({"baseUrl":"http://127.0.0.1:35460/v1"});
    let (status, error) = exchange("PATCH", &item, Some(ROOT), &patch);
    assert_eq!((status, code(&error)), (400, "feature_profile_control_port"));

    let (status, current) = exchange("GET", &item, Some(ROOT), &json!({}));
    assert_eq!(status, 200, "{current}");
    assert_eq!(current["profile"]["baseUrl"], create["baseUrl"]);
}

#[test]
fn feature_profiles_list_marks_damaged_files_and_keeps_create_available() {
    use crate::management::v2::{feature_profiles, schema_export::assert_valid};
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let good = feature_profiles::create_rewrite_fixture();
    let ids = ["fp-00000000-0000-4000-8000-000000000001", "fp-00000000-0000-4000-8000-000000000002"];
    let root = crate::config::paths::config_dir().unwrap().join(feature_profiles::DIR);
    std::fs::write(root.join(format!("{}.json",ids[0])), "invalid synthetic private payload").unwrap();
    std::fs::write(root.join(format!("{}.json",ids[1])), vec![b'X'; 16385]).unwrap();
    let list = || {
        let (status, value) = exchange("GET", feature_profiles::PATH, Some(ROOT), &json!({}));
        assert_eq!(status, 200, "{value}");
        assert_valid("feature-profiles", "list", &value);
        assert_eq!(value["errors"], json!([
            {"id":ids[0],"code":"invalid_feature_profile_record"},
            {"id":ids[1],"code":"invalid_feature_profile_record"},
        ]));
        assert!(!value.to_string().contains("synthetic private payload"));
        value["profiles"].as_array().unwrap().iter()
            .map(|item| item["id"].as_str().unwrap().to_owned()).collect::<Vec<_>>()
    };
    assert_eq!(list(), [good.clone()]);
    let added = feature_profiles::create_rewrite_fixture();
    let mut found = list();
    found.sort_unstable();
    let mut expected = vec![good, added];
    expected.sort_unstable();
    assert_eq!(found, expected);
}

#[test]
fn avatar_style_dispatch_enforces_owner_scope_and_publishes_only_saved_changes() {
    use crate::management::client_tokens::DeviceScope;
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let tokens = ClientTokenStore::open_default().unwrap();
    let users = UserStore::open_default().unwrap();
    let member = users.create_user("Style member", None, Grant::default(), "root").unwrap();
    let member_token = tokens.issue("style-member").unwrap();
    users.bind_token(&member_token.token_id, Some(&member.id), "root").unwrap();
    let (status, created) = exchange("POST", "/v2/profiles", Some(ROOT),
        &json!({"name":"Style owner","kind":"apiKey","preset":"glm","apiKey":"synthetic-placeholder"}));
    assert_eq!(status, 201, "{created}");
    let id = created["profile"]["id"].as_str().unwrap();
    let path = format!("/v2/profiles/{id}");
    let style = json!({"avatarStyle":{"kind":"portrait","portraitThemes":["future-pack/theme"]}});
    assert_eq!(exchange("PATCH", &path, None, &style).0, 401);
    assert_eq!(exchange("PATCH", &path, Some(&member_token.token), &style).0, 403);
    for scope in [DeviceScope::Read, DeviceScope::Chat] {
        let narrowed = tokens.issue_scoped("style-narrowed", scope).unwrap();
        users.bind_token(&narrowed.token_id, Some(OWNER), "root").unwrap();
        assert_eq!(exchange("PATCH", &path, Some(&narrowed.token), &style).0, 403);
    }
    let (paired, _) = tokens.issue_device("style-owner", "Synthetic desktop", "other").unwrap();
    users.bind_token(&paired.token_id, Some(OWNER), "root").unwrap();
    let repository = crate::management::v2::repository::management_v2_repository().unwrap();
    let checkpoint = repository.checkpoint().unwrap();
    REMOTE.with(|remote| remote.set(true));
    let saved = exchange("PATCH", &path, Some(&paired.token), &style);
    REMOTE.with(|remote| remote.set(false));
    assert_eq!(saved.0, 200, "{saved:?}");
    assert_eq!(saved.1["profile"]["appearance"]["avatarStyle"], style["avatarStyle"]);
    assert_eq!(exchange("PATCH", &path, Some(ROOT), &style).1["changed"], false);
    assert_eq!(exchange("PATCH", &path, Some(ROOT), &json!({"avatarStyle":{"kind":"unknown"}})).0, 400);
    assert_eq!(exchange("PATCH", &path, Some(ROOT), &json!({"avatarThemes":["cats"]})).0, 200);
    let events = host_events(&checkpoint, crate::management::v2::profiles_api::PROFILES_CHANGED_EVENT);
    assert_eq!(events.len(), 1, "only the committed style change publishes");
    assert_eq!(events[0]["profileId"], id);
    assert_eq!(events[0]["fields"], json!(["avatarStyle"]));
    assert!(!events[0].to_string().contains("future-pack"));
    assert_eq!(exchange("PATCH", &path, Some(ROOT), &json!({"avatarStyle":null})).0, 200);
    assert_eq!(host_events(&checkpoint, crate::management::v2::profiles_api::PROFILES_CHANGED_EVENT).len(), 2);
}

#[test]
fn profile_link_secret_dispatch_enforces_owner_full_and_never_returns_secret() {
    use base64::Engine;
    use sha2::Digest;
    use crate::management::client_tokens::DeviceScope;
    let _home_lock=crate::config::paths::test_home_env_lock();
    let _instance=PrivateInstance::new();
    let tokens=ClientTokenStore::open_default().unwrap();
    let users=UserStore::open_default().unwrap();
    let full=tokens.issue("profile-link-owner").unwrap();
    users.snapshot().unwrap();
    let member=users.create_user("Link member",None,Grant::default(),"root").unwrap();
    let member_token=tokens.issue("link-member").unwrap();
    users.bind_token(&member_token.token_id,Some(&member.id),"root").unwrap();
    let secret=base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(b"synthetic-secret-dispatch-first"));
    let replacement=base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(b"synthetic-secret-dispatch-replacement"));
    let body=json!({"secret":secret});
    let path=crate::management::v2::profile_link::SECRET_PATH;
    assert_eq!(exchange("GET",path,None,&json!({})).0,401);
    assert_eq!(exchange("GET",path,Some(&member_token.token),&json!({})).0,403);
    for scope in [DeviceScope::Read,DeviceScope::Chat] {
        let narrowed=tokens.issue_scoped("link-narrowed",scope).unwrap();
        users.bind_token(&narrowed.token_id,Some(OWNER),"root").unwrap();
        for (method,payload) in [("GET",json!({})),("PUT",body.clone()),("DELETE",json!({}))] {
            let denied=exchange(method,path,Some(&narrowed.token),&payload);
            assert_eq!(denied.0,403,"{denied:?}");assert_eq!(any_code(&denied.1),"device_scope_denied");
        }
    }
    assert_eq!(exchange("GET",path,Some(ROOT),&json!({})).1["secretId"],Value::Null);
    let (paired,_)=tokens.issue_device("link-owner-device","Synthetic desktop","other").unwrap();
    users.bind_token(&paired.token_id,Some(OWNER),"root").unwrap();
    REMOTE.with(|remote|remote.set(true));
    let (status,stored)=exchange("PUT",path,Some(&paired.token),&body);
    REMOTE.with(|remote|remote.set(false));
    assert_eq!(status,200,"{stored}");assert!(stored.get("secret").is_none());assert!(!stored.to_string().contains(&secret));
    assert_eq!(exchange("GET",path,Some(&full.token),&json!({})).1,stored);
    assert_eq!(exchange("PUT",path,Some(&paired.token),&body).0,403);
    assert_eq!(exchange("DELETE",path,Some(&paired.token),&json!({})).0,403);
    REMOTE.with(|remote|remote.set(true));
    let remote=exchange("PUT",path,Some(&paired.token),&body);
    let remote_get=exchange("GET",path,Some(&paired.token),&json!({}));
    let remote_delete=exchange("DELETE",path,Some(&paired.token),&json!({}));
    REMOTE.with(|remote|remote.set(false));
    assert_eq!(remote.0,403,"{remote:?}");assert_eq!(any_code(&remote.1),"owner_local_only");
    assert_eq!(remote_get,(200,stored.clone()));assert_eq!(remote_delete.0,403);
    let rotated=exchange("PUT",path,Some(ROOT),&json!({"secret":replacement}));
    assert_eq!(rotated.0,200);assert_ne!(rotated.1["secretId"],stored["secretId"]);
    assert_eq!(exchange("DELETE",path,Some(ROOT),&json!({})).1["secretId"],Value::Null);
    REMOTE.with(|remote|remote.set(true));
    let initialized=exchange("PUT",path,Some(&paired.token),&body);
    REMOTE.with(|remote|remote.set(false));
    assert_eq!(initialized.0,403);
    let off=exchange("GET",path,Some(&paired.token),&json!({}));
    assert_eq!(off.1["disabled"],true);assert_eq!(off.1["secretId"],Value::Null);
    let enabled=exchange("PUT",path,Some(ROOT),&body);
    assert_eq!(enabled,(200,stored.clone()));
    assert_eq!(exchange("DELETE",path,Some(ROOT),&json!({})).1["secretId"],Value::Null);
    let audit=std::fs::read_to_string(crate::config::paths::runtime_dir().unwrap().join("profile-link/audit.jsonl")).unwrap();
    let events:Vec<Value>=audit.lines().map(|line|serde_json::from_str(line).unwrap()).collect();
    assert_eq!(events.len(),5);
    assert!(events.iter().all(|e|e["action"]=="profileLinkSecretSet"));
    assert_eq!(events[0]["details"]["secretIdBefore"],Value::Null);
    assert_eq!(events[0]["details"]["secretIdAfter"],stored["secretId"]);
    assert_eq!(events[0]["details"]["actor"],paired.token_id);
    assert_eq!(events[3]["details"]["actor"],"root");
    assert!(!audit.contains(&secret)&&!audit.contains(&replacement));
    let malformed=exchange("PUT",path,Some(ROOT),&json!({"secret":secret,"actionId":secret,"requestId":secret}));
    assert_eq!(malformed.0,400);
    let log=std::fs::read_to_string(crate::config::paths::runtime_dir().unwrap().join("service-logs/management-errors.jsonl")).unwrap_or_default();
    assert!(!log.contains(&secret));
}

thread_local! {
    /// The next exchanges arrive on a remote (non-loopback) listener.
    static REMOTE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

struct PrivateInstance {
    root: PathBuf,
    previous: Selection,
}

#[test]
fn scoped_collection_read_failure_never_falls_through_to_owner_handler() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static READS: AtomicUsize = AtomicUsize::new(0);
    let _lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let tokens = ClientTokenStore::open_default().unwrap();
    let users = UserStore::open_default().unwrap();
    let user = users.create_user("No project grants", None, Grant::default(), "root").unwrap();
    let token = tokens.issue("filter-failure-audit").unwrap();
    users.bind_token(&token.token_id, Some(&user.id), "root").unwrap();
    let mut context = p2_context();
    READS.store(0, Ordering::SeqCst);
    context.list_management_projects = |_| {
        if READS.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(crate::agent_management::AgentManagementError::PersistenceUnavailable);
        }
        // A retry of the owner handler sees the recovered, host-wide collection.
        Ok(crate::management::control_plane::HumanManagementProjectCollection {
            schema: crate::management::control_plane::HumanManagementProjectSchema::V1,
            projects: vec![], archived_projects: vec![],
            available_agents: vec![crate::agent_management::ProjectAgentChoice {
                cutex_session_id: crate::role_revision::CutexSessionId::new("cutex.hidden-agent").unwrap(),
                name: "Hidden agent in another project".into(),
                current_project_id: Some(crate::agent_management::ProjectId::new("hidden-project").unwrap()),
            }],
        })
    };
    let (status, _, bytes) = exchange_with(context, "GET", "/v2/agent-management/projects",
        Some(&token.token), vec![], "application/json", "");
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(status, 503, "transient scope-filter failure exposed owner data: {value}");
    assert_eq!(READS.load(Ordering::SeqCst), 1, "the unfiltered owner handler must not run");
    assert!(!String::from_utf8_lossy(&bytes).contains("hidden-agent"));
}

impl PrivateInstance {
    fn new() -> Self {
        let root = crate::config::paths::home_dir()
            .unwrap()
            .join(format!("ah-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(crate::config::paths::PRIVATE_HOME_MARKER), b"").unwrap();
        let previous = instance::selected().unwrap();
        instance::set_test_selection(Selection::Custom(root.join(".cutex")));
        // Every fixture starts with an empty registry. Finish its empty
        // journal recovery before exercising routes whose callbacks must not
        // be invoked; otherwise batch order can trigger an unrelated load.
        crate::management::v2::repository::management_v2_repository().unwrap()
            .materialize_recovery_reset(&[],true).unwrap();
        Self { root, previous }
    }
}

impl Drop for PrivateInstance {
    fn drop(&mut self) {
        listeners::set_current_connection(None);
        listeners::set_current_client_token(None);
        crate::management::users::set_current_principal(None);
        instance::set_test_selection(self.previous.clone());
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn context() -> ManagementRequestContext {
    // Denied and gate-answered requests never reach native runtime callbacks.
    ManagementRequestContext {
        explicit_launch_action: |_, _| panic!("unexpected runtime action"),
        promote_quick_chat: |_, _, _| panic!("unexpected quick chat promotion"),
        adopt_saved_native: |_, _| panic!("unexpected native adoption"),
        review_agent_archive: |_, _| panic!("unexpected archive review"),
        owner_archive_action: |_, _| panic!("unexpected permanent archive"),
        execute_agent_archive: |_, _| panic!("unexpected archive"),
        durable_agent_candidates: |_| panic!("unexpected agent candidates"),
        import_durable_agent: |_, _| panic!("unexpected agent import"),
        load_registry: || panic!("unexpected registry load"),
        load_runtime_status: |_| panic!("unexpected runtime status"),
        forward_native_request: |_, _| panic!("unexpected native request"),
        respond_native_server_request: |_, _, _| panic!("unexpected native response"),
        handle_user_input: |_| panic!("unexpected user input"),
        flush_user_input_queue: |_, _, _| panic!("unexpected queue flush"),
        steer_user_input_queue: |_, _, _| panic!("unexpected queue steer"),
        interrupt_active_turn: |_, _, _| panic!("unexpected interrupt"),
        load_bootstrap_state: |_, _, _| panic!("unexpected bootstrap"),
        load_latest_plan: |_| panic!("unexpected plan"),
        // r290 P6
        load_board_plan: |_| serde_json::Value::Null,
        load_latest_token_usage: |_| panic!("unexpected usage"),
        revert_session_history: |_, _, _| panic!("unexpected revert"),
        delete_session: |_, _| panic!("unexpected session delete"),
        fork_session: |_, _, _| panic!("unexpected fork"),
        side_chat: |_, _, _| panic!("unexpected side chat"),
        open_offline_history: |_| panic!("unexpected history"),
        mutate_session: |_, _, _| panic!("unexpected session mutation"),
        online_session: |_, _, _| panic!("unexpected online request"),
        bind_project_authority: |_| panic!("unexpected project bind"),
        import_legacy_director_ownership: |_| panic!("unexpected ownership import"),
        reconcile_agent_reservation: |_| panic!("unexpected reservation"),
        abandon_agent_create: |_| panic!("unexpected reservation"),
        list_management_projects: |_| panic!("unexpected project list"),
        read_management_project: |_, _| panic!("unexpected project read"),
        update_management_project_presentation: |_, _| panic!("unexpected project update"),
        execute_management_operator_action: |_, _| panic!("unexpected operator action"),
        execute_management_project_mutation: |_, _| panic!("unexpected project mutation"),
        create_human_agent: |_, _| panic!("unexpected agent create"),
        create_human_session: |_, _| panic!("unexpected session create"),
    }
}

#[test]
fn quick_chat_promote_http_is_owner_only_strict_and_replayable() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let tokens = ClientTokenStore::open_default().unwrap();
    let users = UserStore::open_default().unwrap();
    let guest = users.create_user("Chat reader", None, Grant::default(), "root").unwrap();
    let guest_token = tokens.issue("reader-device").unwrap();
    users.bind_token(&guest_token.token_id, Some(&guest.id), "root").unwrap();
    let owner = tokens.issue("owner-device").unwrap();
    users.bind_token(&owner.token_id, Some(OWNER), "root").unwrap();
    let mut record = crate::session::model::CutexSessionRecord::new(
        "cutex.promote-http".into(), Some(uuid::Uuid::new_v4().to_string()),
        crate::platform::host::current_host_name(), crate::config::paths::config_dir().unwrap().to_string_lossy().into_owned(), None,
    ).unwrap();
    record.registration_class = crate::agent_bus::model::AgentRegistrationClass::LocalOnly;
    record.agent_enabled = false;
    let mut store = crate::session::model::CutexSessionStore::default();
    store.sessions.insert(record.cutex_session_id.clone(), record.clone());
    crate::session::store::save_cutex_session_store(&store).unwrap();
    let path = "/v2/human/sessions/cutex.promote-http/promote";
    let body = json!({"requestId":"upgrade","expectedRevision":record.revision,"name":"HTTP Agent"});
    assert_eq!(exchange("POST", path, None, &body).0, 401);
    assert_eq!(exchange("POST", path, Some(&guest_token.token), &body).0, 403);
    let mut ctx = context();
    ctx.promote_quick_chat = |principal, session, payload| {
        crate::agent_management::AgentManagementProvider::open_default()?.promote_quick_chat(
            principal, &crate::session::store::cutex_sessions_path()?, session, payload)
    };
    let call = |body: &Value| {
        let (status, _, bytes) = exchange_with(ctx, "POST", path, Some(&owner.token), body.to_string().into_bytes(), "application/json", "");
        (status, serde_json::from_slice::<Value>(&bytes).unwrap())
    };
    crate::management::v2::schema_export::assert_valid("quick-chat-promote", "request", &body);
    let mut invalid = body.clone(); invalid["start"] = json!(true);
    let (status, error) = call(&invalid);
    assert_eq!((status, code(&error)), (400, "invalid_request"));
    let (status, result) = call(&body);
    assert_eq!(status, 200, "{result}");
    assert_eq!(result["complete"], true, "{result}");
    let tools = crate::agent_bus::tool_policy::tools_list_for_session("cutex.promote-http");
    assert!(tools["tools"].as_array().unwrap().iter().any(|t| t["name"] == "agent_list"), "{tools}");
    crate::management::v2::schema_export::assert_valid("quick-chat-promote", "response", &result);
    assert_eq!(call(&body), (200, result));
    let mut changed = body.clone(); changed["name"] = json!("Changed");
    let (status, error) = call(&changed);
    assert_eq!((status, code(&error)), (409, "request_id_payload_conflict"));
    let caps = crate::management::v2::capabilities::capabilities_resource();
    assert!(caps.to_string().contains("quick-chat-promote-v1"));
}

fn exchange(method: &str, path: &str, bearer: Option<&str>, body: &Value) -> (u16, Value) {
    let (status, _, body) = exchange_raw(method, path, bearer, body, "");
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// `exchange` with extra request headers; returns the response head and raw body.
fn exchange_raw(method: &str, path: &str, bearer: Option<&str>, body: &Value, extra: &str) -> (u16, String, Vec<u8>) {
    exchange_with(context(), method, path, bearer, body.to_string().into_bytes(), "application/json", extra)
}

/// The full form: own context, raw body and content type.
fn exchange_with(
    context: ManagementRequestContext,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: Vec<u8>,
    content_type: &str,
    extra: &str,
) -> (u16, String, Vec<u8>) {
    exchange_with_tokens(context, method, path, bearer, body, content_type, extra, None)
}

fn exchange_with_tokens(
    context: ManagementRequestContext,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: Vec<u8>,
    content_type: &str,
    extra: &str,
    bridge_token: Option<&str>,
) -> (u16, String, Vec<u8>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n",
        body.len()
    ).into_bytes();
    if let Some(bearer) = bearer {
        request.extend_from_slice(format!("Authorization: Bearer {bearer}\r\n").as_bytes());
    }
    request.extend_from_slice(extra.as_bytes());
    request.extend_from_slice(b"\r\n");
    request.extend_from_slice(&body);
    let label = format!("{method} {path}");
    let client = std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let mut stream = TcpStream::connect(address).unwrap();
        // The main-thread handler owns the server socket and closes it below.
        // Drain until that EOF: native Git scans have their own 30-second budget,
        // so a shorter client timer can abort a valid response under host load.
        stream.write_all(&request).unwrap();
        let mut response = vec![];
        stream.read_to_end(&mut response).unwrap_or_else(|error| panic!("{label} client read after {:?}: {error}", started.elapsed()));
        if started.elapsed() >= Duration::from_secs(10) {
            eprintln!("HTTP test completed {label} after {:?}", started.elapsed());
        }
        response
    });
    let (mut server, _) = listener.accept().unwrap();
    listeners::set_current_connection(Some(ConnectionInfo {
        remote: REMOTE.with(std::cell::Cell::get),
        peer: "127.0.0.1:43210".parse().unwrap(),
    }));
    listeners::set_current_client_token(None);
    crate::management::users::set_current_principal(None);
    crate::http::server::set_keep_alive(crate::http::server::KeepAlive::Off);
    crate::http::server::set_extra_response_headers(String::new());
    crate::http::server::set_accept_gzip(false);
    let handled = handle_management_request(&mut server, bridge_token, Some(ROOT), context);
    listeners::set_current_connection(None);
    listeners::set_current_client_token(None);
    crate::management::users::set_current_principal(None);
    drop(server);
    handled.unwrap_or_else(|error| panic!("{method} {path} handler: {error:#}"));
    let response = client.join().unwrap();
    let split = response.windows(4).position(|v| v == b"\r\n\r\n").unwrap();
    let headers = std::str::from_utf8(&response[..split]).unwrap();
    let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, headers.to_string(), response[split + 4..].to_vec())
}

fn code(value: &Value) -> &str {
    value.pointer("/error/code").and_then(Value::as_str).unwrap_or("")
}

#[test]
fn service_notifications_loopback_listener_accepts_only_the_non_browser_service_bearer() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let slug = "service-notification-http-test";
    let (status, service) = exchange("POST", "/v2/services", Some(ROOT), &json!({
        "slug": slug,
        "runtime": "integration",
        "displayName": "Service notification test",
        "lifetime": "persistent",
    }));
    assert_eq!(status, 201, "{service}");
    let settings = serde_json::to_value(crate::services::notifications::Settings {
        enabled: true,
        ..Default::default()
    }).unwrap();
    let (status, configured) = exchange(
        "PUT", &format!("/v2/services/{slug}/notification-settings"), Some(ROOT), &settings,
    );
    assert_eq!(status, 200, "{configured}");
    let (status, issued) = exchange(
        "POST", &format!("/v2/services/{slug}/notification-credential"), Some(ROOT),
        &json!({"mode": "manual"}),
    );
    assert_eq!(status, 201);
    let token = issued["token"].as_str().unwrap();
    assert!(token.starts_with("ctsvc1_"));

    let route = format!("/v2/services/{slug}/notifications");
    let body = json!({"title": "Synthetic service notice", "priority": "normal", "dedupKey": "loopback"});
    let (status, value) = exchange("POST", &route, Some(token), &body);
    assert_eq!(status, 201, "{value}");
    assert_eq!(value["disposition"], "accepted");
    let (status, _, response) = exchange_raw(
        "POST", &route, Some(token), &body, "Origin: https://service-notification.invalid\r\n",
    );
    assert_eq!(status, 401);
    assert_eq!(code(&serde_json::from_slice(&response).unwrap()), "service_credential_required");
}

#[test]
fn owner_user_and_pending_tokens_get_exactly_their_rights() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let tokens = ClientTokenStore::open_default().unwrap();
    // Existing token before the first users read: migrated to owner.
    let owner = tokens.issue("owner-desktop").unwrap();
    let users = UserStore::open_default().unwrap();
    assert_eq!(users.principal_for_token(&owner.token_id), crate::management::users::Principal::Owner);
    let pending = tokens.issue("ssh-device").unwrap();
    let user2 = users
        .create_user("User 2", None, Grant { projects: Projects::Only(vec!["p1".into()]), standalone_agents: vec![], send: false, ..Grant::default() }, "root")
        .unwrap();
    let device = tokens.issue("device-c").unwrap();
    users.bind_token(&device.token_id, Some(&user2.id), "root").unwrap();
    std::thread::sleep(Duration::from_millis(30));

    // Capabilities: every principal, with its own user.
    let (status, caps) = exchange("GET", "/v2/capabilities", Some(&owner.token), &json!({}));
    assert_eq!(status, 200);
    assert_eq!(caps["user"]["isOwner"], true);
    assert!(caps["featureList"].as_array().unwrap().iter().any(|f| f == "users"));
    let (_, caps) = exchange("GET", "/v2/capabilities", Some(ROOT), &json!({}));
    assert_eq!(caps["user"]["id"], OWNER);
    let (status, caps) = exchange("GET", "/v2/capabilities", Some(&pending.token), &json!({}));
    assert_eq!((status, &caps["user"]["pending"]), (200, &json!(true)));
    let (status, caps) = exchange("GET", "/v2/capabilities", Some(&device.token), &json!({}));
    assert_eq!(status, 200);
    assert_eq!(caps["user"]["id"], user2.id.as_str());
    assert_eq!(caps["user"]["grant"]["projects"], json!(["p1"]));
    let (status, host) = exchange("GET", "/v2/host", Some(&device.token), &json!({}));
    assert_eq!(status, 200);
    assert!(host.get("executable").is_none());

    // Pending: nothing else.
    for (method, path) in [("GET", "/v2/sessions"), ("GET", "/v2/settings"), ("GET", "/v2/summary"), ("GET", "/v2/storage"), ("POST", "/v2/storage/refresh")] {
        let (status, value) = exchange(method, path, Some(&pending.token), &json!({}));
        assert_eq!((status, code(&value)), (403, "access_pending"), "{method} {path}");
    }

    // Non-owner: owner routes refused, invisible objects 404, users API closed.
    for (method, path, body) in [
        ("GET", "/v2/settings", json!({})),
        ("GET", "/v2/storage", json!({})),
        ("POST", "/v2/storage/refresh", json!({})),
        ("GET", "/v2/users", json!({})),
        ("POST", "/v2/users", json!({"name": "escalate"})),
        ("PUT", &format!("/v2/users/token-bindings/{}", device.token_id) as &str, json!({"userId": "owner"})),
        ("PATCH", &format!("/v2/users/{}", user2.id) as &str, json!({"grant": {"projects": "all", "send": true}})),
        ("POST", "/v2/pairing-codes", json!({"userId": "owner"})),
        ("GET", "/v2/client-tokens", json!({})),
        ("PATCH", &format!("/v2/client-tokens/{}", device.token_id) as &str, json!({"name": "mine"})),
        ("GET", "/v2/peers", json!({})),
        ("GET", "/v2/fs/file?path=/etc/passwd", json!({})),
        ("POST", "/v2/terminals", json!({})),
        ("GET", "/v2/sessions?includeHidden=true", json!({})),
        ("GET", "/v2/sessions?lifecycle=retired", json!({})),
        ("GET", "/v2/brand-new-route", json!({})),
    ] {
        let (status, value) = exchange(method, path, Some(&device.token), &body);
        assert_eq!((status, code(&value)), (403, "owner_required"), "{method} {path}");
    }
    for (method, path, body) in [
        ("GET", "/v2/sessions/cutex.hidden", json!({})),
        ("GET", "/v2/sessions/cutex.hidden/bootstrap", json!({})),
        ("GET", "/v2/sessions/cutex.hidden/history", json!({})),
        ("GET", "/v2/sessions/cutex.hidden/bus-messages", json!({})),
        ("POST", "/v2/sessions/cutex.hidden/cutex/requests", json!({"requestId": "r", "method": "cutex/session/get", "params": {}})),
        ("POST", "/v2/sessions/cutex.hidden/app-server/requests", json!({"requestId": "r", "native": {"message": {"id": 1, "method": "thread/read"}}})),
        ("GET", "/v2/events?cutexSessionId=cutex.hidden", json!({})),
    ] {
        let (status, value) = exchange(method, path, Some(&device.token), &body);
        assert_eq!((status, code(&value)), (404, "session_not_found"), "{method} {path}");
    }
    let (status, value) = exchange("GET", "/v2/agent-management/projects/p2", Some(&device.token), &json!({}));
    assert_eq!((status, code(&value)), (404, "project_not_found"));

    // Storage is owner-only; a read uses no runtime callbacks and does not scan.
    for token in [&owner.token, ROOT] {
        let (status,value)=exchange("GET", "/v2/storage", Some(token), &json!({}));
        assert_eq!(status,200,"{value}");
        crate::management::v2::schema_export::assert_valid("storage","overview",&value);
        assert_eq!(value["walk"]["state"],"idle");
    }
    // The owner keeps everything, including the users API.
    let (status, listing) = exchange("GET", "/v2/users", Some(&owner.token), &json!({}));
    assert_eq!(status, 200);
    let tokens_view = listing["tokens"].as_array().unwrap();
    let find = |id: &str| tokens_view.iter().find(|t| t["tokenId"] == id).unwrap().clone();
    assert_eq!(find(&owner.token_id)["userId"], OWNER);
    assert_eq!(find(&pending.token_id)["pending"], true);
    assert_eq!(find(&device.token_id)["userId"], user2.id.as_str());
    assert!(!listing.to_string().contains("cxm1."), "no secrets in the listing");
    let (status, _) = exchange("GET", "/v2/users", Some(ROOT), &json!({}));
    assert_eq!(status, 200);

    // Owner assigns the pending device; it is then the user's.
    let (status, bound) = exchange(
        "PUT",
        &format!("/v2/users/token-bindings/{}", pending.token_id),
        Some(&owner.token),
        &json!({"userId": user2.id}),
    );
    assert_eq!((status, &bound["pending"]), (200, &json!(false)));
    std::thread::sleep(Duration::from_millis(30));
    let (status, value) = exchange("GET", "/v2/settings", Some(&pending.token), &json!({}));
    assert_eq!((status, code(&value)), (403, "owner_required"));
}

#[test]
fn rebinding_or_regranting_ends_open_streams_but_revocation_is_distinct() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let tokens = ClientTokenStore::open_default().unwrap();
    let users = UserStore::open_default().unwrap();
    let issued = tokens.issue("desktop").unwrap();
    users.bind_token(&issued.token_id, Some(OWNER), "root").unwrap();
    let mut request = crate::http::server::SimpleHttpRequest {
        method: "GET".into(),
        path: "/v2/events/stream".into(),
        headers: [("authorization".to_string(), format!("Bearer {}", issued.token))].into_iter().collect(),
        body: Vec::new(),
    };
    let verified = crate::management::client_tokens::authenticate_management_request_with(&mut request, Some(ROOT), &tokens).unwrap();
    assert!(verified.still_valid_in(&tokens));
    let user = users.create_user("C", None, Grant::default(), "root").unwrap();
    users.bind_token(&issued.token_id, Some(&user.id), "root").unwrap();
    std::thread::sleep(Duration::from_millis(30));
    assert!(!verified.still_valid_in(&tokens), "a rebound owner token must end its streams");

    // The access-changed frame (token still issued) differs from revoked.
    let mut out = Vec::new();
    listeners::write_revoked_frame(&mut out, &issued.token_id).unwrap();
    assert!(String::from_utf8_lossy(&out).starts_with("event: cutex/access/changed"));
    tokens.revoke(&issued.token_id).unwrap();
    let mut out = Vec::new();
    listeners::write_revoked_frame(&mut out, &issued.token_id).unwrap();
    assert!(String::from_utf8_lossy(&out).starts_with("event: cutex/device/revoked"));
}

#[test]
fn scoped_sessions_check_methods_send_and_input_types() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let mut store = crate::session::model::CutexSessionStore::default();
    for id in ["cutex.solo", "cutex.other"] {
        let mut record = crate::session::model::CutexSessionRecord::new(
            id.to_string(),
            None,
            "test-host".to_string(),
            "/tmp".to_string(),
            None,
        )
        .unwrap();
        record.exposed_to_backend = true;
        store.sessions.insert(id.to_string(), record);
    }
    let mut hidden = crate::session::model::CutexSessionRecord::new(
        "cutex.hidden".to_string(), None, "test-host".to_string(), "/tmp".to_string(), None,
    )
    .unwrap();
    hidden.exposed_to_backend = false;
    hidden.registration_class = crate::agent_bus::model::AgentRegistrationClass::Persistent;
    store.sessions.insert("cutex.hidden".to_string(), hidden);
    let mut local = crate::session::model::CutexSessionRecord::new(
        "cutex.local-hidden".to_string(), None, "test-host".to_string(), "/tmp".to_string(), None,
    )
    .unwrap();
    local.exposed_to_backend = false;
    store.sessions.insert("cutex.local-hidden".to_string(), local);
    crate::session::store::save_cutex_session_store(&store).unwrap();
    let tokens = ClientTokenStore::open_default().unwrap();
    let users = UserStore::open_default().unwrap();
    let reader = users
        .create_user("Reader", None, Grant { standalone_agents: vec!["cutex.solo".into()], ..Grant::default() }, "root")
        .unwrap();
    let sender = users
        .create_user("Sender", None, Grant { standalone_agents: vec!["cutex.solo".into()], send: true, ..Grant::default() }, "root")
        .unwrap();
    let read_token = tokens.issue("reader").unwrap();
    let send_token = tokens.issue("sender").unwrap();
    users.bind_token(&read_token.token_id, Some(&reader.id), "root").unwrap();
    users.bind_token(&send_token.token_id, Some(&sender.id), "root").unwrap();
    std::thread::sleep(Duration::from_millis(30));

    let scope = Scope::load(&Grant { standalone_agents: vec!["cutex.solo".into()], ..Grant::default() }).unwrap();
    assert!(scope.sees_session("cutex.solo") && !scope.sees_session("cutex.other"));
    // The retired IM exposure flag no longer hides a durable agent: the grant decides.
    let unexposed_grant = Grant { standalone_agents: vec!["cutex.hidden".into()], ..Grant::default() };
    let unexposed = Scope::load(&unexposed_grant).unwrap();
    assert!(unexposed.sees_session("cutex.hidden"), "a granted unexposed durable agent is visible");
    assert!(!unexposed.sees_session("cutex.solo"), "grants still scope the user");
    let local_grant = Grant { standalone_agents: vec!["cutex.local-hidden".into()], ..Grant::default() };
    assert!(
        !Scope::load(&local_grant).unwrap().sees_session("cutex.local-hidden"),
        "an unexposed local-only session stays the owner's"
    );
    let all = Scope::load(&Grant { projects: Projects::All, ..Grant::default() }).unwrap();
    assert!(!all.sees_session("cutex.solo"), "\"all\" projects never includes standalone agents");

    let cutex = |method: &str, params: Value| json!({"requestId": "r-1", "method": method, "params": params});
    let text = json!({"clientUserMessageId": "m", "origin": {"kind": "desktop", "clientId": "c"}, "strategy": "queue", "input": [{"type": "text", "text": "hi"}]});
    let image = json!({"clientUserMessageId": "m", "origin": {"kind": "desktop", "clientId": "c"}, "strategy": "queue", "input": [{"type": "localImage", "path": "/etc/shadow"}]});
    let path = "/v2/sessions/cutex.solo/cutex/requests";
    for method in ["cutex/userInput/submit", "cutex/userInput/queue/steer"] {
        for join in [json!({"askId":"ask-live","attempt":1,"expectedTurnId":"turn-live","runtimeGeneration":7}), Value::Null, json!({})] {
            let mut params = if method.ends_with("submit") {
                let mut params = text.clone();
                params["strategy"] = json!("auto");
                params
            } else { json!({"queueId":"q","expectedRevision":1}) };
            params["joinDelegation"] = join;
            for token in [&read_token.token, &send_token.token] {
                let (status, value) = exchange("POST", path, Some(token), &cutex(method, params.clone()));
                assert_eq!((status, code(&value)), (403, "owner_required"), "{method}: {params}");
            }
        }
    }
    // The root owner's malformed join reaches ordinary schema validation.
    let mut owner_params = text.clone();
    owner_params["strategy"] = json!("auto");
    owner_params["joinDelegation"] = Value::Null;
    let (status, value) = exchange("POST", path, Some(ROOT), &cutex("cutex/userInput/submit", owner_params));
    assert_ne!(status, 403, "root owner must bypass the non-owner gate: {value}");
    let cases: Vec<(&str, Value, u16, &str)> = vec![
        (read_token.token.as_str(), cutex("cutex/session/retire", json!({})), 403, "owner_required"),
        (send_token.token.as_str(), cutex("cutex/session/delete", json!({})), 403, "owner_required"),
        (send_token.token.as_str(), cutex("cutex/ask/create", json!({})), 403, "owner_required"),
        (send_token.token.as_str(), cutex("cutex/runtime/online", json!({})), 403, "owner_required"),
        (read_token.token.as_str(), cutex("cutex/userInput/submit", text.clone()), 403, "send_not_granted"),
        (send_token.token.as_str(), cutex("cutex/userInput/submit", image), 403, "attachment_not_owned"),
    ];
    for (token, body, status, expected) in cases {
        let (got, value) = exchange("POST", path, Some(token), &body);
        assert_eq!((got, code(&value)), (status, expected), "{body}");
    }
    let native = |method: &str| json!({"requestId": "r", "native": {"message": {"id": 1, "method": method, "params": {"threadId": "t"}}}});
    let native_path = "/v2/sessions/cutex.solo/app-server/requests";
    for (token, method, expected) in [
        (send_token.token.as_str(), "turn/start", "owner_required"),
        (send_token.token.as_str(), "turn/steer", "owner_required"),
        (send_token.token.as_str(), "thread/settings/update", "owner_required"),
        (read_token.token.as_str(), "turn/interrupt", "send_not_granted"),
    ] {
        let (got, value) = exchange("POST", native_path, Some(token), &native(method));
        assert_eq!((got, code(&value)), (403, expected), "{method}");
    }
    let (got, value) = exchange("POST", "/v2/sessions/cutex.solo/app-server/server-request-responses", Some(&read_token.token), &json!({}));
    assert_eq!((got, code(&value)), (403, "send_not_granted"));
    let (got, value) = exchange("POST", "/v2/sessions/cutex.other/cutex/requests", Some(&send_token.token), &cutex("cutex/userInput/submit", text));
    assert_eq!((got, code(&value)), (404, "session_not_found"));
}

fn png(side: u32) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(side, side).write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

#[test]
fn self_service_profiles_and_avatar_visibility() {
    use base64::Engine;
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let mut store = crate::session::model::CutexSessionStore::default();
    for id in ["cutex.profile-shared", "cutex.profile-apart"] {
        let mut record = crate::session::model::CutexSessionRecord::new(
            id.to_string(), None, "test-host".to_string(), "/tmp".to_string(), None,
        )
        .unwrap();
        record.exposed_to_backend = true;
        store.sessions.insert(id.to_string(), record);
    }
    crate::session::store::save_cutex_session_store(&store).unwrap();
    let tokens = ClientTokenStore::open_default().unwrap();
    let owner = tokens.issue("owner-desktop").unwrap();
    let users = UserStore::open_default().unwrap();
    users.snapshot().unwrap();
    let pending = tokens.issue("pending").unwrap();
    let shared = |send| Grant { standalone_agents: vec!["cutex.profile-shared".into()], send, ..Grant::default() };
    let alice = users.create_user("Alias A", None, shared(true), "root").unwrap();
    let bob = users.create_user("Alias B", None, shared(false), "root").unwrap();
    let carol = users
        .create_user("Alias C", None, Grant { standalone_agents: vec!["cutex.profile-apart".into()], ..Grant::default() }, "root")
        .unwrap();
    let a = tokens.issue("alice-laptop").unwrap();
    let b = tokens.issue("bob-laptop").unwrap();
    let c = tokens.issue("carol-laptop").unwrap();
    users.bind_token(&a.token_id, Some(&alice.id), "root").unwrap();
    users.bind_token(&b.token_id, Some(&bob.id), "root").unwrap();
    users.bind_token(&c.token_id, Some(&carol.id), "root").unwrap();
    std::thread::sleep(Duration::from_millis(30));

    let avatar = |side| json!({"contentType": "image/png", "data": base64::engine::general_purpose::STANDARD.encode(png(side))});

    // Everyone assigned reads and edits only their own record.
    let (status, me) = exchange("GET", "/v2/users/me", Some(&a.token), &json!({}));
    assert_eq!(status, 200);
    assert_eq!(me["user"], json!({"id": alice.id, "name": "Alias A", "isOwner": false, "displayName": null, "avatarEtag": null}));
    crate::management::v2::schema_export::assert_valid("users", "selfProfile", &me);
    let (status, me) = exchange("PATCH", "/v2/users/me", Some(&a.token), &json!({"displayName": "Alice", "avatar": avatar(128)}));
    assert_eq!(status, 200, "{me}");
    assert_eq!(me["user"]["displayName"], "Alice");
    let alice_etag = me["user"]["avatarEtag"].as_str().unwrap().to_string();
    assert_eq!(alice_etag.len(), 16);
    let (status, me) = exchange("PATCH", "/v2/users/me", Some(&owner.token), &json!({"displayName": "Riley", "avatar": avatar(64)}));
    assert_eq!((status, &me["user"]["id"]), (200, &json!(OWNER)));
    let (status, me) = exchange("GET", "/v2/users/me", Some(ROOT), &json!({}));
    assert_eq!((status, &me["user"]["displayName"]), (200, &json!("Riley")), "root credential = owner");

    for (body, expected) in [
        (json!({"grant": {"projects": "all", "send": true}}), "invalid_request"),
        (json!({"name": "rename my alias"}), "invalid_request"),
        (json!({"displayName": ""}), "invalid_request"),
        (json!({"avatar": {"contentType": "image/webp", "data": base64::engine::general_purpose::STANDARD.encode(png(128))}}), "invalid_avatar"),
        (json!({"avatar": avatar(600)}), "invalid_avatar"),
    ] {
        let (status, value) = exchange("PATCH", "/v2/users/me", Some(&b.token), &body);
        assert_eq!((status, code(&value)), (400, expected), "{body}");
    }
    let (status, value) = exchange("PATCH", "/v2/users/me", Some(&pending.token), &json!({"displayName": "Mallory"}));
    assert_eq!((status, code(&value)), (403, "access_pending"));
    let (status, value) = exchange("GET", &format!("/v2/users/{}/avatar", alice.id), Some(&pending.token), &json!({}));
    assert_eq!((status, code(&value)), (403, "access_pending"));
    // Another user's record or the users API stay the owner's.
    let (status, value) = exchange("PATCH", &format!("/v2/users/{}", bob.id), Some(&a.token), &json!({"name": "x"}));
    assert_eq!((status, code(&value)), (403, "owner_required"));

    // Capabilities and the owner's listing carry the profile.
    let (_, caps) = exchange("GET", "/v2/capabilities", Some(&a.token), &json!({}));
    assert!(caps["featureList"].as_array().unwrap().iter().any(|f| f == "users-self-profile"));
    assert_eq!((&caps["user"]["displayName"], &caps["user"]["avatarEtag"]), (&json!("Alice"), &json!(alice_etag)));
    crate::management::v2::schema_export::assert_valid("users", "capabilityUser", &caps["user"]);
    let (_, caps) = exchange("GET", "/v2/capabilities", Some(&owner.token), &json!({}));
    assert_eq!(caps["user"]["displayName"], "Riley");
    let (_, caps) = exchange("GET", "/v2/capabilities", Some(&pending.token), &json!({}));
    assert!(caps["user"].get("displayName").is_none());
    let (status, listing) = exchange("GET", "/v2/users", Some(&owner.token), &json!({}));
    assert_eq!(status, 200);
    crate::management::v2::schema_export::assert_valid("users", "userList", &listing);
    let row = |id: &str| listing["users"].as_array().unwrap().iter().find(|u| u["id"] == id).unwrap().clone();
    assert_eq!((row(&alice.id)["name"].clone(), row(&alice.id)["displayName"].clone()), (json!("Alias A"), json!("Alice")));
    assert_eq!(row(&bob.id)["avatarEtag"], Value::Null);

    // Avatars: owner sees everyone, a user the owner, itself and users sharing an agent.
    let avatar_path = |id: &str| format!("/v2/users/{id}/avatar");
    let (status, head, bytes) = exchange_raw("GET", &avatar_path(&alice.id), Some(&owner.token), &json!({}), "");
    assert_eq!(status, 200);
    assert!(head.contains("Content-Type: image/png") && head.contains(&format!("ETag: \"{alice_etag}\"")), "{head}");
    assert_eq!(bytes, png(128));
    let (status, _, bytes) = exchange_raw("GET", &avatar_path(&alice.id), Some(&b.token), &json!({}), &format!("If-None-Match: \"{alice_etag}\"\r\n"));
    assert_eq!((status, bytes.len()), (304, 0), "shared agent → visible, ETag honoured");
    let (status, _, _) = exchange_raw("GET", &avatar_path(OWNER), Some(&c.token), &json!({}), "");
    assert_eq!(status, 200, "everyone sees the owner");
    let (status, value) = exchange("GET", &avatar_path(&alice.id), Some(&c.token), &json!({}));
    assert_eq!((status, code(&value)), (404, "avatar_not_found"), "no shared agent → invisible");
    let (status, value) = exchange("GET", &avatar_path(&bob.id), Some(&a.token), &json!({}));
    assert_eq!((status, code(&value)), (404, "avatar_not_found"), "visible but no avatar");

    // actingUser of a non-owner send carries the profile.
    crate::management::users::set_current_principal(Some(users.principal_for_token(&a.token_id)));
    let acting = crate::management::users::current_acting_user().unwrap();
    crate::management::users::set_current_principal(None);
    assert_eq!(acting, json!({"id": alice.id, "name": "Alias A", "displayName": "Alice", "avatarEtag": alice_etag}));
    crate::management::v2::schema_export::assert_valid("users", "actingUser", &acting);

    // Clearing; deleting a user drops its profile.
    let (status, me) = exchange("PATCH", "/v2/users/me", Some(&a.token), &json!({"avatar": null}));
    assert_eq!((status, &me["user"]["avatarEtag"]), (200, &Value::Null));
    users.bind_token(&c.token_id, None, "root").unwrap();
    let (status, _) = exchange("PATCH", "/v2/users/me", Some(&b.token), &json!({"displayName": "Bob"}));
    assert_eq!(status, 200);
    users.bind_token(&b.token_id, None, "root").unwrap();
    let (status, _) = exchange("DELETE", &format!("/v2/users/{}", bob.id), Some(&owner.token), &json!({}));
    assert_eq!(status, 200);
    let profiles = crate::management::user_profiles::ProfileStore::open_default().unwrap();
    assert!(profiles.snapshot().unwrap().profile(&bob.id).is_none());
}

/// user-own-token / user-own-devices: every connection sees its own token;
/// owner, root and members list / revoke only their own devices; pending
/// devices get 403 user_pending.
#[test]
fn own_token_and_own_devices_for_owner_member_pending_and_root() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let tokens = ClientTokenStore::open_default().unwrap();
    let (owner, _) = tokens.issue_device("owner-desktop", "Owner PC", "windows").unwrap();
    let users = UserStore::open_default().unwrap();
    users.snapshot().unwrap();
    let (owner_phone, _) = tokens.issue_device("owner-phone", "Owner Phone", "android").unwrap();
    users.bind_token(&owner_phone.token_id, Some(OWNER), "root").unwrap();
    let alice = users.create_user("Alice", None, Grant::default(), "root").unwrap();
    let bob = users.create_user("Bob", None, Grant::default(), "root").unwrap();
    let (a1, _) = tokens.issue_device("alice-laptop", "Alice Laptop", "linux").unwrap();
    let a2 = tokens.issue("alice-cli").unwrap();
    let (b1, _) = tokens.issue_device("bob-laptop", "Bob Laptop", "macos").unwrap();
    let (pending, _) = tokens.issue_device("stranger", "Stranger PC", "windows").unwrap();
    users.bind_token(&a1.token_id, Some(&alice.id), "root").unwrap();
    users.bind_token(&a2.token_id, Some(&alice.id), "root").unwrap();
    users.bind_token(&b1.token_id, Some(&bob.id), "root").unwrap();
    tokens.rename(&a1.token_id, Some("Alice's work laptop")).unwrap();
    std::thread::sleep(Duration::from_millis(30));

    // C1: capabilities user.token, for every kind of connection.
    let caps = |bearer: &str| exchange("GET", "/v2/capabilities", Some(bearer), &json!({})).1;
    let value = caps(&a1.token);
    assert!(value["featureList"].as_array().unwrap().iter().any(|f| f == "user-own-token"));
    assert!(value["featureList"].as_array().unwrap().iter().any(|f| f == "user-own-devices"));
    assert_eq!(value["user"]["token"], json!({
        "id": a1.token_id, "label": "alice-laptop", "deviceName": "Alice Laptop",
        "ownerName": "Alice's work laptop", "platform": "linux", "scope": "full", "issuedVia": "pairing",
    }));
    crate::management::v2::schema_export::assert_valid("users", "capabilityUser", &value["user"]);
    let value = caps(&a2.token);
    assert_eq!(value["user"]["token"], json!({
        "id": a2.token_id, "label": "alice-cli", "deviceName": null, "ownerName": null, "platform": null, "scope": "full", "issuedVia": "hostCli",
    }));
    let value = caps(&owner.token);
    assert_eq!((&value["user"]["isOwner"], &value["user"]["token"]["id"]), (&json!(true), &json!(owner.token_id)));
    crate::management::v2::schema_export::assert_valid("users", "capabilityUser", &value["user"]);
    let value = caps(&pending.token);
    assert_eq!((&value["user"]["pending"], &value["user"]["token"]["deviceName"]), (&json!(true), &json!("Stranger PC")));
    crate::management::v2::schema_export::assert_valid("users", "capabilityUser", &value["user"]);
    let value = caps(ROOT);
    assert_eq!(value["user"]["token"], Value::Null, "root credential: no client token");
    crate::management::v2::schema_export::assert_valid("users", "capabilityUser", &value["user"]);

    // C2: listing — a member sees exactly its own tokens, current flagged.
    let devices = |bearer: &str| exchange("GET", "/v2/users/me/devices", Some(bearer), &json!({}));
    let ids = |value: &Value| {
        let mut ids: Vec<(String, bool)> = value["devices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| (d["tokenId"].as_str().unwrap().to_string(), d["current"].as_bool().unwrap()))
            .collect();
        ids.sort();
        ids
    };
    let sorted = |mut v: Vec<(String, bool)>| { v.sort(); v };
    let (status, value) = devices(&a1.token);
    assert_eq!(status, 200, "{value}");
    crate::management::v2::schema_export::assert_valid("users", "ownDevices", &value);
    assert_eq!(value["userId"], json!(alice.id));
    assert_eq!(ids(&value), sorted(vec![(a1.token_id.clone(), true), (a2.token_id.clone(), false)]));
    let item = value["devices"].as_array().unwrap().iter().find(|d| d["tokenId"] == a1.token_id).unwrap().clone();
    assert_eq!((&item["ownerName"], &item["deviceName"], &item["kind"], &item["pending"], &item["userId"]),
        (&json!("Alice's work laptop"), &json!("Alice Laptop"), &json!("device"), &json!(false), &json!(alice.id)));
    let (_, value) = devices(&b1.token);
    assert_eq!(ids(&value), vec![(b1.token_id.clone(), true)]);
    // Owner token and root: the owner user's devices.
    let (status, value) = devices(&owner.token);
    assert_eq!((status, &value["userId"]), (200, &json!(OWNER)));
    assert_eq!(ids(&value), sorted(vec![(owner.token_id.clone(), true), (owner_phone.token_id.clone(), false)]));
    let (status, value) = devices(ROOT);
    assert_eq!(status, 200);
    assert_eq!(ids(&value), sorted(vec![(owner.token_id.clone(), false), (owner_phone.token_id.clone(), false)]));
    // Pending: 403 user_pending on both routes.
    let (status, value) = devices(&pending.token);
    assert_eq!((status, code(&value)), (403, "user_pending"));
    let path = |id: &str| format!("/v2/users/me/devices/{id}");
    let (status, value) = exchange("DELETE", &path(&pending.token_id), Some(&pending.token), &json!({}));
    assert_eq!((status, code(&value)), (403, "user_pending"));

    // Removal: another user's / the owner's / an unknown token reads as 404.
    for target in [&b1.token_id, &owner.token_id, &pending.token_id, &"mct-doesnotexist".to_string()] {
        let (status, value) = exchange("DELETE", &path(target), Some(&a1.token), &json!({}));
        assert_eq!((status, code(&value)), (404, "client_token_not_found"), "{target}");
    }
    let (status, value) = exchange("DELETE", &path(&a2.token_id), Some(&owner.token), &json!({}));
    assert_eq!((status, code(&value)), (404, "client_token_not_found"), "the owner's me = the owner user only");
    // Other methods stay closed for members.
    let (status, value) = exchange("POST", "/v2/users/me/devices", Some(&a1.token), &json!({}));
    assert_eq!((status, code(&value)), (403, "owner_required"));
    let (status, value) = exchange("GET", &path(&a2.token_id), Some(&a1.token), &json!({}));
    assert_eq!((status, code(&value)), (403, "owner_required"));
    let (status, value) = exchange("POST", "/v2/users/me/devices", Some(ROOT), &json!({}));
    assert_eq!((status, code(&value)), (405, "method_not_allowed"));

    // A member removes its other device, then itself (= signing out).
    let (status, value) = exchange("DELETE", &path(&a2.token_id), Some(&a1.token), &json!({}));
    assert_eq!(status, 200, "{value}");
    assert_eq!(value, json!({"tokenId": a2.token_id, "revoked": true, "current": false}));
    crate::management::v2::schema_export::assert_valid("users", "ownDeviceRevoked", &value);
    assert!(tokens.list().unwrap().iter().all(|t| t.token_id != a2.token_id));
    let (status, _) = devices(&a2.token);
    assert_eq!(status, 401, "a revoked token is refused");
    let (status, value) = exchange("DELETE", &path(&a1.token_id), Some(&a1.token), &json!({}));
    assert_eq!((status, &value["current"]), (200, &json!(true)));
    std::thread::sleep(Duration::from_millis(30));
    let (status, _) = devices(&a1.token);
    assert_eq!(status, 401, "self-removal signs the device out");
    // The owner removes its own phone; root sees the rest.
    let (status, _) = exchange("DELETE", &path(&owner_phone.token_id), Some(ROOT), &json!({}));
    assert_eq!(status, 200);
    let (_, value) = devices(&owner.token);
    assert_eq!(ids(&value), vec![(owner.token_id.clone(), true)]);
    // Bob and the pending device were untouched.
    let remaining: Vec<String> = tokens.list().unwrap().into_iter().map(|t| t.token_id).collect();
    assert!(remaining.contains(&b1.token_id) && remaining.contains(&pending.token_id));
}

/// Person identity through the dispatcher: challenge / claim for every
/// principal (pending included), auto-binding, owner-only key management
/// and the decorated listing.
#[test]
fn person_identity_claims_and_owner_key_management() {
    use base64::Engine;
    use ring::signature::KeyPair;
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let tokens = ClientTokenStore::open_default().unwrap();
    let users = UserStore::open_default().unwrap();
    let b64 = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let person = |seed: u8| ring::signature::Ed25519KeyPair::from_seed_unchecked(&[seed; 32]).unwrap();
    let (me, colleague) = (person(11), person(12));
    let public = |pair: &ring::signature::Ed25519KeyPair| b64(pair.public_key().as_ref());
    let challenge = |bearer: &str| {
        let (status, value) = exchange("GET", "/v2/identity/challenge", Some(bearer), &json!({}));
        assert_eq!(status, 200, "{value}");
        crate::management::v2::schema_export::assert_valid("users", "identityChallenge", &value);
        value
    };
    let signed = |pair: &ring::signature::Ed25519KeyPair, challenge: &Value| {
        let message = crate::management::persons::signing_message(
            challenge["hostId"].as_str().unwrap(),
            challenge["nonce"].as_str().unwrap(),
            challenge["tokenId"].as_str().unwrap(),
        );
        json!({"publicKey": public(pair), "signature": b64(pair.sign(message.as_bytes()).as_ref()), "nonce": challenge["nonce"]})
    };

    let owner_device = tokens.issue("owner-desktop").unwrap();
    users.snapshot().unwrap();
    // The local desktop (root credential) records its person on owner.
    let c = challenge(ROOT);
    assert_eq!(c["tokenId"], "root");
    let (status, result) = exchange("POST", "/v2/identity/claim", Some(ROOT), &signed(&me, &c));
    assert_eq!((status, &result["outcome"]), (200, &json!("ownerKeyAdded")), "{result}");
    crate::management::v2::schema_export::assert_valid("users", "identityClaimResult", &result);
    // Each nonce works once.
    let (status, value) = exchange("POST", "/v2/identity/claim", Some(ROOT), &signed(&me, &c));
    assert_eq!((status, code(&value)), (400, "challenge_invalid"));

    // A pending device of the same person: bound to owner at once.
    let laptop = tokens.issue("laptop").unwrap();
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(users.principal_for_token(&laptop.token_id), crate::management::users::Principal::Pending);
    let c = challenge(&laptop.token);
    assert_eq!(c["tokenId"], laptop.token_id.as_str());
    // A wrong signature spends the nonce and changes nothing.
    let mut forged = signed(&colleague, &c);
    forged["publicKey"] = json!(public(&me));
    let (status, value) = exchange("POST", "/v2/identity/claim", Some(&laptop.token), &forged);
    assert_eq!((status, code(&value)), (400, "invalid_signature"));
    let (status, value) = exchange("POST", "/v2/identity/claim", Some(&laptop.token), &signed(&me, &c));
    assert_eq!((status, code(&value)), (400, "challenge_invalid"));
    // A claim signed for another token does not verify for this caller.
    let c = challenge(&laptop.token);
    let mut other_token = c.clone();
    other_token["tokenId"] = json!(owner_device.token_id);
    let (status, value) = exchange("POST", "/v2/identity/claim", Some(&laptop.token), &signed(&me, &other_token));
    assert_eq!((status, code(&value)), (400, "invalid_signature"));
    // Without `nonce` the caller's live nonces are tried.
    let c = challenge(&laptop.token);
    let mut body = signed(&me, &c);
    body.as_object_mut().unwrap().remove("nonce");
    let (status, result) = exchange("POST", "/v2/identity/claim", Some(&laptop.token), &body);
    assert_eq!(status, 200, "{result}");
    assert_eq!((&result["outcome"], &result["userId"], &result["pending"]), (&json!("autoBound"), &json!(OWNER), &json!(false)));
    std::thread::sleep(Duration::from_millis(30));
    let (status, _) = exchange("GET", "/v2/users", Some(&laptop.token), &json!({}));
    assert_eq!(status, 200, "an auto-bound owner device has owner rights");

    // An unknown person on a pending device stays pending, with its name.
    let guest = tokens.issue("guest").unwrap();
    std::thread::sleep(Duration::from_millis(30));
    let c = challenge(&guest.token);
    let mut body = signed(&colleague, &c);
    body["displayName"] = json!("Colleague");
    let (status, result) = exchange("POST", "/v2/identity/claim", Some(&guest.token), &body);
    assert_eq!((status, &result["outcome"], &result["pending"]), (200, &json!("pending"), &json!(true)), "{result}");
    let (status, value) = exchange("GET", "/v2/settings", Some(&guest.token), &json!({}));
    assert_eq!((status, code(&value)), (403, "access_pending"));
    // Key management is the owner's.
    let user = users.create_user("Colleague", None, Grant::default(), "root").unwrap();
    let keys_path = format!("/v2/users/{}/person-keys", user.id);
    for bearer in [&guest.token] {
        let (status, value) = exchange("PUT", &keys_path, Some(bearer), &json!({"personKeys": []}));
        assert_eq!((status, code(&value)), (403, "access_pending"));
    }
    let (status, value) = exchange("PUT", &keys_path, Some(&laptop.token), &json!({"personKeys": [{"publicKey": public(&me)}]}));
    assert_eq!((status, code(&value)), (409, "person_key_in_use"));
    let (status, value) = exchange("PUT", &keys_path, Some(&laptop.token), &json!({"personKeys": [{"publicKey": public(&colleague), "label": "Colleague"}]}));
    assert_eq!(status, 200, "{value}");
    crate::management::v2::schema_export::assert_valid("users", "personKeysResult", &value);
    let (status, _) = exchange("PATCH", "/v2/users/owner/person-keys", Some(&laptop.token), &json!({}));
    assert_eq!(status, 405);
    // The colleague's next claim on that pending device binds it.
    let c = challenge(&guest.token);
    let (_, result) = exchange("POST", "/v2/identity/claim", Some(&guest.token), &signed(&colleague, &c));
    assert_eq!((&result["outcome"], &result["userId"]), (&json!("autoBound"), &json!(user.id)));
    // A bound device presenting another person is only recorded.
    let c = challenge(&guest.token);
    let (_, result) = exchange("POST", "/v2/identity/claim", Some(&guest.token), &signed(&me, &c));
    assert_eq!((&result["outcome"], &result["userId"], &result["personUserId"]), (&json!("recorded"), &json!(user.id), &json!(OWNER)));

    // The owner's listing.
    let (status, listing) = exchange("GET", "/v2/users", Some(&laptop.token), &json!({}));
    assert_eq!(status, 200);
    crate::management::v2::schema_export::assert_valid("users", "userList", &listing);
    let row = |id: &str| listing["users"].as_array().unwrap().iter().find(|u| u["id"] == id).unwrap().clone();
    assert_eq!(row(OWNER)["personKeys"][0]["publicKey"], public(&me));
    assert_eq!(row(&user.id)["personKeys"][0]["label"], "Colleague");
    assert_eq!(listing["tokenPersons"][&laptop.token_id], public(&me));
    let token = listing["tokens"].as_array().unwrap().iter().find(|t| t["tokenId"] == guest.token_id.as_str()).unwrap();
    assert_eq!(token["person"]["publicKey"], public(&me));
    assert_eq!(token["person"]["previousPublicKey"], public(&colleague));
    assert_eq!(token["person"]["displayName"], "Colleague");
    assert!(!listing.to_string().contains("cxm1."), "no secrets in the listing");
    let (_, caps) = exchange("GET", "/v2/capabilities", Some(&guest.token), &json!({}));
    assert!(caps["featureList"].as_array().unwrap().iter().any(|f| f == "person-identity"));
    // Other methods on the identity routes are not for non-owners.
    let (status, value) = exchange("DELETE", "/v2/identity/claim", Some(&guest.token), &json!({}));
    assert_eq!((status, code(&value)), (403, "owner_required"));

    // Acknowledging the change (owner only) clears previousPublicKey.
    let ack = format!("/v2/users/token-persons/{}/previous", guest.token_id);
    let (status, value) = exchange("DELETE", &ack, Some(&guest.token), &json!({}));
    assert_eq!((status, code(&value)), (403, "owner_required"));
    let (status, value) = exchange("DELETE", &ack, Some(&laptop.token), &json!({}));
    assert_eq!((status, &value["cleared"]), (200, &json!(true)), "{value}");
    crate::management::v2::schema_export::assert_valid("users", "tokenPersonPreviousCleared", &value);
    let (_, listing) = exchange("GET", "/v2/users", Some(ROOT), &json!({}));
    crate::management::v2::schema_export::assert_valid("users", "userList", &listing);
    let token = listing["tokens"].as_array().unwrap().iter().find(|t| t["tokenId"] == guest.token_id.as_str()).unwrap();
    assert_eq!((&token["person"]["previousPublicKey"], &token["person"]["publicKey"]), (&Value::Null, &json!(public(&me))));
    assert_eq!(users.principal_for_token(&guest.token_id), crate::management::users::Principal::User {
        id: user.id.clone(), name: "Colleague".into(), grant: Grant::default(),
    });
    let (status, value) = exchange("DELETE", "/v2/users/token-persons/mct-missing/previous", Some(ROOT), &json!({}));
    assert_eq!((status, code(&value)), (404, "client_token_not_found"));
}

#[test]
fn narrowed_host_cli_owner_tokens_cannot_read_host_control_credentials() {
    use crate::management::client_tokens::DeviceScope;
    let _lock = crate::config::paths::test_home_env_lock();
    let instance = PrivateInstance::new();
    let tokens = ClientTokenStore::open_default().unwrap();
    let users = UserStore::open_default().unwrap();
    let full = tokens.issue("secret-full-owner").unwrap();
    users.snapshot().unwrap();
    let mut config = crate::config::store::load_codez_config();
    config.agent_bus_token = Some("synthetic-control-secret-audit".into());
    crate::config::store::save_codez_config(&config).unwrap();
    let config = crate::config::paths::config_path().unwrap();
    let link = crate::config::paths::config_dir().unwrap().join("runtime/profile-link/secret-v1");
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::fs::write(&link,"synthetic-control-secret-audit").unwrap();
    let report = instance.root.join("report.txt");
    std::fs::write(&report,"ordinary report").unwrap();
    let url = |path: &std::path::Path| format!("/v2/fs/file?path={}", url::form_urlencoded::byte_serialize(path.to_string_lossy().as_bytes()).collect::<String>());
    for scope in [DeviceScope::Read, DeviceScope::Chat] {
        let token = tokens.issue_scoped("narrowed-secret-owner",scope).unwrap();
        users.bind_token(&token.token_id,Some(OWNER),"root").unwrap();
        let caps = exchange("GET","/v2/capabilities",Some(&token.token),&json!({})).1;
        assert_eq!(caps["user"]["ownerLocal"],true,"location semantics remain unchanged");
        for secret in [&config,&link] {
            let (status,value) = exchange("GET",&url(secret),Some(&token.token),&json!({}));
            assert_eq!(status,403,"a narrowed token could recover a control credential");
            assert!(!value.to_string().contains("synthetic-control-secret-audit"));
        }
        assert_eq!(exchange("GET",&url(&report),Some(&token.token),&json!({})).0,200);
    }
    for bearer in [ROOT,full.token.as_str()] {
        assert_eq!(exchange("GET",&url(&config),Some(bearer),&json!({})).0,200);
    }
}

/// A context whose session lookups work on the private instance (empty
/// registry, no runtime); user input stops at a marker error.
fn p2_context() -> ManagementRequestContext {
    let mut context = context();
    context.load_registry = || Ok(Default::default());
    context.load_runtime_status = |_| Ok(None);
    context.handle_user_input = |_| {
        Err(crate::management::v2::user_input::UserInputExecutionError {
            stage: "route".into(),
            code: "test_user_input_reached".into(),
            message: "the access gate let the input through".into(),
            retryable: false,
            details: json!({}),
            outcome_unknown: false,
        })
    };
    context
}

fn p2_exchange(method: &str, path: &str, bearer: &str, body: &Value) -> (u16, Value) {
    let (status, _, body) = exchange_with(p2_context(), method, path, Some(bearer), body.to_string().into_bytes(), "application/json", "");
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// P2 access (capability users-p2-access): read-only workspace files of
/// visible agents, own attachments as images, delegation commands on
/// visible asks.
#[test]
fn p2_users_read_workspaces_send_own_images_and_act_on_visible_asks() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let instance = PrivateInstance::new();
    let work = instance.root.join("work");
    std::fs::create_dir_all(work.join("src")).unwrap();
    std::fs::write(work.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(instance.root.join("outside-secret.txt"), "secret").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(instance.root.join("outside-secret.txt"), work.join("escape.txt")).unwrap();
    let has_git = std::process::Command::new("git").args(["init", "-q"]).current_dir(&work).status().is_ok_and(|s| s.success());
    let host = crate::platform::host::current_host_name();
    let mut store = crate::session::model::CutexSessionStore::default();
    for (id, cwd, exposed) in [
        ("cutex.p2-solo", work.clone(), true),
        ("cutex.p2-other", work.clone(), true),
        ("cutex.p2-broad", instance.root.clone(), true),
        ("cutex.p2-unexposed", work.clone(), false),
    ] {
        let mut record = crate::session::model::CutexSessionRecord::new(
            id.to_string(), None, host.clone(), cwd.to_string_lossy().into_owned(), None,
        )
        .unwrap();
        record.exposed_to_backend = exposed;
        record.registration_class = crate::agent_bus::model::AgentRegistrationClass::Persistent;
        store.sessions.insert(id.to_string(), record);
    }
    crate::session::store::save_cutex_session_store(&store).unwrap();
    let tokens = ClientTokenStore::open_default().unwrap();
    let users = UserStore::open_default().unwrap();
    users.snapshot().unwrap();
    let visible = vec!["cutex.p2-solo".to_string(), "cutex.p2-broad".to_string(), "cutex.p2-unexposed".to_string()];
    let reader = users.create_user("Reader", None, Grant { standalone_agents: visible.clone(), ..Grant::default() }, "root").unwrap();
    let sender = users.create_user("Sender", None, Grant { standalone_agents: visible.clone(), send: true, ..Grant::default() }, "root").unwrap();
    let read_token = tokens.issue("p2-reader").unwrap();
    let send_token = tokens.issue("p2-sender").unwrap();
    let send_token_2 = tokens.issue("p2-sender-phone").unwrap();
    users.bind_token(&read_token.token_id, Some(&reader.id), "root").unwrap();
    users.bind_token(&send_token.token_id, Some(&sender.id), "root").unwrap();
    users.bind_token(&send_token_2.token_id, Some(&sender.id), "root").unwrap();
    std::thread::sleep(Duration::from_millis(30));
    let (_, caps) = exchange("GET", "/v2/capabilities", Some(&read_token.token), &json!({}));
    assert!(caps["featureList"].as_array().unwrap().iter().any(|f| f == "users-p2-access"));

    // (a) Read-only files of a visible agent's cwd, read grant suffices.
    let (status, dir) = p2_exchange("GET", "/v2/sessions/cutex.p2-solo/fs/dir", &read_token.token, &json!({}));
    assert_eq!(status, 200, "{dir}");
    assert!(dir["entries"].as_array().unwrap().iter().any(|e| e["name"] == "src"), "{dir}");
    let (status, file) = p2_exchange("GET", "/v2/sessions/cutex.p2-solo/fs/file?path=src/main.rs", &read_token.token, &json!({}));
    assert_eq!((status, &file["content"]), (200, &json!("fn main() {}\n")), "{file}");
    let (status, _) = p2_exchange("GET", "/v2/sessions/cutex.p2-solo/fs/search?q=main", &read_token.token, &json!({}));
    assert_eq!(status, 200);
    let (status, git) = p2_exchange("GET", "/v2/sessions/cutex.p2-solo/git/status", &read_token.token, &json!({}));
    assert_eq!(status, 200, "{git}");
    for (path, expected) in [
        ("/v2/sessions/cutex.p2-solo/fs/file?path=../outside-secret.txt", (403, "path_outside_root")),
        ("/v2/sessions/cutex.p2-solo/fs/file?path=src/../../outside-secret.txt", (403, "path_outside_root")),
        ("/v2/sessions/cutex.p2-broad/fs/dir", (403, "workspace_root_too_broad")),
        ("/v2/sessions/cutex.p2-broad/fs/file?path=.cutex/config.json", (403, "workspace_root_too_broad")),
        ("/v2/sessions/cutex.p2-other/fs/dir", (404, "session_not_found")),
        ("/v2/projects/p-not-granted/fs/dir", (404, "project_not_found")),
    ] {
        let (status, value) = p2_exchange("GET", path, &read_token.token, &json!({}));
        assert_eq!((status, code(&value)), expected, "{path}");
    }
    // A granted agent with the retired IM exposure flag off is visible like any other.
    let (status, value) = p2_exchange("GET", "/v2/sessions/cutex.p2-unexposed/fs/dir", &read_token.token, &json!({}));
    assert_eq!(status, 200, "{value}");
    #[cfg(unix)]
    {
        let (status, value) = p2_exchange("GET", "/v2/sessions/cutex.p2-solo/fs/file?path=escape.txt", &read_token.token, &json!({}));
        assert_eq!((status, code(&value)), (403, "path_outside_root"), "symlink escape");
    }
    if has_git {
        let (status, value) = p2_exchange("GET", "/v2/sessions/cutex.p2-solo/git/diff?path=../outside-secret.txt", &read_token.token, &json!({}));
        assert_eq!((status, code(&value)), (403, "path_outside_root"));
        let (status, value) = p2_exchange("GET", "/v2/sessions/cutex.p2-solo/git/diff?path=src/main.rs", &read_token.token, &json!({}));
        assert_eq!(status, 200, "{value}");
        assert!(value.to_string().contains("fn main"), "untracked file diffed against /dev/null: {value}");
        #[cfg(unix)]
        {
            let (status, value) = p2_exchange("GET", "/v2/sessions/cutex.p2-solo/git/diff?path=escape.txt", &read_token.token, &json!({}));
            assert_eq!(status, 200, "{value}");
            assert!(!value.to_string().contains("secret"), "an untracked symlink out of the root is not diffed: {value}");
        }
    }
    // Writes, terminals and host-wide files stay the owner's.
    for (method, path) in [
        ("POST", "/v2/sessions/cutex.p2-solo/terminals"),
        ("GET", "/v2/sessions/cutex.p2-solo/terminals"),
        ("POST", "/v2/sessions/cutex.p2-solo/fs/file"),
        ("GET", "/v2/fs/file?path=/etc/hostname"),
        ("GET", "/v2/fs/dirs"),
    ] {
        let (status, value) = p2_exchange(method, path, &send_token.token, &json!({}));
        assert_eq!((status, code(&value)), (403, "owner_required"), "{method} {path}");
    }
    // The owner keeps the broad root.
    let (status, _) = p2_exchange("GET", "/v2/sessions/cutex.p2-broad/fs/dir", ROOT, &json!({}));
    assert_eq!(status, 200);

    // (b) Image inputs: own uploads only.
    let upload = |bearer: &str, session: &str| {
        let (status, _, body) = exchange_with(
            p2_context(), "POST", &format!("/v2/sessions/{session}/attachments"), Some(bearer), png(8), "image/png", "",
        );
        (status, serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null))
    };
    let (status, value) = upload(&read_token.token, "cutex.p2-solo");
    assert_eq!((status, code(&value)), (403, "send_not_granted"));
    let (status, value) = upload(&send_token.token, "cutex.p2-other");
    assert_eq!((status, code(&value)), (404, "session_not_found"));
    let (status, attachment) = upload(&send_token.token, "cutex.p2-solo");
    assert_eq!(status, 201, "{attachment}");
    let own_path = attachment["path"].as_str().unwrap().to_string();
    assert!(crate::management::v2::attachments::uploaded_by("cutex.p2-solo", &send_token.token_id, &own_path));
    assert!(!crate::management::v2::attachments::uploaded_by("cutex.p2-solo", &send_token_2.token_id, &own_path));
    let submit = |path: &str| json!({
        "requestId": "r-img", "method": "cutex/userInput/submit",
        "params": {"clientUserMessageId": "m-img", "origin": {"kind": "backend", "clientId": "c"}, "strategy": "queue",
            "input": [{"type": "text", "text": "look"}, {"type": "localImage", "path": path}]},
    });
    let requests = "/v2/sessions/cutex.p2-solo/cutex/requests";
    let (status, value) = p2_exchange("POST", requests, &send_token.token, &submit(&own_path));
    // Past the gate: the handler answers (no runtime in this test).
    assert_ne!(status, 403, "{value}");
    assert!(matches!(code(&value), "session_offline" | "test_user_input_reached"), "{value}");
    for (bearer, path) in [
        (&send_token_2.token, own_path.as_str()),
        (&send_token.token, "/etc/hostname"),
        (&send_token.token, &format!("{own_path}.by-x") as &str),
    ] {
        let (status, value) = p2_exchange("POST", requests, bearer, &submit(path));
        assert_eq!((status, code(&value)), (403, "attachment_not_owned"), "{path}");
    }
    let (status, value) = p2_exchange("POST", requests, &send_token.token, &json!({
        "requestId": "r-url", "method": "cutex/userInput/submit",
        "params": {"clientUserMessageId": "m-url", "origin": {"kind": "backend", "clientId": "c"}, "strategy": "queue",
            "input": [{"type": "image", "url": "https://example.invalid/x.png"}]},
    }));
    assert_eq!((status, code(&value)), (403, "input_type_not_allowed"));

    // (c) Delegation commands on visible, related asks.
    let asks = crate::asks::store::AskStore::open_default().unwrap();
    let agent = |id: &str| crate::asks::model::Requester::Agent { cutex_session_id: id.into(), name: None };
    let mut new = crate::asks::store::tests::new_ask("p2-visible", "please");
    new.requester = agent("cutex.p2-solo");
    new.target_cutex_session_id = "cutex.p2-broad".into();
    let (visible_ask, _) = asks.with_state(|state| state.create_ask(new, chrono::Utc::now())).unwrap().unwrap();
    let mut new = crate::asks::store::tests::new_ask("p2-hidden", "secret");
    new.requester = agent("cutex.p2-solo");
    new.target_cutex_session_id = "cutex.p2-other".into();
    let (hidden_ask, _) = asks.with_state(|state| state.create_ask(new, chrono::Utc::now())).unwrap().unwrap();
    asks.with_state(|state|{
        let question:crate::asks::model::QuestionRecord=serde_json::from_value(json!({
            "questionId":"q-visible-mixed-target","askId":visible_ask.ask_id,"attempt":1,"runtimeGeneration":1,
            "native":{"threadId":"t","turnId":"u","itemId":"i","requestId":1},
            "questions":[],"route":"owner","state":"open","createdAt":"2026-11-17T00:00:00Z"
        })).unwrap();
        let mut hidden_question=question.clone();hidden_question.question_id="q-hidden-mixed-target".into();
        hidden_question.ask_id=hidden_ask.ask_id.clone();
        state.questions.insert(question.question_id.clone(),question);
        state.questions.insert(hidden_question.question_id.clone(),hidden_question);
    }).unwrap();
    let command = |method: &str, params: Value| json!({"requestId": format!("r-{method}"), "method": method, "params": params});
    for (bearer, session, body, expected) in [
        (&read_token.token, "cutex.p2-solo", command("cutex/ask/stop", json!({"askId": visible_ask.ask_id})), (403, "send_not_granted")),
        (&send_token.token, "cutex.p2-solo", command("cutex/ask/stop", json!({"askId": hidden_ask.ask_id})), (404, "ask_not_found")),
        (&send_token.token, "cutex.p2-solo", command("cutex/ask/stop", json!({"askId":hidden_ask.ask_id,"questionId":"q-visible-mixed-target"})), (404, "ask_not_found")),
        (&send_token.token, "cutex.p2-solo", command("cutex/ask/reopen", json!({"askId":hidden_ask.ask_id,"questionId":"q-visible-mixed-target","note":"private scope-confusion test"})), (404, "ask_not_found")),
        (&send_token.token, "cutex.p2-solo", command("cutex/ask/close", json!({"askId":hidden_ask.ask_id,"questionId":"q-visible-mixed-target","outcome":"abandoned"})), (404, "ask_not_found")),
        (&send_token.token, "cutex.p2-solo", command("cutex/ask/retry", json!({"askId":hidden_ask.ask_id,"questionId":"q-visible-mixed-target"})), (404, "ask_not_found")),
        (&send_token.token, "cutex.p2-solo", command("cutex/ask/resume", json!({"askId":hidden_ask.ask_id,"questionId":"q-visible-mixed-target"})), (404, "ask_not_found")),
        (&send_token.token, "cutex.p2-solo", command("cutex/ask/respond", json!({"askId":visible_ask.ask_id,"questionId":"q-hidden-mixed-target","answers":{}})), (404, "question_not_found")),
        (&send_token.token, "cutex.p2-other", command("cutex/ask/stop", json!({"askId": visible_ask.ask_id})), (404, "session_not_found")),
        (&send_token.token, "cutex.p2-solo", command("cutex/ask/respond", json!({"questionId": "q-missing", "answers": {}})), (404, "question_not_found")),
        (&send_token.token, "cutex.p2-solo", command("cutex/ask/create", json!({"text": "x", "actionId": "a"})), (403, "owner_required")),
        (&send_token.token, "cutex.p2-solo", command("cutex/asks/pause", json!({})), (403, "owner_required")),
    ] {
        let (status, value) = p2_exchange("POST", &format!("/v2/sessions/{session}/cutex/requests"), bearer, &body);
        assert_eq!((status, code(&value)), expected, "{body}");
    }
    // A visible ask through either end's path reaches the handler.
    let (status, value) = p2_exchange(
        "POST", "/v2/sessions/cutex.p2-broad/cutex/requests", &send_token.token,
        &command("cutex/ask/stop", json!({"askId": visible_ask.ask_id})),
    );
    assert_eq!(status, 200, "{value}");
    assert_eq!(value.pointer("/cutex/result/ask/askId"), Some(&json!(visible_ask.ask_id)), "{value}");
    assert_eq!(value.pointer("/cutex/result/ask/closed"), Some(&json!("stopped")), "{value}");
    let state = asks.snapshot().unwrap();
    assert!(state.asks[&hidden_ask.ask_id].closed.is_none(), "the refused ask is untouched");
}

/// Safety grants (capability `claude-safety-grants`): only the owner decides.
/// Non-owner users (read or send grant) are refused by the gate, a caller
/// without the root credential (an agent, any other process) by the route.
#[test]
fn safety_check_decisions_and_list_are_the_owners_only() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let mut store = crate::session::model::CutexSessionStore::default();
    let mut record = crate::session::model::CutexSessionRecord::new(
        "cutex.solo".to_string(), None, "test-host".to_string(), "/tmp".to_string(), None,
    )
    .unwrap();
    record.exposed_to_backend = true;
    store.sessions.insert("cutex.solo".to_string(), record);
    crate::session::store::save_cutex_session_store(&store).unwrap();
    let tokens = ClientTokenStore::open_default().unwrap();
    let users = UserStore::open_default().unwrap();
    let grant = Grant { standalone_agents: vec!["cutex.solo".into()], send: true, ..Grant::default() };
    let sender = users.create_user("Sender", None, grant.clone(), "root").unwrap();
    let reader = users.create_user("Reader", None, Grant { send: false, ..grant }, "root").unwrap();
    let send_token = tokens.issue("sender").unwrap();
    let read_token = tokens.issue("reader").unwrap();
    users.bind_token(&send_token.token_id, Some(&sender.id), "root").unwrap();
    users.bind_token(&read_token.token_id, Some(&reader.id), "root").unwrap();
    std::thread::sleep(Duration::from_millis(30));

    let path = "/v2/sessions/cutex.solo/cutex/requests";
    for method in ["cutex/safetyCheck/grant", "cutex/safetyCheck/dismiss"] {
        let body = json!({"requestId": "r-1", "method": method, "params": {"blockId": "sb-1"}});
        for token in [send_token.token.as_str(), read_token.token.as_str()] {
            let (status, value) = exchange("POST", path, Some(token), &body);
            assert_eq!((status, code(&value)), (403, "owner_required"), "{method}");
        }
        // No root credential (what an agent or a bridge caller has): refused
        // before any state is read.
        for bearer in [None, Some("agent-bus-token-is-not-a-management-credential")] {
            let (status, value) = exchange("POST", path, bearer, &body);
            assert_eq!((status, code(&value)), (403, "cutex_method_denied"), "{method} {bearer:?}");
        }
    }
    for token in [send_token.token.as_str(), read_token.token.as_str()] {
        let (status, value) = exchange("GET", "/v2/safety-checks", Some(token), &Value::Null);
        assert_eq!((status, code(&value)), (403, "owner_required"));
    }
    for bearer in [None, Some("agent-bus-token-is-not-a-management-credential")] {
        let (status, _) = exchange("GET", "/v2/safety-checks", bearer, &Value::Null);
        assert_eq!(status, 401, "{bearer:?}");
    }
    let (status, value) = exchange("GET", "/v2/safety-checks?state=all", Some(ROOT), &Value::Null);
    assert_eq!((status, &value["blocks"]), (200, &json!([])), "{value}");
}

/// host-owner-person: every principal sees `/v2/host` `owner`; only
/// owner-local requests (root or a CLI-issued owner token, on loopback) may
/// set or clear the owner person.
#[test]
fn host_owner_is_visible_to_everyone_and_set_only_locally() {
    use base64::Engine;
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let tokens = ClientTokenStore::open_default().unwrap();
    let owner_device = tokens.issue("owner-desktop").unwrap();
    let users = UserStore::open_default().unwrap();
    users.snapshot().unwrap();
    let pending = tokens.issue("ssh-device").unwrap();
    let member = users.create_user("Member", None, Grant::default(), "root").unwrap();
    let device = tokens.issue("member-phone").unwrap();
    users.bind_token(&device.token_id, Some(&member.id), "root").unwrap();
    let key = |seed: u8| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([seed; 32]);
    for seed in [1, 2] {
        let claim = crate::management::persons::VerifiedClaim { token_id: None, public_key: key(seed), display_name: None };
        users.apply_claim(&claim, "root").unwrap();
    }
    std::thread::sleep(Duration::from_millis(30));
    let fingerprint = crate::management::persons::fingerprint(&key(1));

    for bearer in [ROOT, owner_device.token.as_str(), device.token.as_str(), pending.token.as_str()] {
        let (status, host) = exchange("GET", "/v2/host", Some(bearer), &json!({}));
        assert_eq!(status, 200, "{host}");
        assert_eq!(
            host["owner"],
            json!({"displayName": "Owner", "fingerprint": fingerprint, "confirmed": true, "source": "firstRootClaim"}),
            "{host}"
        );
        crate::management::v2::schema_export::assert_valid("host", "host", &host);
    }
    let caps = crate::management::v2::capabilities::capabilities_resource();
    assert!(caps["featureList"].as_array().unwrap().iter().any(|f| f == "host-owner-person"));
    assert_eq!(caps["features"]["hostOwnerPerson"], true);

    // A paired device's token is refused, even owner-bound on loopback; so
    // are member and pending tokens. (The SSH-tunnelled desktop's
    // CLI-issued owner token may: below.)
    let (paired, _) = tokens.issue_device("owner-phone", "Pixel", "android").unwrap();
    users.bind_token(&paired.token_id, Some(OWNER), "pairing-code").unwrap();
    std::thread::sleep(Duration::from_millis(30));
    let path = "/v2/users/owner-person";
    let (status, value) = exchange("PUT", path, Some(&paired.token), &json!({"publicKey": key(2)}));
    assert_eq!((status, code(&value)), (403, "owner_person_local_only"), "{value}");
    assert_eq!(value["error"]["details"], json!({"clientToken": true, "issuedVia": "pairing"}));
    let (status, value) = exchange("DELETE", path, Some(&paired.token), &json!({}));
    assert_eq!((status, code(&value)), (403, "owner_person_local_only"), "{value}");
    for bearer in [device.token.as_str(), pending.token.as_str()] {
        let (status, _) = exchange("PUT", path, Some(bearer), &json!({"publicKey": key(2)}));
        assert_eq!(status, 403);
    }
    // A remote listener: refused (root is not even accepted there).
    REMOTE.with(|remote| remote.set(true));
    let (status, value) = exchange("PUT", path, Some(&owner_device.token), &json!({"publicKey": key(2)}));
    assert_eq!((status, code(&value)), (403, "owner_person_local_only"), "{value}");
    let (status, _) = exchange("PUT", path, Some(ROOT), &json!({"publicKey": key(2)}));
    assert_eq!(status, 401);
    let (status, host) = exchange("GET", "/v2/host", Some(&device.token), &json!({}));
    assert_eq!((status, &host["owner"]["fingerprint"]), (200, &json!(fingerprint)));
    REMOTE.with(|remote| remote.set(false));

    // Root on loopback: set, validation, clear.
    let (status, value) = exchange("PUT", path, Some(ROOT), &json!({"publicKey": key(9)}));
    assert_eq!((status, code(&value)), (400, "not_an_owner_key"), "{value}");
    let (status, value) = exchange("PUT", path, Some(ROOT), &json!({"publicKey": key(2), "x": 1}));
    assert_eq!((status, code(&value)), (400, "invalid_request"), "{value}");
    let (status, value) = exchange("PUT", path, Some(ROOT), &json!({"publicKey": key(2)}));
    assert_eq!(status, 200, "{value}");
    let second = crate::management::persons::fingerprint(&key(2));
    assert_eq!(value["owner"], json!({"displayName": "Owner", "fingerprint": second, "confirmed": true, "source": "local"}));
    let (_, host) = exchange("GET", "/v2/host", Some(&pending.token), &json!({}));
    assert_eq!(host["owner"], value["owner"]);
    let (status, value) = exchange("DELETE", path, Some(ROOT), &json!({}));
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["owner"], json!({"displayName": "Owner", "fingerprint": null, "confirmed": false, "source": null}));
    let (status, value) = exchange("GET", path, Some(ROOT), &json!({}));
    assert_eq!((status, code(&value)), (405, "method_not_allowed"));
    // "本机和 SSH 都算": the owner token issued by `client-token issue` on the
    // host (an SSH-tunnelled desktop) on loopback sets and clears it too.
    let (status, value) = exchange("PUT", path, Some(&owner_device.token), &json!({"publicKey": key(1)}));
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["owner"]["fingerprint"], json!(fingerprint));
    let (status, value) = exchange("DELETE", path, Some(&owner_device.token), &json!({}));
    assert_eq!((status, &value["owner"]["confirmed"]), (200, &json!(false)), "{value}");
    let audit = std::fs::read_to_string(users.dir().join("audit.jsonl")).unwrap();
    assert!(audit.contains("\"ownerPersonSet\""));
}

/// The latest host events named `method` since `checkpoint`.
fn host_events(checkpoint: &crate::management::v2::model::EventCheckpoint, method: &str) -> Vec<Value> {
    let repository = crate::management::v2::repository::management_v2_repository().unwrap();
    let page = repository
        .page(crate::management::v2::repository::ReplayQuery {
            stream_id: Some(checkpoint.stream_id.clone()),
            after: checkpoint.cursor.clone(),
            limit: 1000,
            cutex_session_id: Some(crate::management::v2::services::HOST_EVENT_SESSION_ID.into()),
        })
        .unwrap();
    page.events
        .iter()
        .filter_map(|event| event.cutex.as_ref())
        .filter(|message| message.method == method)
        .map(|message| message.params.clone())
        .collect()
}

/// A v2 error code, or the peers routes' top-level `code`.
fn any_code(value: &Value) -> &str {
    value.pointer("/error/code").or_else(|| value.get("code")).and_then(Value::as_str).unwrap_or("")
}

/// Owner's paired devices, limit 1 (owner-device-notice): owner pairing
/// codes are short-lived and single use; every new owner device (pairing,
/// assignment, person-key auto-binding, CLI) is listed until acknowledged
/// and announced on the host event stream.
#[test]
fn owner_device_notices_for_pairing_assignment_person_key_and_cli() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let tokens = ClientTokenStore::open_default().unwrap();
    let users = UserStore::open_default().unwrap();
    let first = tokens.issue("local-desktop").unwrap();
    users.snapshot().unwrap(); // migrated: the first token is the owner's, no notice
    let member = users.create_user("Member", None, Grant::default(), "root").unwrap();
    let member_token = tokens.issue("member-laptop").unwrap();
    users.bind_token(&member_token.token_id, Some(&member.id), "root").unwrap();
    let repository = crate::management::v2::repository::management_v2_repository().unwrap();
    let checkpoint = repository.checkpoint().unwrap();

    // Codes: owner 5 minutes and `ownerDevice`, a member's 10; scope checked.
    let (status, owner_code) = exchange("POST", "/v2/pairing-codes", Some(ROOT), &json!({"label": "Phone", "scope": "chat"}));
    assert_eq!(status, 201, "{owner_code}");
    assert_eq!((&owner_code["ttlSeconds"], &owner_code["ownerDevice"], &owner_code["scope"]), (&json!(300), &json!(true), &json!("chat")));
    crate::management::v2::schema_export::assert_valid("pair", "pairingCode", &owner_code);
    let (status, member_code) = exchange("POST", "/v2/pairing-codes", Some(ROOT), &json!({"userId": member.id}));
    assert_eq!(status, 201, "{member_code}");
    assert_eq!((&member_code["ttlSeconds"], &member_code["ownerDevice"], &member_code["scope"]), (&json!(600), &json!(false), &json!("full")));
    let (status, value) = exchange("POST", "/v2/pairing-codes", Some(ROOT), &json!({"scope": "admin"}));
    assert_eq!((status, code(&value)), (400, "invalid_scope"));
    // Loopback only (already): a remote listener never creates codes.
    REMOTE.with(|remote| remote.set(true));
    let (status, value) = exchange("POST", "/v2/pairing-codes", Some(&first.token), &json!({}));
    assert_eq!((status, code(&value)), (403, "loopback_only"), "{value}");
    REMOTE.with(|remote| remote.set(false));

    // Pairing with the owner code: an owner device with the code's scope, announced at once.
    let (status, paired) = exchange("POST", "/v2/pair", None, &json!({"code": owner_code["code"], "deviceName": "Pixel", "platform": "android"}));
    assert_eq!(status, 200, "{paired}");
    assert_eq!((&paired["userId"], &paired["device"]["scope"]), (&json!(OWNER), &json!("chat")));
    let phone = paired["device"]["tokenId"].as_str().unwrap().to_string();
    let phone_token = paired["token"].as_str().unwrap().to_string();
    let (status, value) = exchange("POST", "/v2/pair", None, &json!({"code": owner_code["code"]}));
    assert_eq!((status, code(&value)), (401, "invalid_pairing_code"), "single use");
    let (status, _) = exchange("POST", "/v2/pair", None, &json!({"code": member_code["code"], "deviceName": "Tab"}));
    assert_eq!(status, 200);
    let added = host_events(&checkpoint, crate::management::users::OWNER_DEVICE_ADDED);
    assert_eq!(added.len(), 1, "{added:?}");
    assert_eq!((&added[0]["tokenId"], &added[0]["via"], &added[0]["scope"], &added[0]["deviceName"]), (&json!(phone), &json!("pairing"), &json!("chat"), &json!("Pixel")));

    // Assignment of a pending device by an owner principal.
    let pending = tokens.issue("ssh-desktop").unwrap();
    let (status, _) = exchange("PUT", &format!("/v2/users/token-bindings/{}", pending.token_id), Some(&first.token), &json!({"userId": "owner"}));
    assert_eq!(status, 200);
    // A pending device that claims a person key the owner holds.
    let key = base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, [7u8; 32]);
    users.apply_claim(&crate::management::persons::VerifiedClaim { token_id: None, public_key: key.clone(), display_name: None }, "root").unwrap();
    let laptop = tokens.issue("laptop").unwrap();
    let claimed = users.apply_claim(&crate::management::persons::VerifiedClaim { token_id: Some(laptop.token_id.clone()), public_key: key, display_name: None }, "laptop").unwrap();
    assert_eq!(claimed.outcome, crate::management::persons::ClaimOutcome::AutoBound);
    // The CLI on the host (`client-token issue --user owner`, `users assign`).
    let cli = tokens.issue("desktop-over-ssh").unwrap();
    users.bind_token(&cli.token_id, Some(OWNER), "cli").unwrap();
    assert!(users.snapshot().unwrap().new_owner_devices.iter().any(|n| n.token_id == cli.token_id && !n.announced));
    std::thread::sleep(Duration::from_millis(30));
    // The daemon's next request announces what the CLI (or the store API) added.
    let (status, _) = exchange("GET", "/v2/capabilities", Some(ROOT), &json!({}));
    assert_eq!(status, 200);
    let added = host_events(&checkpoint, crate::management::users::OWNER_DEVICE_ADDED);
    let vias: Vec<(String, String)> = added.iter().map(|e| (e["tokenId"].as_str().unwrap().to_string(), e["via"].as_str().unwrap().to_string())).collect();
    for (token, via) in [(&phone, "pairing"), (&pending.token_id, "assigned"), (&laptop.token_id, "personKey"), (&cli.token_id, "ssh")] {
        assert_eq!(vias.iter().filter(|(t, v)| t == token && v == via).count(), 1, "{token} {via}: {vias:?}");
    }
    assert_eq!(vias.len(), 4, "each new owner device is announced once: {vias:?}");

    // The list: owner principals only (a narrowed owner device may read it).
    let path = "/v2/users/owner/new-devices";
    let (status, listed) = exchange("GET", path, Some(&phone_token), &json!({}));
    assert_eq!(status, 200, "{listed}");
    let devices = listed["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 4, "{listed}");
    let phone_item = devices.iter().find(|d| d["tokenId"] == phone.as_str()).unwrap();
    assert_eq!((&phone_item["via"], &phone_item["current"], &phone_item["scope"], &phone_item["userId"]), (&json!("pairing"), &json!(true), &json!("chat"), &json!(OWNER)));
    assert!(phone_item["at"].is_string() && phone_item["deviceName"] == "Pixel");
    assert!(!listed.to_string().contains("cxm1."), "no secrets");
    let (status, value) = exchange("GET", path, Some(&member_token.token), &json!({}));
    assert_eq!((status, code(&value)), (403, "owner_required"));
    // Ack: a full owner principal; the chat phone may not.
    let ack = |id: &str| format!("{path}/{id}/ack");
    let (status, value) = exchange("POST", &ack(&phone), Some(&phone_token), &json!({}));
    assert_eq!((status, code(&value)), (403, "device_scope_denied"), "{value}");
    let (status, value) = exchange("POST", &ack(&phone), Some(&first.token), &json!({}));
    assert_eq!((status, &value), (200, &json!({"tokenId": phone, "acknowledged": true})));
    let (status, value) = exchange("POST", &ack(&phone), Some(ROOT), &json!({}));
    assert_eq!((status, &value["acknowledged"]), (200, &json!(false)), "idempotent");
    let (status, value) = exchange("GET", &ack(&phone), Some(ROOT), &json!({}));
    assert_eq!((status, code(&value)), (405, "method_not_allowed"));
    // Revoking (or rebinding away from owner) drops the notice.
    tokens.revoke(&cli.token_id).unwrap();
    users.bind_token(&pending.token_id, Some(&member.id), "root").unwrap();
    std::thread::sleep(Duration::from_millis(30));
    let (_, listed) = exchange("GET", path, Some(ROOT), &json!({}));
    let ids: Vec<&str> = listed["devices"].as_array().unwrap().iter().map(|d| d["tokenId"].as_str().unwrap()).collect();
    assert_eq!(ids, vec![laptop.token_id.as_str()], "{listed}");
    let audit = std::fs::read_to_string(users.dir().join("audit.jsonl")).unwrap();
    assert!(audit.contains("\"ownerDeviceAcknowledged\""));
}

/// Rewrite one record of the private `tokens.json` (simulating a record of
/// an older build, or an unknown origin): `None` removes `issuedVia`.
fn set_issued_via(tokens: &ClientTokenStore, token_id: &str, via: Option<&str>) {
    let path = tokens.tokens_path();
    let mut raw: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for record in raw["tokens"].as_array_mut().unwrap() {
        if record["tokenId"] == token_id {
            match via {
                Some(via) => record["issuedVia"] = json!(via),
                None => {
                    record.as_object_mut().unwrap().remove("issuedVia");
                }
            }
        }
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&raw).unwrap()).unwrap();
}

/// Owner's paired devices, limit 2 (owner-local-only; "本机和 SSH 都算"):
/// root-identity actions need a loopback listener and either the root
/// credential or an owner token issued by `client-token issue` on the host
/// (the SSH-tunnelled desktop). A paired device's token never counts, even
/// owner-bound on loopback; tightening stays open to it.
#[test]
fn root_identity_actions_are_owner_local_only() {
    use crate::management::client_tokens::DeviceScope;
    let _home_lock = crate::config::paths::test_home_env_lock();
    let instance = PrivateInstance::new();
    let tokens = ClientTokenStore::open_default().unwrap();
    // The SSH-tunnelled desktop: `client-token issue` on the host, the owner's.
    let ssh = tokens.issue("desktop-over-ssh").unwrap();
    let users = UserStore::open_default().unwrap();
    users.snapshot().unwrap();
    // The owner's paired phone (an owner pairing code).
    let (paired, _) = tokens.issue_device("owner-phone", "Pixel", "android").unwrap();
    users.bind_token(&paired.token_id, Some(OWNER), "pairing-code").unwrap();
    // Tokens of an older build (no issuedVia): inferred from kind.
    let old_ssh = tokens.issue("old-ssh-desktop").unwrap();
    users.bind_token(&old_ssh.token_id, Some(OWNER), "cli").unwrap();
    let (old_paired, _) = tokens.issue_device("old-phone", "iPhone", "ios").unwrap();
    users.bind_token(&old_paired.token_id, Some(OWNER), "pairing-code").unwrap();
    // An origin this build does not know: never local.
    let unknown = tokens.issue("unknown-origin").unwrap();
    users.bind_token(&unknown.token_id, Some(OWNER), "cli").unwrap();
    // CLI-issued but not the owner's (pending; a member), or narrowed.
    let ssh_pending = tokens.issue("ssh-member-pending").unwrap();
    let member = users.create_user("Member", None, Grant::default(), "root").unwrap();
    let ssh_member = tokens.issue("ssh-member").unwrap();
    users.bind_token(&ssh_member.token_id, Some(&member.id), "cli").unwrap();
    let ssh_chat = tokens.issue_scoped("ssh-chat", DeviceScope::Chat).unwrap();
    users.bind_token(&ssh_chat.token_id, Some(OWNER), "cli").unwrap();
    set_issued_via(&tokens, &old_ssh.token_id, None);
    set_issued_via(&tokens, &old_paired.token_id, None);
    set_issued_via(&tokens, &unknown.token_id, Some("invite"));
    std::thread::sleep(Duration::from_millis(30));
    let via = |id: &str| tokens.list().unwrap().into_iter().find(|t| t.token_id == id).unwrap().issued_via;
    use crate::management::client_tokens::IssuedVia;
    assert_eq!(
        (via(&ssh.token_id), via(&paired.token_id), via(&old_ssh.token_id), via(&old_paired.token_id), via(&unknown.token_id)),
        (IssuedVia::HostCli, IssuedVia::Pairing, IssuedVia::HostCli, IssuedVia::Pairing, IssuedVia::Unknown)
    );

    // /v2/capabilities says whether the connection counts (user.ownerLocal).
    let caps = |bearer: &str| exchange("GET", "/v2/capabilities", Some(bearer), &json!({})).1;
    for (bearer, local) in [
        (ROOT, true),
        (ssh.token.as_str(), true),
        (old_ssh.token.as_str(), true),
        (ssh_chat.token.as_str(), true),
        (paired.token.as_str(), false),
        (old_paired.token.as_str(), false),
        (unknown.token.as_str(), false),
        (ssh_pending.token.as_str(), false),
        (ssh_member.token.as_str(), false),
    ] {
        let value = caps(bearer);
        assert_eq!(value["user"]["ownerLocal"], json!(local), "{bearer}: {}", value["user"]);
        crate::management::v2::schema_export::assert_valid("users", "capabilityUser", &value["user"]);
    }
    let value = caps(&ssh.token);
    assert_eq!((&value["user"]["token"]["issuedVia"], &value["features"]["ownerLocalOnly"]), (&json!("hostCli"), &json!(true)));
    assert_eq!(caps(&paired.token)["user"]["token"]["issuedVia"], "pairing");
    REMOTE.with(|remote| remote.set(true));
    assert_eq!(caps(&ssh.token)["user"]["ownerLocal"], false, "a remote listener never counts");
    REMOTE.with(|remote| remote.set(false));

    let local_only = |value: &Value, action: &str| {
        // r290 P10 (B6): a full-scope owner device is asked to confirm (428).
        assert_eq!(any_code(value), "owner_confirm_required", "{value}");
        let details = value.pointer("/error/details").or_else(|| value.get("details")).unwrap();
        assert_eq!(details["action"], action, "{value}");
    };
    let refused = [paired.token.as_str(), old_paired.token.as_str(), unknown.token.as_str()];
    let allowed = [ROOT, ssh.token.as_str(), old_ssh.token.as_str()];

    // Approval: owner devices may turn it on; off only owner-local.
    let settings = "/v2/users/settings";
    for bearer in refused {
        let (status, value) = exchange("PATCH", settings, Some(bearer), &json!({"ownerRequiresApproval": false}));
        assert_eq!(status, 428, "{value}"); // r290 P10 (B6)
        local_only(&value, "disableOwnerApproval");
    }
    let (_, value) = exchange("PATCH", settings, Some(&paired.token), &json!({"ownerRequiresApproval": false}));
    // r290 P10 (B6): the 428 details also carry summary, confirmToken, expiresAt, ttlSeconds.
    for (key, expected) in [("action", json!("disableOwnerApproval")), ("clientToken", json!(true)), ("issuedVia", json!("pairing"))] {
        assert_eq!(value["error"]["details"][key], expected, "{value}");
    }
    let (status, value) = exchange("PATCH", settings, Some(&paired.token), &json!({"ownerRequiresApproval": true}));
    assert_eq!(status, 200, "{value}");
    for bearer in allowed {
        let (status, value) = exchange("PATCH", settings, Some(bearer), &json!({"ownerRequiresApproval": false}));
        assert_eq!((status, &value["settings"]["ownerRequiresApproval"]), (200, &json!(false)), "{value}");
        exchange("PATCH", settings, Some(ROOT), &json!({"ownerRequiresApproval": true}));
    }
    // A narrowed SSH token: the scope gate refuses first.
    let (status, value) = exchange("PATCH", settings, Some(&ssh_chat.token), &json!({"ownerRequiresApproval": false}));
    assert_eq!((status, code(&value)), (403, "device_scope_denied"), "{value}");
    // A member's CLI token: the user gate refuses first.
    let (status, value) = exchange("PATCH", settings, Some(&ssh_member.token), &json!({"ownerRequiresApproval": false}));
    assert_eq!((status, code(&value)), (403, "owner_required"), "{value}");
    REMOTE.with(|remote| remote.set(true));
    let (status, value) = exchange("PATCH", settings, Some(&ssh.token), &json!({"ownerRequiresApproval": false}));
    assert_eq!(status, 428); // r290 P10 (B6)
    local_only(&value, "disableOwnerApproval");
    REMOTE.with(|remote| remote.set(false));

    // Peers: pairing and enabling.
    for (method, path, body, action) in [
        ("POST", "/v2/peers/invites", json!({}), "peerPair"),
        ("POST", "/v2/peers", json!({"invite": "x"}), "peerPair"),
        ("PATCH", "/v2/peers/peer-1", json!({"enabled": true}), "peerEnable"),
        ("PATCH", "/v2/settings", json!({"peers": {"enabled": true}}), "peersEnable"),
    ] {
        for bearer in refused {
            let (status, value) = exchange(method, path, Some(bearer), &body);
            assert_eq!(status, 428, "{method} {path}: {value}"); // r290 P10 (B6)
            local_only(&value, action);
        }
        for bearer in allowed {
            let (_, value) = exchange(method, path, Some(bearer), &body);
            assert_ne!(any_code(&value), "owner_local_only", "{method} {path} {bearer}: {value}");
        }
    }
    // Pre-existing loopback-only writes stay open to every owner device on
    // loopback (disabling a peer: refused later only because it does not exist).
    for bearer in [paired.token.as_str(), ssh.token.as_str()] {
        let (_, value) = exchange("PATCH", "/v2/peers/peer-1", Some(bearer), &json!({"enabled": false}));
        assert_ne!(any_code(&value), "owner_local_only", "{value}");
        let (status, value) = exchange("PATCH", "/v2/settings", Some(bearer), &json!({"peers": {"enabled": false}}));
        assert_eq!(status, 200, "{value}");
    }
    let (status, value) = exchange("PATCH", "/v2/settings", Some(&ssh.token), &json!({"managementCorsOrigins": ["http://localhost:5173"]}));
    assert_eq!(status, 200, "SSH keeps loopback-only settings: {value}");
    REMOTE.with(|remote| remote.set(true));
    let (status, value) = exchange("PATCH", "/v2/settings", Some(&ssh.token), &json!({"managementCorsOrigins": []}));
    assert_eq!((status, code(&value)), (403, "loopback_only"), "{value}");
    REMOTE.with(|remote| remote.set(false));

    // The root credential file through the host file read.
    let config = crate::config::paths::config_path().unwrap();
    if !config.exists() {
        std::fs::write(&config, b"{}").unwrap();
    }
    let read = format!("/v2/fs/file?path={}", config.display());
    for bearer in refused {
        let (status, value) = exchange("GET", &read, Some(bearer), &json!({}));
        assert_eq!((status, code(&value)), (403, "owner_local_only"), "{value}");
    }
    for bearer in allowed {
        let (status, value) = exchange("GET", &read, Some(bearer), &json!({}));
        assert_eq!(status, 200, "{value}");
    }
    std::fs::write(instance.root.join("notes.txt"), b"ok").unwrap();
    let (status, _) = exchange("GET", &format!("/v2/fs/file?path={}", instance.root.join("notes.txt").display()), Some(&paired.token), &json!({}));
    assert_eq!(status, 200, "other files stay readable");
    // The workspace file read (a workspace rooted at home) uses the same check.
    let refuse = crate::management::v2::host_files::refuse_root_credential_file;
    let mut token = crate::management::client_tokens::VerifiedClientToken::test_token(&paired.token_id);
    token.issued_via = IssuedVia::Pairing;
    listeners::set_current_client_token(Some(token.clone()));
    assert_eq!(refuse(&config).unwrap_err().code, "owner_local_only");
    assert!(refuse(&instance.root.join("notes.txt")).is_ok());
    token.issued_via = IssuedVia::HostCli;
    listeners::set_current_client_token(Some(token));
    assert!(refuse(&config).is_ok());
    listeners::set_current_client_token(None);

    // Pairing codes: owner-local (the SSH desktop keeps creating them).
    for bearer in allowed {
        let (status, value) = exchange("POST", "/v2/pairing-codes", Some(bearer), &json!({}));
        assert_eq!(status, 201, "{value}");
    }
    for bearer in refused {
        let (status, value) = exchange("POST", "/v2/pairing-codes", Some(bearer), &json!({}));
        assert_eq!(status, 428, "{value}"); // r290 P10 (B6)
        local_only(&value, "pairingCode");
    }
    REMOTE.with(|remote| remote.set(true));
    let (status, value) = exchange("POST", "/v2/pairing-codes", Some(&ssh.token), &json!({}));
    assert_eq!((status, code(&value)), (403, "loopback_only"), "{value}");
    REMOTE.with(|remote| remote.set(false));
}

/// Owner's paired devices, limit 3 (device-scope): effective rights = the
/// user's rights ∩ the device scope, for owner and member tokens alike.
#[test]
fn device_scope_narrows_owner_and_member_tokens() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let host = crate::platform::host::current_host_name();
    let mut store = crate::session::model::CutexSessionStore::default();
    let mut record = crate::session::model::CutexSessionRecord::new(
        "cutex.scope-solo".to_string(), None, host, "/tmp".to_string(), None,
    )
    .unwrap();
    record.exposed_to_backend = true;
    record.registration_class = crate::agent_bus::model::AgentRegistrationClass::Persistent;
    store.sessions.insert("cutex.scope-solo".to_string(), record);
    crate::session::store::save_cutex_session_store(&store).unwrap();
    use crate::management::client_tokens::DeviceScope;
    let tokens = ClientTokenStore::open_default().unwrap();
    let users = UserStore::open_default().unwrap();
    let full = tokens.issue("owner-full").unwrap();
    users.snapshot().unwrap();
    let read = tokens.issue_scoped("owner-read", DeviceScope::Read).unwrap();
    let chat = tokens.issue_scoped("owner-chat", DeviceScope::Chat).unwrap();
    users.bind_token(&read.token_id, Some(OWNER), "root").unwrap();
    users.bind_token(&chat.token_id, Some(OWNER), "root").unwrap();
    let sender = users
        .create_user("Sender", None, Grant { standalone_agents: vec!["cutex.scope-solo".into()], send: true, ..Grant::default() }, "root")
        .unwrap();
    let member_read = tokens.issue_scoped("member-read", DeviceScope::Read).unwrap();
    users.bind_token(&member_read.token_id, Some(&sender.id), "root").unwrap();
    std::thread::sleep(Duration::from_millis(30));

    // Capabilities report the scope.
    let caps = |bearer: &str| exchange("GET", "/v2/capabilities", Some(bearer), &json!({})).1;
    assert_eq!(caps(ROOT)["user"]["scope"], "full");
    assert_eq!(caps(&full.token)["user"]["scope"], "full");
    let read_caps = caps(&read.token);
    assert_eq!((&read_caps["user"]["scope"], &read_caps["user"]["token"]["scope"], &read_caps["user"]["isOwner"]), (&json!("read"), &json!("read"), &json!(true)));
    assert_eq!(read_caps["features"]["deviceScope"], true);
    assert_eq!(caps(&member_read.token)["user"]["scope"], "read");

    let denied = |bearer: &str, method: &str, path: &str, body: &Value, required: &str| {
        let (status, value) = p2_exchange(method, path, bearer, body);
        assert_eq!((status, code(&value)), (403, "device_scope_denied"), "{method} {path} {body}: {value}");
        assert_eq!(value["error"]["details"]["required"], required, "{value}");
    };
    let passes = |bearer: &str, method: &str, path: &str, body: &Value| {
        let (status, value) = p2_exchange(method, path, bearer, body);
        assert_ne!(code(&value), "device_scope_denied", "{method} {path} {body}: {status} {value}");
        (status, value)
    };
    let cutex = |method: &str, params: Value| json!({"requestId": "r-1", "method": method, "params": params});
    let text = json!({"clientUserMessageId": "m", "origin": {"kind": "backend", "clientId": "c"}, "strategy": "queue", "input": [{"type": "text", "text": "hi"}]});
    let requests = "/v2/sessions/cutex.scope-solo/cutex/requests";
    let native = |method: &str| json!({"requestId": "r", "native": {"message": {"id": 1, "method": method, "params": {"threadId": "t"}}}});
    let native_path = "/v2/sessions/cutex.scope-solo/app-server/requests";

    // read: GETs pass (owner routes included), writes and talking do not.
    for path in ["/v2/users", "/v2/client-tokens", "/v2/users/owner/new-devices", "/v2/host"] {
        let (status, value) = passes(&read.token, "GET", path, &json!({}));
        assert_eq!(status, 200, "{path}: {value}");
    }
    denied(&read.token, "PATCH", "/v2/settings", &json!({"attachments": {"retentionDays": 3}}), "full");
    denied(&read.token, "POST", "/v2/users", &json!({"name": "x"}), "full");
    denied(&read.token, "PATCH", &format!("/v2/client-tokens/{}", read.token_id), &json!({"scope": "full"}), "full");
    denied(&read.token, "POST", "/v2/pairing-codes", &json!({}), "full");
    denied(&read.token, "POST", requests, &cutex("cutex/userInput/submit", text.clone()), "chat");
    denied(&read.token, "POST", native_path, &native("turn/interrupt"), "chat");
    denied(&read.token, "POST", "/v2/sessions/cutex.scope-solo/attachments", &json!({}), "chat");
    // Its own identity claim and signing itself out stay possible.
    let (status, _) = passes(&read.token, "GET", "/v2/identity/challenge", &json!({}));
    assert_eq!(status, 200);
    denied(&read.token, "DELETE", &format!("/v2/users/me/devices/{}", chat.token_id), &json!({}), "full");

    // chat: talking to agents, not approvals, settings or agent operations.
    let (status, value) = passes(&chat.token, "POST", requests, &cutex("cutex/userInput/submit", text.clone()));
    assert!(matches!(code(&value), "session_offline" | "test_user_input_reached"), "{status} {value}");
    let image = json!({"clientUserMessageId": "m", "origin": {"kind": "backend", "clientId": "c"}, "strategy": "queue", "input": [{"type": "localImage", "path": "/etc/hostname"}]});
    let (status, value) = p2_exchange("POST", requests, &chat.token, &cutex("cutex/userInput/submit", image));
    assert_eq!((status, code(&value)), (403, "attachment_not_owned"), "{value}");
    for method in ["cutex/session/retire", "cutex/session/delete", "cutex/ask/create", "cutex/ask/close", "cutex/runtime/online"] {
        denied(&chat.token, "POST", requests, &cutex(method, json!({})), "full");
    }
    for method in ["turn/start", "turn/steer", "thread/settings/update"] {
        denied(&chat.token, "POST", native_path, &native(method), "full");
    }
    denied(&chat.token, "PATCH", "/v2/settings", &json!({"attachments": {"retentionDays": 3}}), "full");
    denied(&chat.token, "POST", "/v2/human/agents", &json!({}), "full");
    denied(&chat.token, "POST", "/v2/terminals", &json!({}), "full");
    denied(&chat.token, "PATCH", "/v2/users/settings", &json!({"ownerRequiresApproval": true}), "full");
    // Server responses: questions yes, approvals no.
    crate::management::v2::server_requests::record_request("cutex.scope-solo", 1, json!({"id": 41, "method": "item/tool/requestUserInput", "params": {"questions": []}})).unwrap();
    crate::management::v2::server_requests::record_request("cutex.scope-solo", 1, json!({"id": 42, "method": "item/commandExecution/requestApproval", "params": {}})).unwrap();
    let respond = |id: i64| json!({"responseId": format!("resp-{id}"), "native": {"message": {"id": id, "result": {}}}});
    let responses = "/v2/sessions/cutex.scope-solo/app-server/server-request-responses";
    let (status, value) = passes(&chat.token, "POST", responses, &respond(41));
    assert_ne!(status, 403, "{value}");
    denied(&chat.token, "POST", responses, &respond(42), "full");
    denied(&chat.token, "POST", responses, &respond(99), "full");
    denied(&read.token, "POST", responses, &respond(41), "chat");

    // Members: the user's rights ∩ the scope (send granted, device read-only).
    denied(&member_read.token, "POST", requests, &cutex("cutex/userInput/submit", text.clone()), "chat");
    let (status, value) = passes(&member_read.token, "GET", "/v2/sessions/cutex.scope-solo", &json!({}));
    assert_ne!(status, 403, "{value}");
    // ... and the scope never widens the user: a full member token is still refused owner routes.
    let member_full = tokens.issue("member-full").unwrap();
    users.bind_token(&member_full.token_id, Some(&sender.id), "root").unwrap();
    std::thread::sleep(Duration::from_millis(30));
    let (status, value) = p2_exchange("GET", "/v2/users", &member_full.token, &json!({}));
    assert_eq!((status, code(&value)), (403, "owner_required"));

    // An owner principal changes the scope (PATCH); a narrowed device cannot.
    let patch = |bearer: &str, id: &str, body: Value| exchange("PATCH", &format!("/v2/client-tokens/{id}"), Some(bearer), &body);
    let (status, value) = patch(&full.token, &read.token_id, json!({"scope": "chat"}));
    assert_eq!((status, &value), (200, &json!({"tokenId": read.token_id, "ownerName": null, "scope": "chat"})), "{value}");
    crate::management::v2::schema_export::assert_valid("pair", "clientTokenRenamed", &value);
    let (status, value) = patch(ROOT, &read.token_id, json!({"scope": "admin"}));
    assert_eq!((status, code(&value)), (400, "invalid_scope"));
    let (status, value) = patch(ROOT, &read.token_id, json!({"name": "Phone", "scope": "read"}));
    assert_eq!((status, &value["ownerName"], &value["scope"]), (200, &json!("Phone"), &json!("read")));
    let (status, value) = patch(ROOT, "mct-0000000000000000", json!({"scope": "read"}));
    assert_eq!((status, code(&value)), (404, "client_token_not_found"));
    let (_, listed) = exchange("GET", "/v2/client-tokens", Some(ROOT), &json!({}));
    let scope_of = |id: &str| listed["tokens"].as_array().unwrap().iter().find(|t| t["tokenId"] == id).unwrap()["scope"].clone();
    assert_eq!((scope_of(&read.token_id), scope_of(&full.token_id), scope_of(&chat.token_id)), (json!("read"), json!("full"), json!("chat")));
    let (_, listing) = exchange("GET", "/v2/users", Some(ROOT), &json!({}));
    assert!(listing["tokens"].as_array().unwrap().iter().any(|t| t["tokenId"] == chat.token_id.as_str() && t["scope"] == "chat"));
    // Widened to full: the device may act again.
    patch(ROOT, &chat.token_id, json!({"scope": "full"}));
    std::thread::sleep(Duration::from_millis(30));
    let (status, value) = p2_exchange("POST", "/v2/users", &chat.token, &json!({"name": "Now allowed"}));
    assert_eq!(status, 201, "{value}");
    let audit = std::fs::read_to_string(users.dir().join("audit.jsonl")).unwrap();
    assert!(audit.contains("\"tokenScopeChanged\""));
}

/// Remote close of a desktop client (client-remote-close; cutex-desktop
/// docs/research/remote-close-desktop-20261019.md) through the production
/// dispatcher: presence for every principal (pending and narrowed included),
/// list visibility, the close matrix, events, outcomes and expiry.
#[test]
fn desktop_presence_and_remote_close() {
    use crate::management::client_tokens::DeviceScope;
    use crate::management::v2::clients_api::{self, CLOCK_OFFSET};
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    clients_api::with_registry(|registry| registry.clear());
    CLOCK_OFFSET.with(|offset| offset.set(0));
    let tokens = ClientTokenStore::open_default().unwrap();
    let users = UserStore::open_default().unwrap();
    let owner_device = tokens.issue("owner-laptop").unwrap();
    users.snapshot().unwrap(); // migrated: the first token is the owner's
    let owner_chat = tokens.issue_scoped("owner-phone", DeviceScope::Chat).unwrap();
    users.bind_token(&owner_chat.token_id, Some(OWNER), "root").unwrap();
    let alice = users.create_user("Alice", None, Grant::default(), "root").unwrap();
    let alice_desktop = tokens.issue("alice-desktop").unwrap();
    let alice_phone = tokens.issue_scoped("alice-phone", DeviceScope::Read).unwrap();
    users.bind_token(&alice_desktop.token_id, Some(&alice.id), "root").unwrap();
    users.bind_token(&alice_phone.token_id, Some(&alice.id), "root").unwrap();
    let bob = users.create_user("Bob", None, Grant::default(), "root").unwrap();
    let bob_desktop = tokens.issue("bob-desktop").unwrap();
    users.bind_token(&bob_desktop.token_id, Some(&bob.id), "root").unwrap();
    let pending = tokens.issue("pending-desktop").unwrap();
    std::thread::sleep(Duration::from_millis(30));
    let repository = crate::management::v2::repository::management_v2_repository().unwrap();
    let checkpoint = repository.checkpoint().unwrap();

    let present = |bearer: &str, instance: &str, device: &str, allow: bool| {
        exchange("POST", "/v2/clients/presence", Some(bearer), &json!({
            "installId": format!("install-{instance}"), "instanceId": instance, "appName": "cutex-desktop",
            "appVersion": "0.9.0", "deviceName": device, "platform": "linux", "allowRemoteClose": allow,
        }))
    };
    // Every principal reports presence: root, narrowed owner, user, read-only, pending.
    for (bearer, instance, device, allow) in [
        (ROOT, "i-host", "Host PC", true),
        (owner_chat.token.as_str(), "i-owner-phone", "Owner phone", true),
        (alice_desktop.token.as_str(), "i-alice", "Alice PC", true),
        (alice_phone.token.as_str(), "i-alice-phone", "Alice tablet", true),
        (bob_desktop.token.as_str(), "i-bob", "Bob PC", false),
        (pending.token.as_str(), "i-pending", "Guest PC", true),
    ] {
        let (status, value) = present(bearer, instance, device, allow);
        assert_eq!(status, 200, "{instance}: {value}");
        assert_eq!(value, json!({"instanceId": instance, "heartbeatSeconds": 60, "state": "online", "closeRequest": null}));
        crate::management::v2::schema_export::assert_valid("clients", "presenceResponse", &value);
    }
    let (status, value) = present(&bob_desktop.token, "i-alice", "Bob PC", true);
    assert_eq!((status, code(&value)), (409, "client_instance_conflict"), "{value}");
    let (status, value) = exchange("POST", "/v2/clients/presence", Some(ROOT), &json!({"instanceId": "x"}));
    assert_eq!((status, code(&value)), (400, "invalid_request"));
    let (status, _) = exchange("POST", "/v2/clients/presence", None, &json!({}));
    assert_eq!(status, 401);

    // List visibility.
    let list = |bearer: &str| {
        let (status, value) = exchange("GET", "/v2/clients", Some(bearer), &json!({}));
        assert_eq!(status, 200, "{value}");
        crate::management::v2::schema_export::assert_valid("clients", "clientList", &value);
        let mut ids: Vec<String> = value["clients"].as_array().unwrap().iter().map(|c| c["instanceId"].as_str().unwrap().to_string()).collect();
        ids.sort();
        (ids, value)
    };
    let (all, listed) = list(ROOT);
    assert_eq!(all, ["i-alice", "i-alice-phone", "i-bob", "i-host", "i-owner-phone", "i-pending"]);
    let host = listed["clients"].as_array().unwrap().iter().find(|c| c["instanceId"] == "i-host").unwrap();
    assert_eq!((&host["current"], &host["userId"], &host["tokenId"], &host["local"], &host["state"]), (&json!(true), &json!("owner"), &Value::Null, &json!(true), &json!("online")));
    assert_eq!(list(&owner_device.token).0.len(), 6, "an owner token with full scope sees all");
    assert_eq!(list(&owner_chat.token).0, ["i-host", "i-owner-phone"], "narrowed owner: the owner's own");
    assert_eq!(list(&alice_phone.token).0, ["i-alice", "i-alice-phone"]);
    assert_eq!(list(&bob_desktop.token).0, ["i-bob"]);
    assert_eq!(list(&pending.token).0, ["i-pending"], "pending: its own only");

    // Close matrix over HTTP.
    let close = |bearer: &str, instance: &str, body: Value| exchange("POST", &format!("/v2/clients/{instance}/close"), Some(bearer), &body);
    let refused = |bearer: &str, instance: &str, reason: &str| {
        let (status, value) = close(bearer, instance, json!({}));
        assert_eq!((status, code(&value), &value["error"]["details"]["reason"]), (403, "client_close_forbidden", &json!(reason)), "{instance}: {value}");
    };
    refused(&pending.token, "i-pending", "pending");
    refused(&owner_chat.token, "i-alice", "scope");
    refused(&alice_phone.token, "i-alice", "scope");
    refused(&bob_desktop.token, "i-alice", "otherUser");
    refused(&alice_desktop.token, "i-host", "otherUser");
    let (status, value) = close(ROOT, "i-unknown", json!({}));
    assert_eq!((status, code(&value)), (404, "client_not_found"));
    let (status, value) = close(ROOT, "i-bob", json!({}));
    assert_eq!((status, code(&value)), (409, "remote_close_disabled"));
    let (status, value) = close(ROOT, "i-alice", json!({"graceSeconds": 9999}));
    assert_eq!((status, code(&value)), (400, "invalid_request"));
    assert!(host_events(&checkpoint, clients_api::CLOSE_REQUESTED).is_empty(), "refusals publish nothing");
    // Same user from her full desktop; the owner (root) closes anything.
    let (status, value) = close(&alice_desktop.token, "i-alice-phone", json!({}));
    assert_eq!((status, &value), (202, &json!({"instanceId": "i-alice-phone", "state": "closeRequested"})));
    crate::management::v2::schema_export::assert_valid("clients", "closeAccepted", &value);
    let (status, _) = close(ROOT, "i-alice", json!({"graceSeconds": 30}));
    assert_eq!(status, 202);
    let (status, _) = close(&owner_device.token, "i-pending", json!({}));
    assert_eq!(status, 202);
    let requested = host_events(&checkpoint, clients_api::CLOSE_REQUESTED);
    assert_eq!(requested.len(), 3, "{requested:?}");
    assert_eq!(requested[0]["requestedBy"], json!({"deviceName": "Alice PC", "userId": alice.id, "local": true}));
    assert_eq!((&requested[0]["instanceId"], &requested[0]["graceSeconds"]), (&json!("i-alice-phone"), &json!(15)));
    assert_eq!((&requested[1]["requestedBy"]["deviceName"], &requested[1]["graceSeconds"]), (&json!("Host PC"), &json!(30)));
    // The owner token without a presence of its own: its token's name.
    assert_eq!(requested[2]["requestedBy"], json!({"deviceName": "owner-laptop", "userId": "owner", "local": true}));
    for event in &requested {
        crate::management::v2::schema_export::assert_valid("clients", "closeRequestedEvent", event);
    }
    let (_, listed) = list(ROOT);
    let state = |id: &str| listed["clients"].as_array().unwrap().iter().find(|c| c["instanceId"] == id).unwrap()["state"].clone();
    assert_eq!((state("i-alice"), state("i-host")), (json!("closeRequested"), json!("online")));
    // A pending device sees the request on its heartbeat (no event stream).
    let (_, beat) = present(&pending.token, "i-pending", "Guest PC", true);
    assert_eq!((&beat["state"], &beat["closeRequest"]["requestedBy"]["deviceName"]), (&json!("closeRequested"), &json!("owner-laptop")));
    crate::management::v2::schema_export::assert_valid("clients", "presenceResponse", &beat);

    // A member's event page carries its own user's client events only.
    // (After the checkpoint: the event log of the test process is shared.)
    let since = match &checkpoint.cursor {
        Some(cursor) => format!("streamId={}&after={cursor}", checkpoint.stream_id),
        None => format!("streamId={}", checkpoint.stream_id),
    };
    let (status, page) = p2_exchange("GET", &format!("/v2/events?cutexSessionId=cutex.host&limit=1000&{since}"), &alice_phone.token, &json!({}));
    assert_eq!(status, 200, "{page}");
    let seen: Vec<&str> = page["events"].as_array().unwrap().iter().map(|e| e["cutex"]["params"]["instanceId"].as_str().unwrap_or("")).collect();
    assert_eq!(seen, ["i-alice-phone", "i-alice"], "{page}");
    let (_, page) = p2_exchange("GET", &format!("/v2/events?limit=1000&{since}"), &bob_desktop.token, &json!({}));
    assert!(page["events"].as_array().unwrap().is_empty(), "{page}");

    // Outcomes: only the registering credential.
    let closing = |bearer: &str, instance: &str, outcome: &str| exchange("POST", &format!("/v2/clients/{instance}/closing"), Some(bearer), &json!({"outcome": outcome}));
    let (status, value) = closing(ROOT, "i-alice", "closing");
    assert_eq!((status, &value["error"]["details"]["reason"]), (403, &json!("notRegistrant")), "{value}");
    let (status, value) = closing(&alice_phone.token, "i-alice", "closing");
    assert_eq!((status, code(&value)), (403, "client_close_forbidden"), "same user, other device");
    let (status, value) = closing(&alice_desktop.token, "i-alice", "maybe");
    assert_eq!((status, code(&value)), (400, "invalid_request"));
    let (status, value) = closing(&alice_desktop.token, "i-unknown", "closing");
    assert_eq!((status, code(&value)), (404, "client_not_found"));
    let (status, value) = closing(&alice_phone.token, "i-alice-phone", "cancelled");
    assert_eq!((status, &value), (200, &json!({"instanceId": "i-alice-phone", "state": "cancelled"})));
    crate::management::v2::schema_export::assert_valid("clients", "closingResponse", &value);
    let (status, _) = closing(&alice_desktop.token, "i-alice", "closing");
    assert_eq!(status, 200);
    let changed = host_events(&checkpoint, clients_api::STATE_CHANGED);
    assert_eq!(changed, vec![json!({"instanceId": "i-alice-phone", "state": "cancelled"}), json!({"instanceId": "i-alice", "state": "closing"})]);
    for event in &changed {
        crate::management::v2::schema_export::assert_valid("clients", "stateChangedEvent", event);
    }
    let (_, listed) = list(ROOT);
    let state = |id: &str| listed["clients"].as_array().unwrap().iter().find(|c| c["instanceId"] == id).unwrap()["state"].clone();
    assert_eq!((state("i-alice"), state("i-alice-phone")), (json!("closing"), json!("cancelled")));
    let (status, value) = close(ROOT, "i-alice", json!({}));
    assert_eq!((status, &value["state"]), (202, &json!("closing")), "already closing: no new request");
    assert_eq!(host_events(&checkpoint, clients_api::CLOSE_REQUESTED).len(), 3);

    // Other methods on the family: the owner's / 405.
    let (status, value) = exchange("DELETE", "/v2/clients/i-host", Some(&alice_desktop.token), &json!({}));
    assert_eq!((status, code(&value)), (403, "owner_required"));
    let (status, value) = exchange("GET", "/v2/clients/i-host/close", Some(ROOT), &json!({}));
    assert_eq!((status, code(&value)), (405, "method_not_allowed"));

    // Expiry: 3 heartbeats (180 s) without presence.
    CLOCK_OFFSET.with(|offset| offset.set(120));
    let (status, _) = present(ROOT, "i-host", "Host PC", true);
    assert_eq!(status, 200);
    CLOCK_OFFSET.with(|offset| offset.set(clients_api::EXPIRE_SECONDS + 5));
    assert_eq!(list(ROOT).0, ["i-host"], "only the instance with a recent heartbeat remains");
    let (status, value) = close(ROOT, "i-alice", json!({}));
    assert_eq!((status, code(&value)), (404, "client_not_found"));
    CLOCK_OFFSET.with(|offset| offset.set(0));

    let audit = std::fs::read_to_string(users.dir().join("audit.jsonl")).unwrap();
    assert_eq!(audit.matches("\"clientCloseRequested\"").count(), 3);
    assert!(audit.contains("\"clientCloseRefused\"") && audit.contains("\"clientCloseOutcome\""));
    clients_api::with_registry(|registry| registry.clear());
}

/// user-home: every assigned principal reads and writes its own home page
/// (revisioned) and cover; pending devices are refused, a `read` device may
/// only read.
#[test]
fn own_home_page_and_cover_per_user() {
    use base64::Engine;
    use crate::management::client_tokens::DeviceScope;
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let tokens = ClientTokenStore::open_default().unwrap();
    let owner = tokens.issue("owner-desktop").unwrap();
    let users = UserStore::open_default().unwrap();
    users.snapshot().unwrap();
    let alice = users.create_user("Alice", None, Grant::default(), "root").unwrap();
    let bob = users.create_user("Bob", None, Grant::default(), "root").unwrap();
    let a = tokens.issue("alice-laptop").unwrap();
    let a_read = tokens.issue_scoped("alice-read", DeviceScope::Read).unwrap();
    let a_chat = tokens.issue_scoped("alice-chat", DeviceScope::Chat).unwrap();
    let b = tokens.issue("bob-laptop").unwrap();
    let pending = tokens.issue("stranger").unwrap();
    for token in [&a, &a_read, &a_chat] {
        users.bind_token(&token.token_id, Some(&alice.id), "root").unwrap();
    }
    users.bind_token(&b.token_id, Some(&bob.id), "root").unwrap();
    std::thread::sleep(Duration::from_millis(30));

    let caps = exchange("GET", "/v2/capabilities", Some(&a.token), &json!({})).1;
    assert!(caps["featureList"].as_array().unwrap().iter().any(|f| f == "user-home"));
    assert_eq!(caps["features"]["userHome"], true);
    assert!(caps["featureList"].as_array().unwrap().iter().any(|f| f == "user-home-page-pins"));
    assert_eq!(caps["features"]["userHomePagePins"], true);

    // Empty, then a roundtrip with unknown keys kept and coverEtag server-owned.
    let (status, value) = exchange("GET", "/v2/users/me/home", Some(&a.token), &json!({}));
    assert_eq!((status, &value), (200, &json!({"revision": 0, "home": null, "coverEtag": null, "updatedAt": null})));
    crate::management::v2::schema_export::assert_valid("users", "homeView", &value);
    let home = json!({
        "layout": "card",
        "blocks": [{"id": "pins", "visible": true}, {"id": "stats", "visible": false}],
        "bio": "Hello",
        "sidebarEntry": true,
        "showWhenEmpty": true,
        "homeHost": "host-a",
        "pins": [
            {"hostId": "host-a", "kind": "service", "key": "web", "title": "Web", "size": "m"},
            {"hostId": "host-a", "kind": "page", "key": "project-view:proj-1", "title": "Project"},
        ],
        "coverEtag": "ffffffffffffffff",
        "laterFeature": {"on": true},
    });
    crate::management::v2::schema_export::assert_valid("users", "homePut", &json!({"revision": 0, "home": home}));
    let (status, value) = exchange("PUT", "/v2/users/me/home", Some(&a.token), &json!({"revision": 0, "home": home}));
    assert_eq!((status, &value["revision"]), (200, &json!(1)), "{value}");
    assert_eq!((&value["home"]["laterFeature"], &value["home"]["coverEtag"]), (&json!({"on": true}), &Value::Null));
    crate::management::v2::schema_export::assert_valid("users", "homeView", &value);
    let (_, again) = exchange("GET", "/v2/users/me/home", Some(&a_read.token), &json!({}));
    assert_eq!(again, value, "another device of the same user reads the same");

    // Optimistic concurrency.
    let (status, value) = exchange("PUT", "/v2/users/me/home", Some(&a.token), &json!({"revision": 0, "home": {"layout": "gallery"}}));
    assert_eq!((status, code(&value), &value["error"]["details"]["revision"]), (409, "home_revision_conflict", &json!(1)), "{value}");

    // Shape and size.
    for (body, status_code, expected) in [
        (json!({"home": {}}), 400, "invalid_request"),
        (json!({"revision": -1, "home": {}}), 400, "invalid_request"),
        (json!({"revision": 1}), 400, "invalid_request"),
        (json!({"revision": 1, "home": {}, "extra": 1}), 400, "invalid_request"),
        (json!({"revision": 1, "home": []}), 400, "invalid_home"),
        (json!({"revision": 1, "home": {"bio": "x".repeat(201)}}), 400, "invalid_home"),
        (json!({"revision": 1, "home": {"pins": [{"hostId": "h", "kind": "folder", "key": "k"}]}}), 400, "invalid_home"),
        (json!({"revision": 1, "home": {"pins": [{"hostId": "h", "kind": "page", "key": "no-page-id"}]}}), 400, "invalid_home"),
        (json!({"revision": 1, "home": {"blocks": [{"id": "a", "visible": "yes"}]}}), 400, "invalid_home"),
        (json!({"revision": 1, "home": {"blob": "x".repeat(70 * 1024)}}), 413, "home_too_large"),
    ] {
        let (status, value) = exchange("PUT", "/v2/users/me/home", Some(&a.token), &body);
        assert_eq!((status, code(&value)), (status_code, expected), "{body}: {value}");
    }

    // Per-user isolation: Bob and the owner start empty and keep their own.
    let (_, value) = exchange("GET", "/v2/users/me/home", Some(&b.token), &json!({}));
    assert_eq!((&value["revision"], &value["home"]), (&json!(0), &Value::Null));
    let (status, value) = exchange("PUT", "/v2/users/me/home", Some(&owner.token), &json!({"revision": 0, "home": {"bio": "owner"}}));
    assert_eq!(status, 200, "{value}");
    let (_, value) = exchange("GET", "/v2/users/me/home", Some(ROOT), &json!({}));
    assert_eq!(value["home"]["bio"], "owner", "root credential = owner");
    let (_, value) = exchange("GET", "/v2/users/me/home", Some(&a.token), &json!({}));
    assert_eq!(value["home"]["bio"], "Hello");
    let (status, value) = exchange("POST", "/v2/users/me/home", Some(ROOT), &json!({}));
    assert_eq!((status, code(&value)), (405, "method_not_allowed"));
    let (status, value) = exchange("POST", "/v2/users/me/home", Some(&a.token), &json!({}));
    assert_eq!((status, code(&value)), (403, "owner_required"));

    // Pending devices are refused.
    for (method, path) in [("GET", "/v2/users/me/home"), ("PUT", "/v2/users/me/home"), ("GET", "/v2/users/me/home/cover"), ("DELETE", "/v2/users/me/home/cover")] {
        let (status, value) = exchange(method, path, Some(&pending.token), &json!({"revision": 0, "home": {}}));
        assert_eq!((status, code(&value)), (403, "access_pending"), "{method} {path}");
    }

    // Device scope: read only reads; chat may edit the own home page.
    for (method, path) in [("PUT", "/v2/users/me/home"), ("PUT", "/v2/users/me/home/cover"), ("DELETE", "/v2/users/me/home/cover")] {
        let (status, value) = exchange(method, path, Some(&a_read.token), &json!({"revision": 1, "home": {}}));
        assert_eq!((status, code(&value), &value["error"]["details"]["required"]), (403, "device_scope_denied", &json!("chat")), "{method} {path}");
    }
    let (status, value) = exchange("PUT", "/v2/users/me/home", Some(&a_chat.token), &json!({"revision": 1, "home": {"layout": "gallery"}}));
    assert_eq!((status, &value["revision"]), (200, &json!(2)), "{value}");

    // Cover: put, get (ETag), conditional get, size / type limits, delete.
    let cover_png = {
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(1500, 500).write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    };
    let upload = |bytes: &[u8], content_type: &str| json!({"contentType": content_type, "data": base64::engine::general_purpose::STANDARD.encode(bytes)});
    let (status, value) = exchange("GET", "/v2/users/me/home/cover", Some(&a.token), &json!({}));
    assert_eq!((status, code(&value)), (404, "cover_not_found"));
    let (status, value) = exchange("PUT", "/v2/users/me/home/cover", Some(&a_chat.token), &upload(&cover_png, "image/png"));
    assert_eq!(status, 200, "{value}");
    crate::management::v2::schema_export::assert_valid("users", "homeView", &value);
    let etag = value["coverEtag"].as_str().unwrap().to_string();
    assert_eq!((&value["home"]["coverEtag"], &value["revision"]), (&json!(etag), &json!(2)), "cover keeps the revision");
    let (status, head, bytes) = exchange_raw("GET", "/v2/users/me/home/cover", Some(&a_read.token), &json!({}), "");
    assert_eq!(status, 200);
    assert!(head.contains("Content-Type: image/png") && head.contains(&format!("ETag: \"{etag}\"")), "{head}");
    assert_eq!(bytes, cover_png);
    let (status, _, bytes) = exchange_raw("GET", "/v2/users/me/home/cover", Some(&a.token), &json!({}), &format!("If-None-Match: \"{etag}\"\r\n"));
    assert_eq!((status, bytes.len()), (304, 0));
    let (status, value) = exchange("GET", "/v2/users/me/home/cover", Some(&b.token), &json!({}));
    assert_eq!((status, code(&value)), (404, "cover_not_found"), "covers are per user");
    let (_, value) = exchange("GET", "/v2/users/me/home", Some(&a.token), &json!({}));
    assert_eq!(value["home"]["coverEtag"], json!(etag));
    let (status, value) = exchange("PUT", "/v2/users/me/home/cover", Some(&a.token), &upload(&cover_png, "image/jpeg"));
    assert_eq!((status, code(&value)), (400, "invalid_cover"));
    let (status, value) = exchange("PUT", "/v2/users/me/home/cover", Some(&a.token), &upload(&vec![0u8; 2 * 1024 * 1024 + 1], "image/png"));
    assert_eq!((status, code(&value)), (413, "cover_too_large"));
    let (status, value) = exchange("DELETE", "/v2/users/me/home/cover", Some(&a.token), &json!({}));
    assert_eq!((status, &value["coverEtag"]), (200, &Value::Null));
    let (status, _) = exchange("GET", "/v2/users/me/home/cover", Some(&a.token), &json!({}));
    assert_eq!(status, 404);

    // Deleting a user drops its home page.
    users.bind_token(&b.token_id, None, "root").unwrap();
    exchange("PUT", "/v2/users/me/home", Some(&owner.token), &json!({"revision": 1, "home": null}));
    let homes = crate::management::user_home::HomeStore::open_default().unwrap();
    homes.put(&bob.id, 0, Some(serde_json::Map::new()), "root").unwrap();
    let (status, _) = exchange("DELETE", &format!("/v2/users/{}", bob.id), Some(&owner.token), &json!({}));
    assert_eq!(status, 200);
    assert_eq!(homes.get(&bob.id).unwrap().revision, 0);
    assert_eq!(homes.get(&alice.id).unwrap().revision, 2);
}
#[test]
fn relay_loopback_never_accepts_root_public_pairing_or_another_device_token() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let tokens = ClientTokenStore::open_default().unwrap();
    let (a, _) = tokens.issue_device("relay A", "A", "android").unwrap();
    let (b, _) = tokens.issue_device("relay B", "B", "android").unwrap();
    let host_cli = tokens.issue("host desktop").unwrap();
    struct ClearRelay;
    impl Drop for ClearRelay { fn drop(&mut self) { crate::management::relay::set_peer(None); } }
    let _clear = ClearRelay;
    crate::management::relay::set_peer(Some(crate::management::relay::RelayPeer {device_id: a.token_id.clone(), certificate_id: "a".repeat(64)}));
    // exchange_raw explicitly supplies remote=false and loopback peer. TLS
    // identity still forces remote permissions and owner-local refusal.
    for bearer in [None, Some(ROOT)] {
        let (status, _) = exchange("GET", "/v2/users/me", bearer, &json!({}));
        assert_eq!(status, 401);
    }
    for path in ["/", "/v2/pair", "/v2/pairing-codes", "/peer/v1/pair", "/v2/relay/certificates"] {
        let (status, value) = exchange("POST", path, Some(&a.token), &json!({}));
        assert_eq!((status, code(&value)), (403, "relay_path_forbidden"), "{path}: {value}");
    }
    assert_eq!(exchange("OPTIONS", "/v2", Some(&b.token), &json!({})).0, 403);
    for bearer in [&b.token, &host_cli.token] {
        let (status, value) = exchange("GET", "/v2/users/me", Some(bearer), &json!({}));
        assert_eq!((status, code(&value)), (403, "relay_device_mismatch"), "{value}");
    }
    let (status, value) = exchange("GET", "/v2/users/me", Some(&a.token), &json!({}));
    assert_eq!(status, 200, "{value}");
    // A same-device certificate cannot turn an owner device into owner-local.
    let (status, value) = exchange("DELETE", "/v2/users/owner-person", Some(&a.token), &json!({}));
    assert_eq!((status, code(&value)), (403, "owner_person_local_only"), "{value}");
    crate::management::relay::set_peer(None);
    // Ordinary loopback keeps its old root permissions/public health route.
    assert_eq!(exchange_raw("GET", "/", Some(ROOT), &json!({}), "").0, 200);
}
#[test]
fn round2_relay_read_chat_use_scoped_workspace_and_never_host_files() {
    use crate::management::client_tokens::DeviceScope;
    let _lock=crate::config::paths::test_home_env_lock();let instance=PrivateInstance::new();
    let work=instance.root.join("workspace");std::fs::create_dir_all(&work).unwrap();std::fs::write(work.join("ok.txt"),"workspace-ok").unwrap();
    let host=crate::platform::host::current_host_name();let mut sessions=crate::session::model::CutexSessionStore::default();
    let mut record=crate::session::model::CutexSessionRecord::new("cutex.relay-files".into(),None,host,work.to_string_lossy().into_owned(),None).unwrap();
    record.exposed_to_backend=true;record.registration_class=crate::agent_bus::model::AgentRegistrationClass::Persistent;sessions.sessions.insert(record.cutex_session_id.clone(),record);
    let mut broad=crate::session::model::CutexSessionRecord::new("cutex.relay-broad".into(),None,crate::platform::host::current_host_name(),instance.root.to_string_lossy().into_owned(),None).unwrap();broad.exposed_to_backend=true;broad.registration_class=crate::agent_bus::model::AgentRegistrationClass::Persistent;sessions.sessions.insert(broad.cutex_session_id.clone(),broad);
    crate::session::store::save_cutex_session_store(&sessions).unwrap();
    let tokens=ClientTokenStore::open_default().unwrap();let users=UserStore::beside(&tokens);
    let path=format!("/v2/fs/file?path={}",url::form_urlencoded::byte_serialize(work.join("ok.txt").to_string_lossy().as_bytes()).collect::<String>());
    for scope in [DeviceScope::Read,DeviceScope::Chat,DeviceScope::Full] {
        let(device,_)=tokens.issue_device_scoped("test","phone","android",scope).unwrap();users.bind_token(&device.token_id,Some(OWNER),"root").unwrap();
        crate::management::relay::set_peer(Some(crate::management::relay::RelayPeer{device_id:device.token_id.clone(),certificate_id:"a".repeat(64)}));
        let(status,value)=p2_exchange("GET",&path,&device.token,&json!({}));
        assert_eq!(status,if scope.is_full(){200}else{403},"{value}");
        if !scope.is_full(){assert_eq!(code(&value),"relay_workspace_only");assert_eq!(p2_exchange("GET","/v2/fs/dirs",&device.token,&json!({})).0,403);}
        if !scope.is_full(){let(status,value)=p2_exchange("GET","/v2/sessions/cutex.relay-broad/fs/dir",&device.token,&json!({}));assert_eq!((status,code(&value)),(403,"workspace_root_too_broad"));}
        let(status,value)=p2_exchange("GET","/v2/sessions/cutex.relay-files/fs/file?path=ok.txt",&device.token,&json!({}));assert_eq!(status,200,"{value}");
        crate::management::relay::set_peer(None);
        assert_eq!(p2_exchange("GET",&path,&device.token,&json!({})).0,200,"ordinary listener compatibility");
    }
}
#[test]
fn round2_secret_file_identity_protects_hardlinks_across_file_workspace_static() {
    let _lock=crate::config::paths::test_home_env_lock();let instance=PrivateInstance::new();
    let home=crate::config::paths::config_dir().unwrap();std::fs::create_dir_all(home.join("management-relay")).unwrap();
    std::fs::write(home.join("config.json"),serde_json::to_vec(&json!({"management_api_token":ROOT})).unwrap()).unwrap();std::fs::write(home.join("management-relay/identity.json"),"ca-fixture").unwrap();
    let outside=instance.root.join("safe");std::fs::create_dir_all(&outside).unwrap();
    let tokens=ClientTokenStore::open_default().unwrap();let(device,_)=tokens.issue_device("test","phone","android").unwrap();
    UserStore::beside(&tokens).bind_token(&device.token_id,Some(OWNER),"root").unwrap();
    let mut sessions=crate::session::model::CutexSessionStore::default();
    let mut record=crate::session::model::CutexSessionRecord::new("cutex.alias".into(),None,crate::platform::host::current_host_name(),outside.to_string_lossy().into_owned(),None).unwrap();record.exposed_to_backend=true;record.registration_class=crate::agent_bus::model::AgentRegistrationClass::Persistent;sessions.sessions.insert(record.cutex_session_id.clone(),record);crate::session::store::save_cutex_session_store(&sessions).unwrap();
    let resolved=crate::management::v2::workspace::ResolvedRoot::from_dir(crate::management::v2::workspace::Scope{kind:crate::management::v2::workspace::ScopeKind::Session,id:"fixture".into()},&outside).unwrap();
    for (secret,name) in [(home.join("config.json"),"config-alias.html"),(home.join("management-relay/identity.json"),"ca-alias.html")] {
        let alias=outside.join(name);std::fs::hard_link(secret,&alias).unwrap();
        let path=format!("/v2/fs/file?path={}",url::form_urlencoded::byte_serialize(alias.to_string_lossy().as_bytes()).collect::<String>());
        assert_eq!(exchange("GET",&path,Some(&device.token),&json!({})).0,403,"hardlink {name}");
        assert_eq!(p2_exchange("GET",&format!("/v2/sessions/cutex.alias/fs/file?path={name}"),&device.token,&json!({})).0,403,"workspace hardlink {name}");
        listeners::set_current_connection(Some(ConnectionInfo{remote:true,peer:"127.0.0.1:40000".parse().unwrap()}));
        assert!(crate::management::v2::workspace_fs::resolve_rel(&resolved,name).is_err());
        assert!(crate::services::static_files::resolve(&std::fs::canonicalize(&outside).unwrap(),&format!("/{name}")).is_none());
    }
}
#[test]
#[cfg(windows)]
fn round2_http_path_route_inventory_rejects_network_and_device_inputs() {
    let _lock=crate::config::paths::test_home_env_lock();let _instance=PrivateInstance::new();
    let tokens=ClientTokenStore::open_default().unwrap();let(dev,_)=tokens.issue_device("paths","paths","android").unwrap();UserStore::beside(&tokens).bind_token(&dev.token_id,Some(OWNER),"root").unwrap();
    for alias in [r"\\localhost\C$\missing-review-root",r"\\127.0.0.1\C$\missing-review-root",r"\\?\UNC\localhost\C$\fixture",r"\\?\C:\fixture",r"\\.\C:\fixture",r"C:\fixture::$DATA"] {
        let encoded=url::form_urlencoded::byte_serialize(alias.as_bytes()).collect::<String>();
        for endpoint in ["/v2/fs/file","/v2/fs/dirs"] {assert_eq!(exchange("GET",&format!("{endpoint}?path={encoded}"),Some(&dev.token),&json!({})).0,403,"{endpoint} {alias}");}
        assert_eq!(exchange("POST","/v2/services",Some(ROOT),&json!({"slug":"bad-root","runtime":"static","root":alias})).0,403);
        assert_eq!(exchange("GET",&format!("/v2/claude/skills/import?source={encoded}"),Some(ROOT),&json!({})).0,400);
        assert_eq!(exchange("POST","/v2/claude/skills/import",Some(ROOT),&json!({"source":alias,"names":["fixture"]})).0,400);
        assert_eq!(exchange("POST","/v2/codex/skills/archive",Some(ROOT),&json!({"requestId":uuid::Uuid::new_v4().to_string(),"path":alias})).0,403,"archive {alias}");
        assert!(!crate::management::v2::attachments::uploaded_by("cutex.test",&dev.token_id,alias));
        assert!(crate::management::v2::user_input::validate_user_input(&json!({"type":"localImage","path":alias})).is_err());
        let mut sessions=crate::session::store::load_cutex_session_store().unwrap();let mut record=crate::session::model::CutexSessionRecord::new("cutex.unc-root".into(),None,crate::platform::host::current_host_name(),alias.into(),None).unwrap();record.exposed_to_backend=true;record.registration_class=crate::agent_bus::model::AgentRegistrationClass::Persistent;sessions.sessions.insert(record.cutex_session_id.clone(),record);crate::session::store::save_cutex_session_store(&sessions).unwrap();
        for suffix in ["fs/dir","fs/file?path=x","fs/search?q=x","fs/watch","git/status","git/diff?path=x"] {assert_eq!(p2_exchange("GET",&format!("/v2/sessions/cutex.unc-root/{suffix}"),&dev.token,&json!({})).0,403,"{suffix} {alias}");}
    }
}
#[test]
fn relay_certificate_api_requires_owner_local_and_issues_only_registered_devices() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let tokens = ClientTokenStore::open_default().unwrap();
    let (device, _) = tokens.issue_device("phone", "phone", "android").unwrap();
    let users = UserStore::beside(&tokens);
    users.bind_token(&device.token_id, Some(OWNER), "root").unwrap();
    let (status, value) = exchange("POST", "/v2/relay/setup", Some(ROOT), &json!({"bind":"127.0.0.1:39483", "serverNames":["home.test"]}));
    assert_eq!(status, 201, "{value}");
    assert!(value["caPem"].as_str().unwrap().contains("BEGIN CERTIFICATE"));
    assert_eq!(value["serverCaSha256"].as_str().unwrap().len(), 64);
    assert!(!value.to_string().contains("PRIVATE KEY"));
    let key = rcgen::KeyPair::generate().unwrap();
    let csr = rcgen::CertificateParams::default().serialize_request(&key).unwrap().pem().unwrap();
    let (enroll_status,enrollment)=exchange("POST","/v2/relay-enrollment",Some(&device.token),&json!({"csrPem":csr,"deviceId":"untrusted-device-id"}));
    assert_eq!(enroll_status,200,"{enrollment}");assert_eq!(enrollment["deviceId"],device.token_id);
    let body = json!({"enrollmentId":enrollment["enrollmentId"],"csrSha256":enrollment["csrSha256"],"keySha256":enrollment["keySha256"],"confirm":true});
    let (status, value) = exchange("POST", "/v2/relay/certificates", Some(&device.token), &body);
    // r290 P10 (B6): a full-scope owner device must confirm first.
    assert_eq!((status, code(&value)), (428, "owner_confirm_required"));
    REMOTE.with(|c| c.set(true));
    let host_cli = tokens.issue("owner desktop").unwrap();
    users.bind_token(&host_cli.token_id, Some(OWNER), "root").unwrap();
    let (status, value) = exchange("POST", "/v2/relay/certificates", Some(&host_cli.token), &body);
    REMOTE.with(|c| c.set(false));
    assert_eq!((status, code(&value)), (428, "owner_confirm_required")); // r290 P10 (B6)
    let (status, value) = exchange("POST", "/v2/relay/certificates", Some(ROOT), &body);
    assert_eq!(status, 201, "{value}");
    assert_eq!(value["schema"], "cutex.relay-certificate.v1");
    assert_eq!(value["deviceId"], device.token_id);
    let cert_id = value["certificateId"].as_str().unwrap();
    let path = format!("/v2/relay/certificates/{cert_id}");
    let (_, records) = exchange("GET", "/v2/relay/certificates", Some(ROOT), &json!({}));
    assert_eq!(records["certificates"].as_array().unwrap().len(), 1);
    assert!(!records.to_string().contains("PRIVATE KEY"));
    assert_eq!(exchange_raw("DELETE", &format!("/v2/relay/certificates/{}",cert_id.to_ascii_uppercase()), Some(ROOT), &json!({}), "").0, 204);
    assert_eq!(exchange_raw("DELETE", &path, Some(ROOT), &json!({}), "").0, 204);
    let (_, records) = exchange("GET", "/v2/relay/certificates", Some(ROOT), &json!({}));
    assert_eq!(records["certificates"][0]["revoked"], true);
    assert_eq!(records["certificates"][0]["active"], false);
    let (status, value) = exchange("POST", "/v2/relay/certificates", Some(ROOT), &json!({"deviceId":"unknown", "csrPem": csr}));
    assert_eq!((status, code(&value)), (400, "relay_invalid_request"));
}

#[test]
fn review_relay_private_directory_never_reaches_non_owner_local_file_reads() {
    let _home_lock=crate::config::paths::test_home_env_lock();let _instance=PrivateInstance::new();
    let tokens=ClientTokenStore::open_default().unwrap();let users=UserStore::beside(&tokens);
    let dir=crate::config::paths::config_dir().unwrap().join("management-relay");std::fs::create_dir_all(&dir).unwrap();
    let file=dir.join("identity.json");std::fs::write(&file,b"private-ca-fixture").unwrap();
    for scope in crate::management::client_tokens::DeviceScope::ALL {
        let (token,_)=tokens.issue_device_scoped("phone","phone","android",scope).unwrap();
        users.bind_token(&token.token_id,Some(OWNER),"root").unwrap();
        for remote in [false,true] {
            REMOTE.with(|r|r.set(remote));
            let (status,value)=exchange("GET",&format!("/v2/fs/file?path={}",file.display()),Some(&token.token),&json!({}));
            REMOTE.with(|r|r.set(false));
            let expected=if scope.is_full(){"owner_local_only"}else{"credential_read_denied"};
            assert_eq!((status,code(&value)),(403,expected),"scope={scope:?} remote={remote}: {value}");
        }
    }
    let mut proof=crate::management::client_tokens::VerifiedClientToken::test_token("paired");proof.issued_via=crate::management::client_tokens::IssuedVia::Pairing;
    listeners::set_current_client_token(Some(proof));
    let meta=file.metadata().unwrap();
    assert!(crate::management::v2::workspace_fs::file_content_value(&file,"identity.json",&meta,1024).is_err());
    assert!(crate::management::v2::host_files::refuse_root_credential_file(&dir).is_err());
    assert!(crate::management::v2::host_files::refuse_private_tree(dir.parent().unwrap()).is_err());
    assert!(crate::artifacts::paths::forbidden_reason(&std::fs::canonicalize(&file).unwrap()).is_some());
    listeners::set_current_client_token(None);
    let (_,value)=exchange("GET",&format!("/v2/fs/file?path={}",file.display()),Some(ROOT),&json!({}));
    assert_eq!(value["content"],"private-ca-fixture","owner-local access remains possible");
}

#[test]
fn credential_locations_are_denied_for_read_chat_on_all_owner_token_transports() {
    use crate::management::client_tokens::DeviceScope;
    let home=crate::profiles::lifecycle::tests::TestHome::new();
    std::fs::write(home.root().join(crate::config::paths::PRIVATE_HOME_MARKER),b"").unwrap();
    let instance=PrivateInstance::new();
    let tokens=ClientTokenStore::open_default().unwrap();let users=UserStore::beside(&tokens);
    let secret=home.root().join(".ssh/id_ed25519");
    std::fs::create_dir_all(secret.parent().unwrap()).unwrap();std::fs::write(&secret,"synthetic credential").unwrap();
    let alias=instance.root.join("ordinary-alias.txt");std::fs::hard_link(&secret,&alias).unwrap();
    let config=crate::config::paths::config_dir().unwrap();
    let key=config.join("profile-keys/test.json");std::fs::create_dir_all(key.parent().unwrap()).unwrap();std::fs::write(&key,"synthetic credential").unwrap();
    let feature_key = config.join(crate::management::v2::feature_profiles::DIR)
        .join("fp-00000000-0000-4000-8000-000000000000.json");
    std::fs::create_dir_all(feature_key.parent().unwrap()).unwrap();
    std::fs::write(&feature_key, "synthetic credential").unwrap();
    let report=config.join("projects/p/memory/MEMORY.md");let artifact=config.join("runtime/artifacts/blobs/ab/hash");
    for file in [&report,&artifact] {std::fs::create_dir_all(file.parent().unwrap()).unwrap();std::fs::write(file,"ordinary preview").unwrap();}
    let mut runtime_cases = Vec::new();
    for relative in ["runtime/claude/sessions/s/claude-api-key.bound.json", "runtime/claude/sessions/s/rpc-token", "runtime/app-server/native/capability-token", "runtime/dsh/s/launch.json", "profiles/p/claude/.credentials.json", "profiles/p/claude/api_key"] {
        let file = config.join(relative); std::fs::create_dir_all(file.parent().unwrap()).unwrap(); std::fs::write(&file, "synthetic credential").unwrap(); runtime_cases.push(file);
    }
    for scope in DeviceScope::ALL {
        for paired in [false,true] {
            let token=if paired {tokens.issue_device_scoped("credential-device","fixture","other",scope).unwrap().0}
                else {tokens.issue_scoped("credential-cli",scope).unwrap()};
            users.bind_token(&token.token_id,Some(OWNER),"root").unwrap();
            for remote in [false,true] {
                let read=|file:&std::path::Path| {
                    REMOTE.with(|r|r.set(remote));
                    let out=exchange("GET",&format!("/v2/fs/file?path={}",file.display()),Some(&token.token),&json!({}));
                    REMOTE.with(|r|r.set(false));out
                };
                for file in [&secret,&alias,&key,&feature_key].into_iter().chain(runtime_cases.iter()) {
                    let (status,value)=read(file);
                    if scope.is_full() {assert_eq!(status,200,"{value}");assert_eq!(value["content"],"synthetic credential");}
                    else {assert_eq!((status,code(&value)),(403,"credential_read_denied"),"{scope:?} paired={paired} remote={remote}: {value}");assert!(value["error"]["message"].as_str().unwrap().contains("full owner token"));}
                }
                for file in [&report,&artifact] {let(status,value)=read(file);assert_eq!(status,200,"{value}");assert_eq!(value["content"],"ordinary preview");}
            }
        }
    }
}

#[test]
fn relay_enrollment_and_early_renewal_bind_csr_to_device_without_widening_read_scope() {
    use crate::management::client_tokens::DeviceScope;
    let _lock=crate::config::paths::test_home_env_lock();let _instance=PrivateInstance::new();
    let tokens=ClientTokenStore::open_default().unwrap();let users=UserStore::beside(&tokens);
    let(a,_)=tokens.issue_device_scoped("A","A","android",DeviceScope::Read).unwrap();
    let(b,_)=tokens.issue_device("B","B","android").unwrap();
    users.bind_token(&a.token_id,Some(OWNER),"root").unwrap();users.bind_token(&b.token_id,Some(OWNER),"root").unwrap();
    let(status,setup)=exchange("POST","/v2/relay/setup",Some(ROOT),&json!({"bind":"127.0.0.1:39485","serverNames":["renew.test"]}));assert_eq!(status,201,"{setup}");
    let key=rcgen::KeyPair::generate().unwrap();let csr=rcgen::CertificateParams::default().serialize_request(&key).unwrap().pem().unwrap();
    let(status,enrollment)=exchange("POST","/v2/relay-enrollment",Some(&a.token),&json!({"csrPem":csr,"deviceId":b.token_id}));assert_eq!(status,200,"{enrollment}");assert_eq!(enrollment["deviceId"],a.token_id);
    let poll=format!("/v2/relay-enrollment/{}",enrollment["enrollmentId"].as_str().unwrap());assert_eq!(exchange("GET",&poll,Some(&b.token),&json!({})).0,403);
    let mut approve=json!({"enrollmentId":enrollment["enrollmentId"],"csrSha256":"wrong","keySha256":enrollment["keySha256"],"confirm":true});assert_eq!(exchange("POST","/v2/relay/certificates",Some(ROOT),&approve).0,400);
    approve["csrSha256"]=enrollment["csrSha256"].clone();let(status,issued)=exchange("POST","/v2/relay/certificates",Some(ROOT),&approve);assert_eq!(status,201,"{issued}");
    let(_,polled)=exchange("GET",&poll,Some(&a.token),&json!({}));assert_eq!(polled["certificate"]["certificateId"],issued["certificateId"]);
    assert_eq!(issued["origin"], "https://renew.test");
    assert!(issued.get("tailnet").is_none());
    assert_eq!(polled["certificate"], issued);
    struct Reset;impl Drop for Reset{fn drop(&mut self){crate::management::relay::set_peer(None);}}
    let _reset=Reset;let certificate_id=issued["certificateId"].as_str().unwrap().to_string();
    crate::management::relay::set_peer(Some(crate::management::relay::RelayPeer{device_id:a.token_id.clone(),certificate_id:certificate_id.clone()}));
    let(status,state)=exchange("GET","/v2/relay-certificate",Some(&a.token),&json!({}));assert_eq!(status,200,"{state}");assert_eq!(state["renewalRequired"],false);assert!(state["server"]["expiresAt"].is_string());
    let new_key=rcgen::KeyPair::generate().unwrap();let new_csr=rcgen::CertificateParams::default().serialize_request(&new_key).unwrap().pem().unwrap();
    let(status,value)=exchange("POST","/v2/relay-certificate/renew",Some(&a.token),&json!({"csrPem":new_csr}));assert_eq!((status,code(&value)),(409,"relay_renewal_too_early"));
    let path=crate::config::paths::config_dir().unwrap().join("management-relay/certificates.json");let mut registry:Value=serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();registry["certificates"][0]["expiresAt"]=json!((chrono::Utc::now()+chrono::Duration::days(6)).to_rfc3339());crate::config::atomic::write_private_pretty_json_atomic(&path,&registry,"test aging").unwrap();
    let(status,renewed)=exchange("POST","/v2/relay-certificate/renew",Some(&a.token),&json!({"csrPem":new_csr}));assert_eq!(status,200,"{renewed}");assert_eq!(renewed["deviceId"],a.token_id);assert_ne!(renewed["certificateId"],issued["certificateId"]);
    let(status,retry)=exchange("POST","/v2/relay-certificate/renew",Some(&a.token),&json!({"csrPem":new_csr}));assert_eq!(status,200,"{retry}");assert_eq!(retry["certificateId"],renewed["certificateId"]);
    assert_eq!(renewed["origin"], issued["origin"]);
    assert!(renewed.get("tailnet").is_none());
    assert_eq!(retry, renewed);
    assert_eq!(tokens.list().unwrap().into_iter().find(|t|t.token_id==a.token_id).unwrap().scope,DeviceScope::Read);
    let(status,value)=exchange("POST","/v2/relay-certificate/renew",Some(&b.token),&json!({"csrPem":new_csr}));assert_eq!((status,code(&value)),(403,"relay_device_mismatch"));
    crate::management::relay::set_peer(None);assert_eq!(exchange("POST","/v2/relay-certificate/renew",Some(&a.token),&json!({"csrPem":new_csr})).0,403);
    let(status,rotated)=exchange("POST","/v2/relay/rotate-server",Some(ROOT),&json!({}));assert_eq!(status,201,"{rotated}");assert_eq!(rotated["serverCaSha256"],setup["serverCaSha256"]);assert!(!rotated.to_string().contains("PRIVATE KEY"));
}


#[test]
fn creator_path_refusal_has_a_stable_code_and_session_id_on_the_wire() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let mut ctx = context();
    ctx.explicit_launch_action = |_, _| Err(crate::agent_management::AgentManagementError::CreatorPathsChanged("cutex.refused".into()));
    let request = json!({"operation":"runtime_status","action_id":"path-refusal-test"});
    let (status, _, bytes) = exchange_with(ctx, "POST", "/v2/agent-management/explicit-launch", Some(ROOT), request.to_string().into_bytes(), "application/json", "");
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(status, 409, "{body}");
    assert_eq!(code(&body), "creator_paths_changed");
    assert_eq!(body["error"]["details"]["cutexSessionId"], "cutex.refused");
}

#[test]
fn artifact_raw_download_keeps_content_owner_and_device_access_rules() {
    use crate::management::client_tokens::DeviceScope;
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let tokens = ClientTokenStore::open_default().unwrap();
    let users = UserStore::open_default().unwrap();
    let user = users.create_user("Artifact member", None, Grant::default(), "root").unwrap();
    let token = tokens.issue("artifact-member").unwrap();
    users.bind_token(&token.token_id, Some(&user.id), "root").unwrap();
    let base = "/v2/artifact-store/af-00000000000000000000000000000000/versions/latest";
    for suffix in ["content", "raw"] {
        // r290 P1: artifact reads are scoped; an artifact outside the user's
        // visible projects / agents does not exist for it.
        let (status, body) = exchange("GET", &format!("{base}/{suffix}"), Some(&token.token), &json!({}));
        assert_eq!(status, 404, "{body}");
        assert_eq!(body["error"]["code"], "artifact_not_found");
        assert_eq!(exchange("GET", &format!("{base}/{suffix}"), None, &json!({})).0, 401);
        let owner = tokens.issue_scoped(&format!("artifact-owner-{suffix}"), DeviceScope::Read).unwrap();
        users.bind_token(&owner.token_id, Some(OWNER), "root").unwrap();
        assert_eq!(exchange("GET", &format!("{base}/{suffix}"), Some(&owner.token), &json!({})).0, 404);
    }
}

#[test]
fn artifact_raw_owner_read_device_downloads_twenty_mb_through_dispatcher() {
    use crate::artifacts::{store::{ArtifactStore, Content}, Scope as ArtifactScope};
    use crate::management::client_tokens::DeviceScope;
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let tokens = ClientTokenStore::open_default().unwrap();
    let users = UserStore::open_default().unwrap();
    let owner = tokens.issue_scoped("artifact-download-owner", DeviceScope::Read).unwrap();
    users.bind_token(&owner.token_id, Some(OWNER), "root").unwrap();
    let bytes = vec![0x5au8; 20 * 1024 * 1024];
    let sha = crate::asks::sha256_hex(&bytes);
    let mut new = crate::artifacts::store::tests::blob(&ArtifactScope::session("cutex.download"), "app.apk", "", crate::artifacts::Producer::owner());
    new.media_type = "application/vnd.android.package-archive".into();
    new.source_path = "app.apk".into();
    new.content = Content::Blob { bytes: bytes.clone(), sha256: sha.clone() };
    let outcome = ArtifactStore::open_default().unwrap().publish(new, 64 * 1024 * 1024, chrono::Utc::now()).unwrap();
    let path = format!("/v2/artifact-store/{}/versions/1/raw", outcome.artifact.artifact_id);
    let (status, headers, body) = exchange_raw("GET", &path, Some(&owner.token), &Value::Null, "");
    assert_eq!(status, 200, "{headers}");
    assert_eq!(body.len(), bytes.len());
    assert_eq!(crate::asks::sha256_hex(&body), sha);
    assert!(headers.contains(&format!("X-Cutex-Sha256: {sha}")));
    let (status, headers, body) = exchange_raw("HEAD", &path, Some(&owner.token), &Value::Null, "Range: bytes=0-1\r\n");
    assert_eq!(status, 200); assert!(body.is_empty()); assert!(headers.contains("Content-Length: 20971520"));
    let (status, _, body) = exchange_raw("GET", &path, Some(&owner.token), &Value::Null, "Range: bytes=7-15\r\n");
    assert_eq!(status, 206); assert_eq!(body, vec![0x5a; 9]);
    let user = users.create_user("Artifact denied", None, Grant::default(), "root").unwrap();
    let guest = tokens.issue("artifact-denied").unwrap(); users.bind_token(&guest.token_id, Some(&user.id), "root").unwrap();
    // r290 P1: not visible to a user without that agent: 404 (scoped read).
    let (status, body) = exchange("GET", &path, Some(&guest.token), &Value::Null);
    assert_eq!((status, code(&body)), (404, "artifact_not_found"), "{body}");
}

/// Owner powers on cutex requests (r289): a shared-access user with send
/// rights keeps its granted behaviour but never acts on the owner's state.
/// Callers in order: a scoped user's full device, the owner's chat device,
/// root. Returns each caller's bearer and whether it is the owner.
fn owner_power_callers(ids: &[&str]) -> Vec<(String, bool)> {
    use crate::management::client_tokens::DeviceScope;
    let users = UserStore::open_default().unwrap();
    let tokens = ClientTokenStore::open_default().unwrap();
    let sender = users.create_user("Owner-power sender", None, Grant {
        standalone_agents: ids.iter().map(|id| (*id).into()).collect(), send: true, ..Grant::default()
    }, "root").unwrap();
    let scoped = tokens.issue("owner-power-scoped-device").unwrap();
    users.bind_token(&scoped.token_id, Some(&sender.id), "root").unwrap();
    let owner = tokens.issue_scoped("owner-power-owner-chat", DeviceScope::Chat).unwrap();
    users.bind_token(&owner.token_id, Some(OWNER), "root").unwrap();
    vec![(scoped.token, false), (owner.token, true), (ROOT.to_string(), true)]
}

fn owner_power_sessions(instance: &PrivateInstance, ids: &[&str], class: crate::agent_bus::model::AgentRegistrationClass) {
    let mut sessions = crate::session::model::CutexSessionStore::default();
    for sid in ids {
        let mut record = crate::session::model::CutexSessionRecord::new((*sid).into(), Some(uuid::Uuid::new_v4().to_string()),
            crate::platform::host::current_host_name(), instance.root.to_string_lossy().into_owned(), None).unwrap();
        record.registration_class = class;
        record.exposed_to_backend = false;
        sessions.sessions.insert((*sid).into(), record);
    }
    crate::session::store::save_cutex_session_store(&sessions).unwrap();
}

/// A submit the runtime started (no native runtime in the fixture).
fn started_submit_context() -> ManagementRequestContext {
    let mut ctx = p2_context();
    ctx.handle_user_input = |_| {
        use crate::management::v2::user_input::{UserInputDisposition, UserInputSubmitExecution};
        Ok(UserInputSubmitExecution { disposition: UserInputDisposition::Started, app_server_accepted: true,
            native_request_id: Some(json!("owner-power-native")), native_method: Some("turn/start".into()),
            turn_id: Some("owner-power-turn".into()), queue: None,
            queue_paused: false, passive_notes: None, join_refusal: None, queued_reason: None })
    };
    ctx
}

fn owner_power_submit(ctx: ManagementRequestContext, sid: &str, bearer: &str, tag: &str, extra: Value) -> (u16, Value) {
    let mut params = json!({"clientUserMessageId": format!("owner-power-{tag}"),
        "origin": {"kind": "other", "clientId": "private-test"}, "strategy": "auto",
        "input": [{"type": "text", "text": "private fixture"}]});
    for (key, value) in extra.as_object().unwrap() { params[key] = value.clone(); }
    let body = json!({"requestId": format!("owner-power-req-{tag}"), "method": "cutex/userInput/submit", "params": params});
    let (status, _, bytes) = exchange_with(ctx, "POST", &format!("/v2/sessions/{sid}/cutex/requests"), Some(bearer),
        body.to_string().into_bytes(), "application/json", "");
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// Item 1: an offline submit with `startIfOffline` goes out past the
/// owner's queue pause only for the owner; a scoped user's input waits.
#[test]
fn scoped_offline_submit_does_not_bypass_the_owners_queue_pause() {
    use crate::management::v2::user_input::{self, UserInputOrigin, UserInputOriginKind, UserInputStrategy, UserInputSubmitParams};
    let _home_lock = crate::config::paths::test_home_env_lock();
    let instance = PrivateInstance::new();
    let ids = ["cutex.owner-power-bypass-scoped", "cutex.owner-power-bypass-owner", "cutex.owner-power-bypass-root"];
    owner_power_sessions(&instance, &ids, crate::agent_bus::model::AgentRegistrationClass::Persistent);
    let callers = owner_power_callers(&ids);
    let queue = user_input::user_input_repository().unwrap();
    let mut ctx = p2_context();
    ctx.handle_user_input = |command| {
        use crate::management::v2::user_input::{QueueEnqueueDecision, UserInputDisposition, UserInputSubmitExecution};
        let decision = user_input::user_input_repository().unwrap().enqueue(&command.cutex_session_id, &command.thread_id,
            &command.management_request_id, &command.params).unwrap();
        let item = match decision { QueueEnqueueDecision::Queued(item) | QueueEnqueueDecision::Deduplicated(item) => item,
            _ => panic!("private input identity unexpectedly conflicted") };
        Ok(UserInputSubmitExecution { disposition: UserInputDisposition::Queued, app_server_accepted: false,
            native_request_id: None, native_method: None, turn_id: None, queue: Some(item), queue_paused: true, passive_notes: None,
            join_refusal: None, queued_reason: None })
    };
    ctx.online_session = |_, _, _| Err(crate::management::v2::user_input::UserInputExecutionError { stage: "route".into(),
        code: "fixture_no_runtime_effect".into(), message: "no runtime in the fixture".into(), retryable: false,
        outcome_unknown: false, details: json!({}) });
    for (n, (bearer, is_owner)) in callers.iter().enumerate() {
        let sid = ids[n];
        // The owner paused this agent's queue behind an earlier message.
        queue.enqueue(sid, "owner-power-thread", &format!("owner-earlier-{n}"), &UserInputSubmitParams {
                interrupt_send: None,
            client_user_message_id: format!("owner-earlier-{n}"),
            origin: UserInputOrigin { kind: UserInputOriginKind::Other, client_id: "private-test".into() },
            strategy: UserInputStrategy::Queue, input: vec![json!({"type": "text", "text": "earlier"})],
            expected_turn_id: None, join_delegation: None }).unwrap();
        assert!(matches!(queue.pause(sid, "owner_stop", None).unwrap(), user_input::QueuePauseDecision::Paused { .. }));
        let (status, response) = owner_power_submit(ctx, sid, bearer, &format!("bypass-{n}"), json!({"startIfOffline": true}));
        assert_eq!(status, 200, "{response}");
        let result = &response["cutex"]["result"];
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !matches!(super::super::super::offline_submit::start_outcome(sid), Some(super::super::super::offline_submit::StartOutcome::Failed { .. })) {
            assert!(std::time::Instant::now() < deadline, "background start did not settle");
            std::thread::sleep(Duration::from_millis(10));
        }
        let (_, items) = queue.list(sid).unwrap();
        let sent_id = format!("owner-power-bypass-{n}");
        let sent = items.iter().find(|item| item.client_user_message_id == sent_id).unwrap();
        assert_eq!(sent.bypass_pause, *is_owner, "caller {n}: {response}");
        assert!(items.iter().filter(|item| item.client_user_message_id != sent_id).all(|item| !item.bypass_pause));
        assert_eq!(result["paused"], json!(!is_owner), "caller {n}: {response}");
    }
}

/// Item 2: a started message lifts the owner's delegation pause and Stop
/// hold (and reopens the native gate) only when the owner sent it.
#[test]
fn scoped_message_does_not_lift_the_owners_pause_or_stop_hold() {
    use crate::asks::{control, store::{AskStore, RuntimeCommand}};
    let _home_lock = crate::config::paths::test_home_env_lock();
    let instance = PrivateInstance::new();
    let ids = ["cutex.owner-power-hold-scoped", "cutex.owner-power-hold-owner", "cutex.owner-power-hold-root"];
    owner_power_sessions(&instance, &ids, crate::agent_bus::model::AgentRegistrationClass::Persistent);
    let callers = owner_power_callers(&ids);
    let store = AskStore::open_default().unwrap();
    let is_gate = |command: &RuntimeCommand, sid: &str|
        matches!(command, RuntimeCommand::ResumeGate { cutex_session_id } if cutex_session_id == sid);
    for stop in [false, true] {
        for (n, (bearer, is_owner)) in callers.iter().enumerate() {
            let sid = ids[n];
            store.with_state(|state| {
                let now = chrono::Utc::now();
                if stop { control::owner_stop(state, sid, now); } else { control::owner_pause(state, sid, now); }
                state.commands.retain(|queued| !is_gate(&queued.command, sid));
            }).unwrap();
            let (status, response) = owner_power_submit(started_submit_context(), sid, bearer, &format!("hold-{stop}-{n}"), json!({}));
            assert_eq!(status, 200, "{response}");
            assert_eq!(response["cutex"]["result"]["disposition"], "started", "{response}");
            let state = store.snapshot().unwrap();
            let gates = state.commands.iter().filter(|queued| is_gate(&queued.command, sid)).count();
            assert_eq!(state.pauses.contains_key(sid), !is_owner, "stop={stop} caller {n}: {response}");
            if *is_owner {
                assert!(gates > 0, "the owner's message reopens the native gate");
            } else {
                assert_eq!(state.pauses[sid].stop_hold, stop);
                assert_eq!(gates, 0, "a scoped message reopened the native gate");
            }
            store.with_state(|state| { state.pauses.remove(sid); }).unwrap();
        }
    }
}

/// Item 3: a message ends a pending quota continuation only when the owner
/// sent it.
#[test]
fn scoped_message_does_not_cancel_the_owners_quota_continuation() {
    use crate::management::v2::quota_continue;
    let _home_lock = crate::config::paths::test_home_env_lock();
    let instance = PrivateInstance::new();
    let ids = ["cutex.owner-power-quota-scoped", "cutex.owner-power-quota-owner", "cutex.owner-power-quota-root"];
    owner_power_sessions(&instance, &ids, crate::agent_bus::model::AgentRegistrationClass::Persistent);
    let callers = owner_power_callers(&ids);
    let store = quota_continue::Store::open_default().unwrap();
    for (n, (bearer, is_owner)) in callers.iter().enumerate() {
        let sid = ids[n];
        let now = chrono::Utc::now();
        assert!(store.with_state(|state| state.schedule(sid, now.timestamp() + 3600, None, Some("quota-turn".into()), 1, now)).unwrap());
        let (status, response) = owner_power_submit(started_submit_context(), sid, bearer, &format!("quota-{n}"), json!({}));
        assert_eq!(status, 200, "{response}");
        let pending = store.snapshot().unwrap().sessions.get(sid).and_then(|entry| entry.pending.clone());
        assert_eq!(pending.is_some(), !is_owner, "caller {n}: {response}");
    }
}

/// Item 4: only the owner addresses a hidden (unlisted) durable agent by
/// exact ID; a scoped user with a grant for it gets session_not_found.
#[test]
fn scoped_user_cannot_resolve_a_hidden_session() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let instance = PrivateInstance::new();
    let ids = ["cutex.owner-power-hidden-scoped", "cutex.owner-power-hidden-owner", "cutex.owner-power-hidden-root"];
    owner_power_sessions(&instance, &ids, crate::agent_bus::model::AgentRegistrationClass::LocalOnly);
    let callers = owner_power_callers(&ids);
    for (n, (bearer, is_owner)) in callers.iter().enumerate() {
        let (status, response) = owner_power_submit(started_submit_context(), ids[n], bearer, &format!("hidden-{n}"), json!({}));
        if *is_owner {
            assert_eq!(status, 200, "caller {n}: {response}");
            assert_eq!(response["cutex"]["result"]["disposition"], "started");
        } else {
            assert_eq!((status, any_code(&response)), (404, "session_not_found"), "caller {n}: {response}");
        }
    }
}

/// A shared-access user's explicit queue flush does not send items held by
/// the owner's queue pause; the owner and root still send past it.
#[test]
fn scoped_flush_does_not_send_items_held_by_the_owners_queue_pause() {
    use crate::management::v2::user_input::{self, UserInputOrigin, UserInputOriginKind, UserInputStrategy, UserInputSubmitParams};
    let _home_lock = crate::config::paths::test_home_env_lock();
    let instance = PrivateInstance::new();
    let ids = ["cutex.owner-power-flush-scoped", "cutex.owner-power-flush-owner", "cutex.owner-power-flush-root"];
    owner_power_sessions(&instance, &ids, crate::agent_bus::model::AgentRegistrationClass::Persistent);
    let callers = owner_power_callers(&ids);
    let queue = user_input::user_input_repository().unwrap();
    let mut ctx = p2_context();
    // "Sends" every queued item (no runtime in the fixture).
    ctx.flush_user_input_queue = |sid, _, explicit| {
        assert!(explicit, "an HTTP flush is explicit");
        let queue = user_input::user_input_repository().unwrap();
        let (_, items) = queue.list(sid).unwrap();
        for item in &items { queue.remove(sid, &item.queue_id, item.revision).unwrap().unwrap(); }
        Ok(items.len())
    };
    let flush = |sid: &str, bearer: &str, tag: &str| {
        let revision = queue.list(sid).unwrap().0;
        let body = json!({"requestId": format!("owner-power-flush-{tag}"), "method": "cutex/userInput/queue/flush",
            "params": {"expectedQueueRevision": revision}});
        let (status, _, bytes) = exchange_with(ctx, "POST", &format!("/v2/sessions/{sid}/cutex/requests"), Some(bearer),
            body.to_string().into_bytes(), "application/json", "");
        (status, serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null))
    };
    for (n, (bearer, is_owner)) in callers.iter().enumerate() {
        let sid = ids[n];
        queue.enqueue(sid, "owner-power-thread", &format!("owner-held-{n}"), &UserInputSubmitParams {
                interrupt_send: None,
            client_user_message_id: format!("owner-held-{n}"),
            origin: UserInputOrigin { kind: UserInputOriginKind::Other, client_id: "private-test".into() },
            strategy: UserInputStrategy::Queue, input: vec![json!({"type": "text", "text": "held"})],
            expected_turn_id: None, join_delegation: None }).unwrap();
        assert!(matches!(queue.pause(sid, "manual", None).unwrap(), user_input::QueuePauseDecision::Paused { .. }));
        let (status, response) = flush(sid, bearer, &format!("paused-{n}"));
        if *is_owner {
            assert_eq!(status, 200, "caller {n}: {response}");
            assert_eq!(response["cutex"]["result"]["affectedQueueIds"].as_array().unwrap().len(), 1, "{response}");
            assert!(queue.list(sid).unwrap().1.is_empty());
        } else {
            assert_eq!((status, any_code(&response)), (403, "owner_required"), "caller {n}: {response}");
            assert_eq!(response["error"]["details"]["reason"], "queue_paused", "{response}");
            assert_eq!(queue.list(sid).unwrap().1.len(), 1, "the held item was not sent");
            assert!(queue.pause_state(sid).unwrap().is_some(), "the owner's pause stays");
            // Without the owner's pause the shared-access flush still sends.
            queue.resume(sid).unwrap().unwrap();
            let (status, response) = flush(sid, bearer, &format!("resumed-{n}"));
            assert_eq!(status, 200, "{response}");
            assert_eq!(response["cutex"]["result"]["affectedQueueIds"].as_array().unwrap().len(), 1, "{response}");
            assert!(queue.list(sid).unwrap().1.is_empty());
        }
    }
}

// r290 P1
#[path = "access_project_roles_tests.rs"]
mod project_roles;

// r290 P6
#[test]
fn board_http_refuses_nonowner_before_loading_roster_or_runtime_data() {
    let _lock=crate::config::paths::test_home_env_lock();let _instance=PrivateInstance::new();
    assert_eq!(exchange("GET","/v2/board",None,&json!({})).0,401);
    let users=UserStore::open_default().unwrap();let tokens=ClientTokenStore::open_default().unwrap();
    let user=users.create_user("Board member",None,Grant::default(),"root").unwrap();let token=tokens.issue("board-member").unwrap();
    users.bind_token(&token.token_id,Some(&user.id),"root").unwrap();
    assert_eq!(exchange("GET","/v2/board",Some(&token.token),&json!({})).0,403);
    assert_eq!(exchange("POST","/v2/board",Some(ROOT),&json!({})).0,405);
}

// r290 P6: exercise HTTP headers and the cached read through the owner gate.
#[test]
fn board_http_owner_get_and_conditional_read_use_one_roster_snapshot() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static READS: AtomicUsize = AtomicUsize::new(0);
    let _lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    READS.store(0, Ordering::SeqCst);
    let mut ctx = p2_context();
    ctx.list_management_projects = |_| {
        READS.fetch_add(1, Ordering::SeqCst);
        Ok(crate::management::control_plane::HumanManagementProjectCollection {
            schema: crate::management::control_plane::HumanManagementProjectSchema::V1,
            projects: vec![], archived_projects: vec![], available_agents: vec![],
        })
    };
    let (status, headers, bytes) = exchange_with(ctx, "GET", "/v2/board", Some(ROOT), vec![], "application/json", "");
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    crate::management::v2::schema_export::assert_valid("board", "snapshot", &body);
    let etag = body["etag"].as_str().unwrap();
    assert!(headers.contains(&format!("ETag: {etag}")));
    let mut denied = context();
    // Every data loader panics: a fresh conditional request must touch no stores.
    denied.list_management_projects = |_| panic!("cached board reloaded projects");
    let (status, headers, bytes) = exchange_with(denied, "GET", "/v2/board", Some(ROOT), vec![], "application/json", &format!("If-None-Match: W/{etag}\r\n"));
    assert_eq!(status, 304, "{headers}");
    assert!(bytes.is_empty());
    assert_eq!(READS.load(Ordering::SeqCst), 1);
}

// r290 P10 (B6 / B3 / a) ------------------------------------------------------

/// B6: a full-scope owner device may turn things off on a remote listener;
/// widening needs a fresh single-use confirm bound to the token and action
/// and is announced to every owner device; narrowed / member / pending
/// tokens stay refused; host relay configuration stays owner-local.
#[test]
fn r290_p10_owner_device_turns_off_remotely_and_widens_only_after_confirm() {
    use crate::management::client_tokens::DeviceScope;
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    // These allowed mutations may refresh recovery state after preceding
    // tests; provide the fixture's empty registry rather than a deny-only callback.
    let exchange = |method: &str, path: &str, bearer: Option<&str>, body: &Value| {
        let (status, _, bytes) = exchange_with(p2_context(), method, path, bearer, body.to_string().into_bytes(), "application/json", "");
        (status, serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null))
    };
    let tokens = ClientTokenStore::open_default().unwrap();
    let users = UserStore::open_default().unwrap();
    users.snapshot().unwrap();
    let (phone, _) = tokens.issue_device("owner-phone", "Pixel", "android").unwrap();
    users.bind_token(&phone.token_id, Some(OWNER), "pairing-code").unwrap();
    let (tablet, _) = tokens.issue_device("owner-tablet", "Tab", "android").unwrap();
    users.bind_token(&tablet.token_id, Some(OWNER), "pairing-code").unwrap();
    let (chat, _) = tokens.issue_device_scoped("owner-chat", "Chat", "android", DeviceScope::Chat).unwrap();
    users.bind_token(&chat.token_id, Some(OWNER), "pairing-code").unwrap();
    let member = users.create_user("Member", None, Grant::default(), "root").unwrap();
    let (member_phone, _) = tokens.issue_device("member-phone", "M", "android").unwrap();
    users.bind_token(&member_phone.token_id, Some(&member.id), "root").unwrap();
    let (pending, _) = tokens.issue_device("pending", "P", "android").unwrap();
    let (status, setup) = exchange("POST", "/v2/relay/setup", Some(ROOT), &json!({"bind":"127.0.0.1:39491", "serverNames":["p10.test"]}));
    assert_eq!(status, 201, "{setup}");
    let repository = crate::management::v2::repository::management_v2_repository().unwrap();
    let checkpoint = repository.checkpoint().unwrap();
    struct Local;
    impl Drop for Local { fn drop(&mut self) { REMOTE.with(|r| r.set(false)); } }
    let _local = Local;
    REMOTE.with(|r| r.set(true));

    // Turning off: open to the full owner device on a remote listener.
    let off = json!({"peers": {"enabled": false}});
    let (status, value) = exchange("PATCH", "/v2/settings", Some(&phone.token), &off);
    assert_eq!(status, 200, "{value}");
    for (method, path, body) in [("PATCH", "/v2/peers/peer-1", json!({"enabled": false})), ("DELETE", "/v2/peers/peer-1", json!({}))] {
        let (_, value) = exchange(method, path, Some(&phone.token), &body);
        assert!(!["loopback_only", "owner_local_only", "owner_confirm_required"].contains(&any_code(&value)), "{method} {path}: {value}");
    }
    let revoke = format!("/v2/relay/certificates/{}", "a".repeat(64));
    assert_eq!(exchange_raw("DELETE", &revoke, Some(&phone.token), &json!({}), "").0, 204);
    let (status, value) = exchange("GET", "/v2/relay/certificates", Some(&phone.token), &json!({}));
    assert_eq!(status, 200, "{value}");
    // ... but not for narrowed, member or pending tokens.
    let (status, value) = exchange("PATCH", "/v2/settings", Some(&chat.token), &off);
    assert_eq!((status, code(&value)), (403, "device_scope_denied"), "{value}");
    assert_eq!(exchange_raw("DELETE", &revoke, Some(&chat.token), &json!({}), "").0, 403);
    for bearer in [&member_phone.token, &pending.token] {
        let (status, value) = exchange("PATCH", "/v2/settings", Some(bearer), &off);
        assert_eq!(status, 403, "{value}");
        assert_eq!(exchange_raw("DELETE", &revoke, Some(bearer), &json!({}), "").0, 403);
    }
    // Other remote writes keep their old rules.
    let (status, value) = exchange("PATCH", "/v2/settings", Some(&phone.token), &json!({"peers": {"enabled": false}, "managementCorsOrigins": []}));
    assert_eq!((status, code(&value)), (403, "loopback_only"), "{value}");
    let (_, value) = exchange("PATCH", "/v2/peers/peer-1", Some(&phone.token), &json!({"label": "x"}));
    assert_eq!(any_code(&value), "loopback_only", "{value}");
    let (status, value) = exchange("POST", "/v2/relay/tailnet", Some(&phone.token), &json!({"tailnetBind": null}));
    assert_eq!((status, code(&value)), (403, "owner_local_only"), "{value}");

    // Widening: 428 with a confirm token, then the confirmed repeat runs.
    let on = json!({"peers": {"enabled": true}});
    let (status, first) = exchange("PATCH", "/v2/settings", Some(&phone.token), &on);
    assert_eq!((status, code(&first)), (428, "owner_confirm_required"), "{first}");
    let details = &first["error"]["details"];
    assert_eq!(details["action"], "peersEnable");
    assert_eq!(details["ttlSeconds"], 120);
    assert!(details["summary"].as_str().is_some_and(|s| !s.is_empty()));
    let confirm = details["confirmToken"].as_str().unwrap().to_string();
    // Bound to the token and the action.
    let (status, value) = exchange("PATCH", &format!("/v2/settings?ownerConfirm={confirm}"), Some(&tablet.token), &on);
    assert_eq!((status, code(&value)), (428, "owner_confirm_required"), "{value}");
    let (status, value) = exchange("PATCH", &format!("/v2/users/settings?ownerConfirm={confirm}"), Some(&phone.token), &json!({"ownerRequiresApproval": false}));
    assert_eq!((status, code(&value)), (428, "owner_confirm_required"), "{value}");
    // The same action cannot consume a confirmation for a changed body or query.
    let changed = json!({"peers": {"enabled": true}, "archiveDeleteEmpty": false});
    let (status, value) = exchange("PATCH", &format!("/v2/settings?ownerConfirm={confirm}"), Some(&phone.token), &changed);
    assert_eq!((status, code(&value)), (428, "owner_confirm_required"), "{value}");
    let (status, value) = exchange("PATCH", &format!("/v2/settings?x=changed&ownerConfirm={confirm}"), Some(&phone.token), &on);
    assert_eq!((status, code(&value)), (428, "owner_confirm_required"), "{value}");
    let (status, value) = exchange("PATCH", &format!("/v2/settings?ownerConfirm={confirm}"), Some(&phone.token), &on);
    assert_eq!(status, 200, "{value}");
    // Single use.
    let (status, value) = exchange("PATCH", &format!("/v2/settings?ownerConfirm={confirm}"), Some(&phone.token), &on);
    assert_eq!((status, code(&value)), (428, "owner_confirm_required"), "{value}");
    let notices = host_events(&checkpoint, crate::management::users::OWNER_REMOTE_ACTION);
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!((&notices[0]["action"], &notices[0]["tokenId"], &notices[0]["connection"]), (&json!("peersEnable"), &json!(phone.token_id), &json!("remote")));
    assert_eq!(notices[0]["phase"], "confirmed");
    // Public pairing on: the same flow on the relay owner route.
    let (status, value) = exchange("POST", "/v2/relay/pairing", Some(&phone.token), &json!({"enabled": true}));
    assert_eq!((status, code(&value)), (428, "owner_confirm_required"), "{value}");
    assert_eq!(value["error"]["details"]["action"], "publicPairingOn");
    // Narrowed / member / pending devices never get a confirm.
    for bearer in [&chat.token, &member_phone.token, &pending.token] {
        let (status, value) = exchange("PATCH", "/v2/settings", Some(bearer), &on);
        assert_eq!(status, 403, "{value}");
    }
    REMOTE.with(|r| r.set(false));
    // Owner-local keeps today's behaviour: no confirm.
    let (status, value) = exchange("PATCH", "/v2/settings", Some(ROOT), &off);
    assert_eq!(status, 200, "{value}");
    let (status, value) = exchange("PATCH", "/v2/settings", Some(ROOT), &on);
    assert_eq!(status, 200, "{value}");
}

/// B6: the turning-off / widening lists are explicit and disjoint.
#[test]
fn r290_p10_owner_action_lists_are_explicit() {
    let off = crate::management::users::OWNER_TURN_OFF_ACTIONS;
    let widen = crate::management::users::OWNER_WIDENING_ACTIONS;
    assert!(off.iter().all(|a| !widen.contains(a)));
    for a in ["peerDisable", "peerRemove", "peersDisable", "publicPairingOff", "relayCertificateRevoke", "pairingInviteCancel"] {
        assert!(off.contains(&a), "{a}");
    }
    for a in ["pairingCode", "pairingInvite", "pairingInviteApprove", "relayCertificateApprove", "peerPair", "peerEnable", "peersEnable", "publicPairingOn", "disableOwnerApproval"] {
        assert!(widen.contains(&a), "{a}");
    }
    assert_eq!(crate::management::users::owner_confirm_param("/v2/settings?x=1&ownerConfirm=oc-abc"), Some("oc-abc"));
    assert_eq!(crate::management::users::owner_confirm_param("/v2/settings?ownerConfirm="), None);
    assert_eq!(crate::management::users::owner_confirm_param("/v2/settings"), None);
}

/// (a) Re-pairing with the old device token replaces it; another user's
/// token is never revoked by it.
#[test]
fn r290_p10_repair_with_previous_token_replaces_the_old_device() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let tokens = ClientTokenStore::open_default().unwrap();
    let users = UserStore::open_default().unwrap();
    users.snapshot().unwrap();
    let (old, _) = tokens.issue_device("phone", "Pixel", "android").unwrap();
    users.bind_token(&old.token_id, Some(OWNER), "pairing-code").unwrap();
    let member = users.create_user("Member", None, Grant::default(), "root").unwrap();
    let (other, _) = tokens.issue_device("member", "M", "android").unwrap();
    users.bind_token(&other.token_id, Some(&member.id), "root").unwrap();
    let pair = |previous: &str| {
        let (status, code_value) = exchange("POST", "/v2/pairing-codes", Some(ROOT), &json!({}));
        assert_eq!(status, 201, "{code_value}");
        let (status, paired) = exchange("POST", "/v2/pair", None, &json!({"code": code_value["code"], "deviceName": "Pixel", "platform": "android", "previousToken": previous}));
        assert_eq!(status, 200, "{paired}");
        crate::management::v2::schema_export::assert_valid("pair", "pairResponse", &paired);
        paired
    };
    let ids = || tokens.list().unwrap().into_iter().map(|t| t.token_id).collect::<Vec<_>>();
    // Another user's token: kept, nothing replaced.
    let paired = pair(&other.token);
    assert_eq!(paired["replacedTokenId"], Value::Null);
    assert!(ids().contains(&other.token_id));
    // The device's own previous token: revoked, the new one works.
    let paired = pair(&old.token);
    assert_eq!(paired["replacedTokenId"], json!(old.token_id));
    assert!(!ids().contains(&old.token_id));
    assert!(ids().contains(&paired["device"]["tokenId"].as_str().unwrap().to_string()));
    assert_eq!(exchange("GET", "/v2/users/me", Some(&old.token), &json!({})).0, 401);
    assert_eq!(exchange("GET", "/v2/users/me", Some(paired["token"].as_str().unwrap()), &json!({})).0, 200);
}

/// B3: device rows carry the relay certificate expiry, renewable-from and
/// grace end.
#[test]
fn r290_p10_device_rows_show_relay_certificate_expiry() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let tokens = ClientTokenStore::open_default().unwrap();
    let users = UserStore::beside(&tokens);
    let (device, _) = tokens.issue_device("phone", "phone", "android").unwrap();
    users.bind_token(&device.token_id, Some(OWNER), "root").unwrap();
    let (status, value) = exchange("GET", "/v2/users", Some(ROOT), &json!({}));
    assert_eq!(status, 200, "{value}");
    assert!(value["tokens"].as_array().unwrap().iter().all(|t| t["relayCertificate"].is_null()), "{value}");
    let (status, value) = exchange("POST", "/v2/relay/setup", Some(ROOT), &json!({"bind":"127.0.0.1:39492", "serverNames":["rows.test"]}));
    assert_eq!(status, 201, "{value}");
    let key = rcgen::KeyPair::generate().unwrap();
    let csr = rcgen::CertificateParams::default().serialize_request(&key).unwrap().pem().unwrap();
    let (_, enrollment) = exchange("POST", "/v2/relay-enrollment", Some(&device.token), &json!({"csrPem": csr}));
    let (status, issued) = exchange("POST", "/v2/relay/certificates", Some(ROOT), &json!({"enrollmentId":enrollment["enrollmentId"],"csrSha256":enrollment["csrSha256"],"keySha256":enrollment["keySha256"],"confirm":true}));
    assert_eq!(status, 201, "{issued}");
    for path in ["/v2/users", "/v2/client-tokens"] {
        let (status, value) = exchange("GET", path, Some(ROOT), &json!({}));
        assert_eq!(status, 200, "{value}");
        if path == "/v2/users" {
            crate::management::v2::schema_export::assert_valid("users", "userList", &value);
        }
        let row = value["tokens"].as_array().unwrap().iter().find(|t| t["tokenId"] == device.token_id).unwrap().clone();
        let cert = &row["relayCertificate"];
        assert_eq!((&cert["certificateId"], &cert["expiresAt"], &cert["state"]), (&issued["certificateId"], &issued["expiresAt"], &json!("active")), "{row}");
        let expires = chrono::DateTime::parse_from_rfc3339(cert["expiresAt"].as_str().unwrap()).unwrap();
        let renewable = chrono::DateTime::parse_from_rfc3339(cert["renewableFrom"].as_str().unwrap()).unwrap();
        let grace = chrono::DateTime::parse_from_rfc3339(cert["graceUntil"].as_str().unwrap()).unwrap();
        assert_eq!(expires - renewable, chrono::Duration::days(15));
        assert_eq!(grace - expires, chrono::Duration::days(30));
    }
}
