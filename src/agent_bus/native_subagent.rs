//! Verified native children have a separate read-only principal; never a parent role.
use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const PATH: &str = "/api/mcp/v1/native-child-read";
pub const PROOF_PATH: &str = "/v2/internal/native-subagent/verify";
pub const PROOF_METHOD: &str = "cutex/internal/nativeSubagentVerify";
pub const CHILD_HEADER: &str = "x-cutex-mcp-child-id";
pub const CHILD_ENV: &str = "CUTEX_MCP_CHILD_ID";
pub const REFUSAL: &str = "native subagents cannot manage or message cutex agents";
pub const TOOLS: &[&str] = &[
    "agent_list",
    "agent_name_history",
    "artifact_list",
    "artifact_read",
    "service_list",
    "delegation_read",
    "owner_pins",
];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChildPrincipal {
    pub parent_session_id: String,
    pub generation: u64,
    pub child_id: String,
}
impl ChildPrincipal {
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            crate::agent_bus::routing::is_full_durable_cutex_session_id(&self.parent_session_id)
                && self.generation > 0
                && valid_child(&self.child_id),
            "invalid native child principal"
        );
        Ok(())
    }
}
pub fn valid_child(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b':' | b'.'))
}
pub fn allowed(tool: &str) -> bool {
    TOOLS.contains(&tool)
}
pub fn refusal() -> Value {
    json!({"ok":false,"status":"no_write","code":"native_subagent_read_only","detail":REFUSAL})
}
pub fn filter_tools(mut tools: Value) -> Value {
    if let Some(rows) = tools["tools"].as_array_mut() {
        rows.retain(|row| row["name"].as_str().is_some_and(allowed));
    }
    tools
}

/// Fresh bounded ancestry, using only the current generation's native RPC.
/// No rollout reads or cached edges can authorize a child in a later generation.
pub fn native_ancestry(
    root: &str,
    child: &str,
    mut read: impl FnMut(&str) -> anyhow::Result<Value>,
) -> anyhow::Result<bool> {
    if child == root || !valid_child(child) {
        return Ok(false);
    }
    let mut current = child.to_owned();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..crate::management::v2::subagent_lineage::MAX_LINEAGE_DEPTH {
        if !seen.insert(current.clone()) {
            return Ok(false);
        }
        let thread = read(&current)?;
        if thread["id"].as_str() != Some(current.as_str()) {
            return Ok(false);
        }
        let direct = thread["parentThreadId"].as_str();
        let source = thread
            .pointer("/source/subAgent/thread_spawn/parent_thread_id")
            .and_then(Value::as_str);
        if matches!((direct,source),(Some(a),Some(b)) if a!=b) {
            return Ok(false);
        }
        let Some(parent) = direct.or(source).filter(|s| valid_child(s)) else {
            return Ok(false);
        };
        if parent == root {
            return Ok(true);
        }
        current = parent.to_owned();
    }
    Ok(false)
}

/// Bus-to-Management proof; root credential never leaves the daemon process.
pub fn verify_in_management(principal: &ChildPrincipal) -> anyhow::Result<()> {
    principal.validate()?;
    let config = crate::config::store::load_codez_config_checked()?;
    let token = crate::management::service::management_root_credential(&config, None)?;
    let url = format!(
        "http://127.0.0.1:{}{}",
        crate::management::service::management_port(&config),
        PROOF_PATH
    );
    let body = serde_json::to_vec(principal)?;
    let result = crate::http::client::http_json_request(crate::http::client::HttpJsonRequest {
        url: &url,
        method: "POST",
        token: Some(token),
        body: Some(&body),
        timeout: std::time::Duration::from_secs(15),
        invalid_url_context: "child proof endpoint",
        only_http_message: "child proof requires HTTP",
        missing_host_message: "child proof host",
        connect_context: "child proof unavailable",
        read_context: "child proof unavailable",
        non_success_prefix: "child proof refused",
        parse_context: "child proof response",
        ok_text_as_null: false,
    })?;
    ensure!(
        result["verified"] == true && result["principal"] == serde_json::to_value(principal)?,
        "native child ancestry is not verified"
    );
    Ok(())
}

pub fn validate_read(
    tool: &str,
    args: &Value,
    parent: &str,
    store: &crate::asks::store::AskStore,
) -> anyhow::Result<()> {
    ensure!(allowed(tool), "{REFUSAL}");
    if tool == "delegation_read" {
        let id = args["delegation"]
            .as_str()
            .context("delegation id required")?;
        let state = store.lookup_snapshot(&[id])?;
        ensure!(
            state
                .asks
                .get(id)
                .is_some_and(|ask| ask.requester_session() == Some(parent)),
            "native subagents may read only delegations made by their parent"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_native_children_require_actual_chain() {
        assert!(native_ancestry("root", "child", |id| Ok(
            json!({"id":id,"parentThreadId":if id=="child" {"middle"}else{"root"}})
        ))
        .unwrap());
        assert!(!native_ancestry("root", "forged", |_| Ok(
            json!({"id":"different","parentThreadId":"root"})
        ))
        .unwrap());
        assert!(!native_ancestry("root", "child", |id| Ok(
            json!({"id":id,"parentThreadId":id})
        ))
        .unwrap());
        assert!(!native_ancestry("root","child",|id|Ok(json!({"id":id,"parentThreadId":"root","source":{"subAgent":{"thread_spawn":{"parent_thread_id":"foreign"}}}}))).unwrap());
        assert!(!native_ancestry("root", "root", |_| panic!("parent is not a child")).unwrap());
    }
    #[test]
    fn source_subagent_chain_and_unknown_children() {
        assert!(native_ancestry("root", "child", |id| Ok(
            json!({"id":id,"source":{"subAgent":{"thread_spawn":{"parent_thread_id":"root"}}}})
        ))
        .unwrap());
        assert!(
            !native_ancestry("root", "foreign", |id| Ok(json!({"id":id,"source":"cli"}))).unwrap()
        );
    }
    #[test]
    fn child_toolset_contains_only_reads() {
        for name in [
            "agent_create",
            "agent_delegate",
            "agent_tell",
            "agent_pause",
            "agent_resume",
            "alarm_set",
            "owner_pin",
            "artifact_publish",
            "service_register",
            "browser_script",
            "workflow_start",
        ] {
            assert!(!allowed(name));
        }
        let input =
            json!({"tools":[{"name":"agent_list"},{"name":"agent_tell"},{"name":"service_list"}]});
        assert_eq!(
            filter_tools(input)["tools"],
            json!([{"name":"agent_list"},{"name":"service_list"}])
        );
        assert!(valid_child("a3f74c9"));
        assert!(!valid_child("../escape"));
    }
}

#[cfg(test)]
mod capacity_tests {
    #[test]
    fn native_child_proof_waits_do_not_occupy_control_capacity() {
        for path in [super::PATH, super::PROOF_PATH] {
            let request = crate::http::server::SimpleHttpRequest {
                method: "POST".into(),
                path: path.into(),
                headers: Default::default(),
                body: vec![],
            };
            assert_eq!(
                crate::management::v2::control_capacity::classify(&request),
                crate::management::v2::control_capacity::WorkClass::HeavyRead
            );
        }
    }
}
