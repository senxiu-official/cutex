//! Reversible MCP archive/restore only; no owner credentials or permanent enum.
use serde_json::{json, Value};
pub const PATH: &str = "/api/agent-management/v1/archive";
pub fn is_tool(name: &str) -> bool { matches!(name,"agent_archive"|"agent_restore") }
pub fn definitions() -> Vec<Value> {
    [("agent_archive","Soft archive a project member. Stops its runtime; it can be restored with the same session and native thread."),
     ("agent_restore","Restore a soft-archived project member on the same session and native thread, offline. Permanent retirement cannot be undone.")]
    .into_iter().map(|(name,description)| json!({"name":name,"description":description,
        "inputSchema":{"type":"object","additionalProperties":false,"required":["cutex_session_id"],
        "properties":{"cutex_session_id":{"type":"string"},"action_id":{"type":"string"},"project_id":{"type":"string"}}}})).collect()
}
pub fn invoke(name: &str, mut arguments: Value, transport: impl FnOnce(&Value)->anyhow::Result<Value>) -> Value {
    let action = arguments["action_id"].as_str().unwrap_or("").to_owned();
    let unknown=||json!({"ok":false,"action_id":action,"status":"response_uncertain","outcomeUnknown":true});
    let Some(object) = arguments.as_object_mut() else { return json!({"ok":false,"code":"invalid_arguments"}); };
    if !is_tool(name) || object.keys().any(|k| !matches!(k.as_str(),"action_id"|"cutex_session_id"|"project_id")) {
        return json!({"ok":false,"action_id":action,"code":"invalid_arguments","status":"no_write"});
    }
    object.insert("operation".into(),json!(if name=="agent_restore" {"restore"} else {"archive"}));
    match transport(&arguments) {
        Ok(value) if value["action_id"] == action && value["ok"].is_boolean() => value,
        Err(error) if error.downcast_ref::<crate::agent_bus::client::BusUnreachable>().is_some() =>
            json!({"ok":false,"action_id":action,"status":"no_write","code":"bus_unavailable"}),
        _ => unknown(),
    }
}

pub fn response(request: &crate::agent_management::ManagedAgentArchiveRequest,
    result: Result<crate::agent_management::AgentArchiveReceipt,crate::agent_management::AgentManagementError>) -> Value {
    match result {
        Ok(receipt) => {
            let committed=receipt.stage==crate::agent_management::AgentArchiveStage::Committed;
            json!({"ok":committed,"action_id":request.action_id,"status":if committed {"completed"} else {"response_uncertain"},
                "outcomeUnknown":!committed,"receipt":receipt})
        }
        Err(error @ (crate::agent_management::AgentManagementError::Unauthorized | crate::agent_management::AgentManagementError::InvalidRequest(_))) =>
            json!({"ok":false,"action_id":request.action_id,"status":"no_write","outcomeUnknown":false,"code":"archive_rejected","detail":error.to_string()}),
        Err(error) => json!({"ok":false,"action_id":request.action_id,"status":"response_uncertain",
            "outcomeUnknown":true,"code":"archive_rejected","detail":error.to_string()}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn archive_and_restore_cannot_supply_permanent_operation() {
        for name in ["agent_archive","agent_restore"] {
            let value=invoke(name,json!({"action_id":"x","cutex_session_id":"cutex.x"}),|body| {
                assert_eq!(body["operation"],if name=="agent_restore" {"restore"} else {"archive"});
                Ok(json!({"ok":true,"action_id":"x"}))
            });
            assert_eq!(value["ok"],true);
            assert_eq!(invoke(name,json!({"action_id":"x","operation":"permanent_retire"}), |_|panic!("transported"))["code"],"invalid_arguments");
        }
    }
}
