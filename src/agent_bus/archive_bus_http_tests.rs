// Included by server::tests: real authenticated Bus routing, offline fixtures.
use super::*;
use crate::agent_management::*;
use crate::agent_bus::mcp::archive_mcp;

fn managed(invocation: &AgentManagementInvocation, request: &ManagedAgentArchiveRequest) -> Result<AgentArchiveReceipt,AgentManagementError> {
    struct Offline;
    impl AgentArchiveRuntime for Offline {
        fn prepare(&mut self,_:&CutexSessionRecord,_:bool)->anyhow::Result<()> {Ok(())}
        fn stop_and_verify(&mut self,_:&CutexSessionRecord)->anyhow::Result<()> {Ok(())}
        fn verify_offline(&mut self,_:&CutexSessionRecord)->anyhow::Result<()> {Ok(())}
    }
    AgentManagementProvider::open_default().unwrap().execute_managed_archive(invocation,request,
        &crate::session::store::cutex_sessions_path().unwrap(),&mut Offline)
}

#[test]
fn r264_mcp_bus_soft_archive_restore_and_permanent_denial() {
    use crate::config::instance::{Selection,set_test_selection};
    let _home_lock=crate::config::paths::test_home_env_lock();
    let previous=crate::config::instance::selected().unwrap();
    let path=std::env::temp_dir().join(format!("r264-archive-bus-{}",uuid::Uuid::new_v4()));
    set_test_selection(Selection::Custom(path.clone()));
    let caller="cutex.archive-caller";let runtime="archive-runtime";let thread="018e0000-0000-7000-8000-0000b0009ae4";
    let mut caller_record=active_route_session(caller,thread,runtime,1,7);
    caller_record.registration_class=AgentRegistrationClass::Persistent;
    let target="cutex.archive-target";
    let mut target_record=CutexSessionRecord::new(target.into(),Some("native-target".into()),current_host_name(),path.to_string_lossy().into_owned(),None).unwrap();
    target_record.agent_enabled=true;target_record.registration_class=AgentRegistrationClass::Persistent;target_record.formal_agent_name=Some("Target".into());
    let mut sessions=CutexSessionStore::default();sessions.sessions.insert(caller.into(),caller_record);sessions.sessions.insert(target.into(),target_record.clone());
    crate::session::store::save_cutex_session_store(&sessions).unwrap();
    let store=AgentManagementStore::open_default().unwrap();
    store.with_state(true,|mut snapshot| {
        let project=ProjectId::new("archive-project").unwrap();
        let authority:ProjectAuthority=serde_json::from_value(serde_json::json!({"project_id":project,"authorized_director_session":caller,
            "authority_epoch":1,"updated_at":"2026-10-26T00:00:00Z"})).unwrap();
        snapshot.projects.insert(project.clone(),authority);
        let agent:ManagedAgentRecord=serde_json::from_value(serde_json::json!({"project_id":project,"created_by_director_session":caller,
            "cutex_session_id":target,"native_session_id":"native-target","created_at":"2026-10-26T00:00:00Z",
            "spec":{"name":"Target","cwd":path.to_string_lossy(),"profile":"fixture","runtime_backend":"cute-alden","model":"model","reasoning":"medium",
            "permissions":"workspace-write","approval_policy":"never","sandbox_mode":"workspace-write","groups":[]}})).unwrap();
        snapshot.agents.insert(agent.cutex_session_id.clone(),agent);
        Ok((snapshot,(),true))
    }).unwrap();
    let state=route_state();let mut roster=active_route_roster(runtime,thread);roster.registration_class=AgentRegistrationClass::Persistent;
    state.lock().unwrap().agents.insert(runtime.into(),roster);
    let credential=crate::agent_bus::agent_auth::agent_credential("route-token",runtime,caller);
    let call=|tool:&str,action:&str,generation:u64| {
        archive_mcp::invoke(tool,serde_json::json!({"action_id":action,"project_id":"archive-project","cutex_session_id":target}),|body| {
            let body=body.to_string();
            let raw=format!("POST {} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {credential}\r\nX-Cutex-Agent-Id: {runtime}\r\nX-Cutex-Mcp-Thread-Id: {thread}\r\nX-Cutex-Mcp-Generation: {generation}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",archive_mcp::PATH,body.len());
            let mut handlers=route_test_handlers();handlers.managed_archive=managed;
            let response=invoke_route_with_handlers(raw.into_bytes(),Arc::clone(&state),handlers);
            if !response.starts_with(b"HTTP/1.1 200 ") { anyhow::bail!("fence rejected"); }
            Ok(http_json(&response))
        })
    };
    assert_eq!(call("agent_archive","stale",6)["ok"],false);
    let archived=call("agent_archive","soft",7);assert_eq!(archived["ok"],true,"{archived}");
    assert_eq!(archived["receipt"]["archiveKind"],"soft");
    assert_eq!(call("agent_archive","soft",7),archived);
    let restored=call("agent_restore","restore",7);assert_eq!(restored["ok"],true,"{restored}");
    let saved=crate::session::store::load_cutex_session_store().unwrap();
    assert!(saved.sessions[target].is_active());assert_eq!(saved.sessions[target].app_server_thread_id(),Some("native-target"));
    assert!(saved.sessions[target].current_runtime_agent_id.is_none());
    let invocation=AgentManagementInvocation {caller_cutex_session:crate::role_revision::CutexSessionId::new(caller).unwrap(),caller_runtime_agent_id:runtime.into()};
    let request=ManagedAgentArchiveRequest {action_id:AgentActionId::new("permanent-denied").unwrap(),project_id:Some(ProjectId::new("archive-project").unwrap()),
        cutex_session_id:crate::role_revision::CutexSessionId::new(target).unwrap(),operation:AgentArchiveOperation::PermanentRetire};
    assert!(matches!(managed(&invocation,&request),Err(AgentManagementError::Unauthorized)));
    assert!(store.snapshot().unwrap().agents[&request.cutex_session_id].retired_at.is_none());
    set_test_selection(previous);
}
