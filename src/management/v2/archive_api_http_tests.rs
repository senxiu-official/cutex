//! Included by the existing real HTTP access harness in the shared patch.
use super::*;
use crate::agent_management::*;
use crate::management::v2::archive::api::{OwnerRequest, REVIEW, EXECUTE};

fn owner_handler(principal: &crate::management::control_plane::HumanManagementPrincipal, request: OwnerRequest)
    -> Result<Value, AgentManagementError> {
    struct Offline;
    impl AgentArchiveRuntime for Offline {
        fn prepare(&mut self, _: &crate::session::model::CutexSessionRecord, _: bool) -> anyhow::Result<()> { Ok(()) }
        fn stop_and_verify(&mut self, _: &crate::session::model::CutexSessionRecord) -> anyhow::Result<()> { Ok(()) }
        fn verify_offline(&mut self, _: &crate::session::model::CutexSessionRecord) -> anyhow::Result<()> { Ok(()) }
    }
    let provider=AgentManagementProvider::open_default().unwrap();
    let path=crate::session::store::cutex_sessions_path().unwrap();
    let result=match request {
        OwnerRequest::Review(request)=>serde_json::to_value(provider.review_permanent_agent_retirement(principal,&path,&request)?),
        OwnerRequest::Execute(request)=>serde_json::to_value(provider.permanently_retire_agent(principal,&path,&request,&mut Offline)?),
    };
    result.map_err(|_|AgentManagementError::InvalidStore)
}

#[test]
fn r264_owner_review_execute_http_rejects_nonowner_and_requires_bound_token() {
    let _home_lock=crate::config::paths::test_home_env_lock();
    let instance=PrivateInstance::new();
    let mut record=crate::session::model::CutexSessionRecord::new("cutex.archive-http".into(),Some("thread-archive".into()),
        crate::platform::host::current_host_name(),instance.root.to_string_lossy().into_owned(),None).unwrap();
    record.agent_enabled=true;
    record.registration_class=crate::agent_bus::model::AgentRegistrationClass::Persistent;
    record.formal_agent_name=Some("可粘贴的完整名字 🦅".into());
    let mut sessions=crate::session::model::CutexSessionStore::default();
    sessions.sessions.insert(record.cutex_session_id.clone(),record.clone());
    crate::session::store::save_cutex_session_store(&sessions).unwrap();
    let tokens=ClientTokenStore::open_default().unwrap();
    let users=UserStore::open_default().unwrap();
    let owner=tokens.issue("archive-owner").unwrap();
    users.bind_token(&owner.token_id,Some(OWNER),"root").unwrap();
    let member=users.create_user("archive-member",None,Grant::default(),"root").unwrap();
    let token=tokens.issue("archive-member").unwrap();
    users.bind_token(&token.token_id,Some(&member.id),"root").unwrap();
    let call=|path:&str,bearer:Option<&str>,body:Value| {
        let mut ctx=context(); ctx.owner_archive_action=owner_handler;
        let (status,_,bytes)=exchange_with(ctx,"POST",path,bearer,body.to_string().into_bytes(),"application/json","");
        (status,serde_json::from_slice::<Value>(&bytes).unwrap())
    };
    for path in [REVIEW,EXECUTE] {
        assert_eq!(call(path,None,json!({})).0,401);
        assert_eq!(call(path,Some(&token.token),json!({"confirmed_name":"可粘贴的完整名字 🦅"})).0,403);
    }
    let (status,review)=call(REVIEW,Some(&owner.token),json!({"cutex_session_id":record.cutex_session_id}));
    assert_eq!(status,200,"{review}");
    let mut request=json!({"action_id":"retire-http","cutex_session_id":record.cutex_session_id,
        "confirmed_name":"wrong","confirmation_token":review["confirmation_token"],"expected_revision":review["revision"]});
    assert_eq!(call(EXECUTE,Some(&owner.token),request.clone()).0,409);
    request["confirmed_name"]=review["formal_name"].clone();
    let (status,receipt)=call(EXECUTE,Some(&owner.token),request.clone());
    assert_eq!(status,200,"{receipt}");
    assert_eq!(receipt["archiveKind"],"permanent");
    assert_eq!(call(EXECUTE,Some(&owner.token),request.clone()),(200,receipt));
    request["action_id"]=json!("another-action");
    assert_eq!(call(EXECUTE,Some(&owner.token),request).0,409);
    // Simulate the authority commit surviving while the session projection is still active.
    let mut crashed=crate::session::store::load_cutex_session_store().unwrap();
    crashed.sessions.insert(record.cutex_session_id.clone(),record.clone());
    crashed.agent_archive_receipts.clear();
    crate::session::store::save_cutex_session_store(&crashed).unwrap();
    let provider=AgentManagementProvider::open_default().unwrap();
    let id=crate::role_revision::CutexSessionId::new(record.cutex_session_id.clone()).unwrap();
    let start_error=provider.review_stock_runtime(&crate::session::store::cutex_sessions_path().unwrap(),&id,false).unwrap_err().to_string();
    assert!(start_error.contains("permanent_retirement_is_not_runnable"),"{start_error}");
    let state=AgentManagementStore::open_default().unwrap().snapshot().unwrap();
    assert!(crate::agent_management::ensure_agent_not_permanently_retired_in(&state,&record).is_err());
    assert!(crate::agent_management::agent_execution_blocked(&record));
    assert!(crate::session::archive::validate_restore_preconditions(&record,record.durable_revision()).is_err());
    let projected=crate::management::v2::archive::projection::fields(&record,Some(&state),Some(&sessions));
    assert_eq!(projected["archiveKind"],"permanent");
    assert_eq!(projected["allowedActions"],json!([]));
}
