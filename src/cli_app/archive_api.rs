//! CLI adapters for the shared archive routes (same session input lane).
use cutex::agent_management::*;
use cutex::management::control_plane::HumanManagementPrincipal;
use cutex::management::v2::archive::api::OwnerRequest;

pub(crate) fn ensure_input_allowed(id: &str) -> anyhow::Result<()> {
    let sessions=cutex::session::store::load_cutex_session_store_shared()?;
    let key=cutex::session::service::cutex_session_key_for_user_id_including_retired(&sessions,id)
        .ok_or_else(||anyhow::anyhow!("archive_state_unavailable"))?;
    let record=sessions.sessions.get(&key).ok_or_else(||anyhow::anyhow!("archive_state_unavailable"))?;
    cutex::agent_management::ensure_agent_not_permanently_retired(record)?;
    anyhow::ensure!(!record.is_retired(),"restore_the_archived_agent_first");
    Ok(())
}

pub(crate) fn owner(principal: &HumanManagementPrincipal, request: OwnerRequest) -> Result<serde_json::Value, AgentManagementError> {
    match request {
        OwnerRequest::Review(request) => {
            let path = cutex::session::store::cutex_sessions_path().map_err(|_| AgentManagementError::PersistenceUnavailable)?;
            let result = super::super::management_context::management_agent_provider()?
                .review_permanent_agent_retirement(principal, &path, &request)?;
            serde_json::to_value(result).map_err(|_| AgentManagementError::InvalidStore)
        }
        OwnerRequest::Execute(request) => {
            let result = super::super::management_context::permanently_retire_agent(principal,&request)?;
            serde_json::to_value(result).map_err(|_| AgentManagementError::InvalidStore)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cutex::session::model::{CutexSessionRecord,CutexSessionStore};
    use cutex::management::v2::user_input::*;
    #[test]
    fn r264_permanent_active_projection_blocks_real_submit_and_steer_before_io() {
        let _home=crate::cli_app::test_home::IsolatedTestHome::new_private("r264-input-gate").unwrap();
        let mut record=CutexSessionRecord::new("cutex.input-gate".into(),Some("original-thread".into()),
            cutex::platform::host::current_host_name(),"cwd".into(),None).unwrap();
        record.agent_enabled=true;record.registration_class=cutex::agent_bus::model::AgentRegistrationClass::Persistent;
        record.formal_agent_name=Some("Input Gate".into());
        let mut sessions=CutexSessionStore::default();sessions.sessions.insert(record.cutex_session_id.clone(),record.clone());
        cutex::session::store::save_cutex_session_store(&sessions).unwrap();
        let provider=AgentManagementProvider::open_default().unwrap();
        let mut authority=provider.store().snapshot().unwrap();
        let receipt:AgentArchiveReceipt=serde_json::from_value(serde_json::json!({
            "request":{"action_id":"input-gate-retire","reason":null,"review":{"cutex_session_id":record.cutex_session_id,"formal_name":"Input Gate","operation":"permanent_retire","revision":record.durable_revision(),"runtime_generation":record.runtime_generation}},
            "request_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "stage":"permanent_retirement_committed","result":record,"error":null,"committed_at":"2026-10-26T00:00:00Z","archiveKind":"permanent","restorable":false
        })).unwrap();
        authority.agent_archive_audit.insert("input-gate-retire/permanent".into(),receipt);
        let authority_path=cutex::config::paths::runtime_dir().unwrap().join("agent-management/v1/agent-management-v1.json");
        std::fs::write(&authority_path,serde_json::to_vec(&authority).unwrap()).unwrap();
        assert!(ensure_input_allowed(&record.cutex_session_id).is_err(),"missing authority lock must fail closed");
        std::fs::write(authority_path.with_file_name("agent-management-v1.lock"),b"").unwrap();
        assert_eq!(provider.store().snapshot().unwrap().agent_archive_audit.len(),1);
        // The authoritative tombstone is durable while session projection remains active.
        let command=UserInputSubmitCommand { management_request_id:"input-gate".into(),cutex_session_id:record.cutex_session_id.clone(),thread_id:"original-thread".into(),
            params:UserInputSubmitParams {client_user_message_id:"input-gate-message".into(),origin:UserInputOrigin {kind:UserInputOriginKind::Backend,client_id:"test".into()},
                strategy:UserInputStrategy::Auto,input:vec![serde_json::json!({"type":"text","text":"must not send"})],expected_turn_id:None,join_delegation:None} };
        let submit=crate::cli_app::app_server_user_input::submit_v2(command.clone()).unwrap_err();
        assert_eq!(submit.code,"archive_blocks_input");assert!(!submit.outcome_unknown);
        let steer=crate::cli_app::app_server_user_input::steer_queued_v2(&command,false,&serde_json::Value::Null).unwrap_err();
        assert_eq!(steer.code,"archive_blocks_input");
        assert_eq!(cutex::session::store::load_cutex_session_store().unwrap().sessions[&record.cutex_session_id].app_server_thread_id(),Some("original-thread"));
    }
}