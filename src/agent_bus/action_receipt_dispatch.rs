//! Production work run by the Bus-owned worker after revalidating the MCP
//! occurrence. No transport, secondary queue, or new authority domain.
use std::sync::{Arc, Mutex};
use serde_json::{json, Value};
use crate::agent_bus::{server::AgentBusRequestHandlers, store::AgentBusState};
use crate::agent_management::{AgentManagementInvocation, AgentManagementRequest, SparseAgentManagementRequest};

pub fn is_create(payload: &AgentManagementRequest, sparse: Option<&SparseAgentManagementRequest>) -> bool {
    sparse.map_or_else(|| matches!(payload.operation,crate::agent_management::AgentOperation::Create { .. }),
        |request| matches!(request.operation,crate::agent_management::SparseOperation::Create { .. }))
}

pub fn create(
    state: &Arc<Mutex<AgentBusState>>, invocation: AgentManagementInvocation,
    payload: AgentManagementRequest, sparse: Option<SparseAgentManagementRequest>,
    handlers: AgentBusRequestHandlers, facade: u32,
    launch_policy: impl FnOnce(&AgentManagementInvocation, &AgentManagementRequest) -> Option<Value>,
) -> anyhow::Result<Value> {
    let (payload, sparse_digest) = if let Some(sparse) = &sparse {
        match crate::agent_management::admit(&invocation.caller_cutex_session,sparse) {
            Ok(admitted) => (admitted.request,Some(admitted.sparse_sha256)),
            Err(refusal) => return Ok(json!({"schema":"cutex/agent-management/v1","action_id":payload.action_id,
                "outcome":{"status":"no_write","code":refusal.code,"detail":refusal.detail}})),
        }
    } else { (payload,None) };
    if let Some(refusal) = launch_policy(&invocation,&payload) { return Ok(refusal); }
    let delegated = sparse.as_ref().and_then(|s| crate::agent_management::delegated_task(s,&payload)).map(str::to_owned);
    let full_digest = crate::agent_management::full_sha256(&payload);
    let caller = invocation.caller_cutex_session.clone();
    let action_id = payload.action_id.clone();
    let mut response = (handlers.agent_management)(state,invocation,payload)?;
    if let Some(task) = delegated {
        crate::asks::create_task::attach(&mut response,caller.as_str(),&task,action_id.as_str(),facade);
    }
    crate::management::v2::profile_auto_pick::attach_to_reply(&mut response,action_id.as_str());
    if let Some(sparse_digest) = sparse_digest {
        if let Some(digest) = response.pointer_mut("/outcome/receipt/request_sha256") {
            if full_digest.as_ref().is_some_and(|full| digest.as_str() == Some(full.as_str())) { *digest=json!(sparse_digest.as_str()); }
        }
    }
    // Normalize even legacy/provider cached attachment responses before storing a full reply.
    crate::asks::create_task::normalize_uncertain_attachment(&mut response,action_id.as_str());
    super::persist_creation_reply(caller.as_str(),action_id.as_str(),
        full_digest.as_ref().map(|d|d.as_str()),response)
}

pub fn delegate(caller: &str, body: &[u8], facade: u32) -> anyhow::Result<Value> {
    // agent_delegate does not use send/wake hooks; reject any accidental use
    // rather than expanding this async route to additional mutating tools.
    let payload: Value = serde_json::from_slice(body)?;
    anyhow::ensure!(payload["tool"] == "agent_delegate", "async tool is not agent_delegate");
    let store = crate::asks::store::AskStore::open_default()?;
    let world = crate::asks::scheduler::HostWorld::load();
    let send = |_: &crate::asks::tools::TellSend<'_>| anyhow::bail!("unexpected send in agent_delegate");
    let wake = |_: &str, _: &str, _: &str| None;
    let clock = chrono::Utc::now;
    let sleep = || std::thread::sleep(std::time::Duration::from_millis(250));
    let ctx = crate::asks::tools::ToolContext { store:&store,world:&world,caller,now:clock(),
        send:&send,wake_chain:&wake,clock:&clock,sleep:&sleep,
        wait:crate::agent_bus::tool_policy::delegation_wait_enabled(caller),facade };
    Ok(crate::asks::tools::handle(&ctx,body))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sparse_create_is_selected_before_placeholder_resolution() {
        let sparse: SparseAgentManagementRequest=serde_json::from_value(json!({
            "schema":"cutex/agent-management-sparse/v1","action_id":"create-sparse","operation":"create","agent":{"name":"fixture"}}
        )).unwrap();
        let placeholder=AgentManagementRequest {schema:crate::agent_management::AgentManagementSchema::V1,
            action_id:sparse.action_id.clone(),project_id:None,operation:crate::agent_management::AgentOperation::QueryManaged};
        assert!(is_create(&placeholder,Some(&sparse)));
        assert!(!is_create(&placeholder,None));
    }
}
