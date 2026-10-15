//! Root-only scoped MCP routes (DESIGN §4.11, gap 22):
//!
//! - `GET /v2/codex/managed-mcp` — Cutex built-in servers (`cutex`,
//!   `cutex_job`), when they are injected, approval mode and tool list.
//! - `GET|PUT /v2/projects/{projectId}/mcp` — project-layer servers.
//! - `GET|PUT /v2/sessions/{cutexSessionId}/mcp` — agent-layer servers plus
//!   `disabled` inherited names.
//! - `GET /v2/sessions/{cutexSessionId}/mcp/effective` — merged view.
//!
//! Storage, validation, redaction and launch rendering live in
//! `crate::launch::scoped_mcp`.

use std::collections::BTreeMap;
use std::net::TcpStream;

use serde::Deserialize;
use serde_json::json;
use serde_json::Value;

use crate::http::server::write_json_response;
use crate::http::server::SimpleHttpRequest;
use crate::launch::scoped_mcp as core;
use crate::launch::scoped_mcp::Scope;

pub const MANAGED_MCP_PATH: &str = "/v2/codex/managed-mcp";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopedMcpRoute {
    ManagedMcp,
    Project(String),
    Session(String),
    SessionEffective(String),
}

fn percent_decode(encoded: &str) -> Option<String> {
    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = encoded.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    let text = String::from_utf8(out).ok()?;
    (!text.is_empty() && !text.contains('/') && !text.chars().any(char::is_control)).then_some(text)
}

pub fn parse_path(path: &str) -> Option<ScopedMcpRoute> {
    if path == MANAGED_MCP_PATH {
        return Some(ScopedMcpRoute::ManagedMcp);
    }
    if let Some(rest) = path.strip_prefix("/v2/projects/") {
        let encoded = rest.strip_suffix("/mcp")?;
        if encoded.contains('/') {
            return None;
        }
        return percent_decode(encoded).map(ScopedMcpRoute::Project);
    }
    let rest = path.strip_prefix("/v2/sessions/")?;
    if let Some(encoded) = rest.strip_suffix("/mcp/effective") {
        if encoded.contains('/') {
            return None;
        }
        return percent_decode(encoded).map(ScopedMcpRoute::SessionEffective);
    }
    let encoded = rest.strip_suffix("/mcp")?;
    if encoded.contains('/') {
        return None;
    }
    percent_decode(encoded).map(ScopedMcpRoute::Session)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScopePutBody {
    pub servers: BTreeMap<String, Value>,
    #[serde(default)]
    pub disabled: Option<Vec<String>>,
    #[serde(default)]
    pub expected_revision: Option<u64>,
}

pub fn scope_view(scope: Scope, id: &str, entry: &core::ScopeEntry) -> Value {
    let mut body = json!({
        "contractVersion": 2,
        "scope": scope.as_str(),
        "id": id,
        "revision": entry.revision,
        "updatedAt": entry.updated_at,
        "servers": core::redact_servers(&entry.servers),
    });
    if scope == Scope::Session {
        body["disabled"] = json!(entry.disabled);
    }
    body
}

/// Static description of the Cutex built-ins, with the daemon's own
/// `cutex` tool catalog (the facade serves the same `agent_bus::mcp`).
pub fn managed_mcp_resource(job_configured: bool, facade: Option<String>) -> Value {
    let tools: Vec<Value> = crate::agent_bus::mcp::tools()["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|tool| json!({ "name": tool["name"], "description": tool["description"] }))
        .collect();
    json!({
        "contractVersion": 2,
        "servers": [
            {
                "name": "cutex",
                "description": "Cutex Agent Bus and management tools (messages, tasks, alarms, managed Agent queries).",
                "injected": "every managed (stock/explicit runtime) agent launch",
                "enabledWhen": "the agent is not an owned local-only session (no Agent Bus identity)",
                "approvalMode": "approve",
                "codeModeDirectOnly": true,
                "command": facade,
                "envVars": ["CUTEX_AGENT_ID", "CUTEX_RUNTIME_GENERATION", "CUTEX_AGENT_BUS_URL", "CUTEX_AGENT_BUS_TOKEN"],
                "toolsSource": "daemon",
                "tools": tools,
            },
            {
                "name": "cutex_job",
                "description": "Cutex Job Service tools (sandboxed jobs through the Job adapter).",
                "injected": "managed agent launches when the selected runtime deployment has a Job MCP descriptor",
                "configured": job_configured,
                "enabledWhen": "configured; ordinary (non-managed) sessions get it disabled",
                "approvalMode": "auto",
                "codeModeDirectOnly": false,
                "envVars": ["CUTEX_AGENT_ID", "CUTEX_AGENT_BUS_URL", "CUTEX_AGENT_BUS_TOKEN"],
                "toolsSource": "external",
                "tools": Value::Null,
            }
        ],
        "reservedNames": core::BUILTIN_SERVER_NAMES,
    })
}

fn selected_runtime() -> (bool, Option<String>, Option<std::path::PathBuf>) {
    match crate::launch::local_deployment::LocalDeployment::selected() {
        Ok(Some(deployment)) => {
            let facade = std::fs::read(&deployment.bundle_manifest)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<crate::launch::stock::StockBundle>(&bytes).ok())
                .map(|bundle| display_path(&bundle.facade.path));
            (deployment.job_mcp.is_some(), facade, Some(deployment.native_home))
        }
        _ => (false, None, crate::config::paths::host_codex_home_dir().ok()),
    }
}

fn display_path(path: &std::path::Path) -> String {
    let text = path.to_string_lossy();
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_string()
}

fn project_exists(project_id: &str) -> anyhow::Result<bool> {
    let Ok(id) = crate::agent_management::ProjectId::new(project_id.to_string()) else {
        return Ok(false);
    };
    let store = crate::agent_management::AgentManagementStore::open_default()?;
    let snapshot = store.snapshot().map_err(anyhow::Error::new)?;
    Ok(crate::agent_management::project_exists(&snapshot, &id))
}

fn session_record(
    cutex_session_id: &str,
) -> anyhow::Result<Option<crate::session::model::CutexSessionRecord>> {
    let store = crate::session::store::load_cutex_session_store()?;
    Ok(store.sessions.get(cutex_session_id).cloned())
}

fn not_found(stream: &mut TcpStream, code: &str, message: &str, details: Value) -> anyhow::Result<()> {
    super::server::write_v2_error(stream, 404, "Not Found", code, message, false, details)
}

fn store_unavailable(stream: &mut TcpStream, error: &anyhow::Error) -> anyhow::Result<()> {
    super::server::write_v2_error(
        stream,
        503,
        "Service Unavailable",
        "scoped_mcp_store_unavailable",
        "the scoped MCP store could not be read or written",
        true,
        json!({ "diagnostic": format!("{error:#}") }),
    )
}

pub(super) fn handle(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    route: ScopedMcpRoute,
) -> anyhow::Result<()> {
    let method = request.method.as_str();
    let method_not_allowed = |stream: &mut TcpStream, allowed: &str| {
        super::server::write_v2_error(
            stream,
            405,
            "Method Not Allowed",
            "method_not_allowed",
            &format!("use {allowed}"),
            false,
            json!({}),
        )
    };
    match route {
        ScopedMcpRoute::ManagedMcp => {
            if method != "GET" {
                return method_not_allowed(stream, "GET");
            }
            let (job, facade, _) = selected_runtime();
            write_json_response(stream, 200, "OK", &managed_mcp_resource(job, facade))
        }
        ScopedMcpRoute::Project(project_id) => {
            match project_exists(&project_id) {
                Ok(true) => {}
                Ok(false) => {
                    return not_found(
                        stream,
                        "project_not_found",
                        "no such cutex project",
                        json!({ "projectId": project_id }),
                    )
                }
                Err(error) => return store_unavailable(stream, &error),
            }
            scope_route(stream, request, Scope::Project, &project_id, method_not_allowed)
        }
        ScopedMcpRoute::Session(session_id) => {
            match session_record(&session_id) {
                Ok(Some(_)) => {}
                Ok(None) => {
                    return not_found(
                        stream,
                        "session_not_found",
                        "no such cutex session on this host",
                        json!({ "cutexSessionId": session_id }),
                    )
                }
                Err(error) => return store_unavailable(stream, &error),
            }
            scope_route(stream, request, Scope::Session, &session_id, method_not_allowed)
        }
        ScopedMcpRoute::SessionEffective(session_id) => {
            if method != "GET" {
                return method_not_allowed(stream, "GET");
            }
            let record = match session_record(&session_id) {
                Ok(Some(record)) => record,
                Ok(None) => {
                    return not_found(
                        stream,
                        "session_not_found",
                        "no such cutex session on this host",
                        json!({ "cutexSessionId": session_id }),
                    )
                }
                Err(error) => return store_unavailable(stream, &error),
            };
            let result = (|| -> anyhow::Result<Value> {
                let store = core::load_store()?;
                let project_id = core::session_project_id(&session_id)?;
                let (job, _, home) = selected_runtime();
                let global = match &home {
                    Some(home) => core::global_servers(home)?,
                    None => BTreeMap::new(),
                };
                let builtins = [
                    core::BuiltinServer { name: "cutex", enabled: crate::agent_bus::tool_policy::cutex_mcp_enabled(&record) },
                    core::BuiltinServer { name: "cutex_job", enabled: job },
                ];
                let servers = core::effective_servers(
                    &global,
                    project_id.as_ref().and_then(|project| store.projects.get(project)),
                    store.sessions.get(&session_id),
                    &builtins,
                );
                Ok(json!({
                    "contractVersion": 2,
                    "cutexSessionId": session_id,
                    "projectId": project_id,
                    "codexHome": home.as_deref().map(display_path),
                    "appliesOn": "next launch",
                    "servers": servers,
                }))
            })();
            match result {
                Ok(body) => write_json_response(stream, 200, "OK", &body),
                Err(error) => store_unavailable(stream, &error),
            }
        }
    }
}

fn scope_route(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    scope: Scope,
    id: &str,
    method_not_allowed: impl Fn(&mut TcpStream, &str) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    match request.method.as_str() {
        "GET" => match core::load_store() {
            Ok(store) => {
                let map = match scope {
                    Scope::Project => &store.projects,
                    Scope::Session => &store.sessions,
                };
                let entry = map.get(id).cloned().unwrap_or_default();
                write_json_response(stream, 200, "OK", &scope_view(scope, id, &entry))
            }
            Err(error) => store_unavailable(stream, &error),
        },
        "PUT" => {
            let body: ScopePutBody = match serde_json::from_slice(&request.body) {
                Ok(body) => body,
                Err(error) => {
                    return super::server::write_v2_error(
                        stream,
                        400,
                        "Bad Request",
                        "invalid_request",
                        &format!("body must be {{servers, disabled?, expectedRevision?}}: {error}"),
                        false,
                        json!({}),
                    )
                }
            };
            if scope == Scope::Project && body.disabled.as_ref().is_some_and(|d| !d.is_empty()) {
                return super::server::write_v2_error(
                    stream,
                    400,
                    "Bad Request",
                    "invalid_request",
                    "disabled is only supported on the session (agent) scope",
                    false,
                    json!({}),
                );
            }
            match core::replace_scope(
                scope,
                id,
                body.servers,
                body.disabled.unwrap_or_default(),
                body.expected_revision,
            ) {
                Ok(entry) => write_json_response(stream, 200, "OK", &scope_view(scope, id, &entry)),
                Err(core::UpdateError::Invalid(message)) => super::server::write_v2_error(
                    stream,
                    400,
                    "Bad Request",
                    "invalid_mcp_config",
                    &message,
                    false,
                    json!({}),
                ),
                Err(core::UpdateError::RevisionConflict { expected, actual }) => {
                    super::server::write_v2_error(
                        stream,
                        409,
                        "Conflict",
                        "revision_conflict",
                        "the scoped MCP config changed; reload and retry",
                        false,
                        json!({ "expectedRevision": expected, "currentRevision": actual }),
                    )
                }
                Err(core::UpdateError::Store(error)) => store_unavailable(stream, &error),
            }
        }
        _ => method_not_allowed(stream, "GET or PUT"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_parse_and_reject_traversal() {
        assert_eq!(parse_path("/v2/codex/managed-mcp"), Some(ScopedMcpRoute::ManagedMcp));
        assert_eq!(parse_path("/v2/projects/p1/mcp"), Some(ScopedMcpRoute::Project("p1".into())));
        assert_eq!(
            parse_path("/v2/sessions/cutex.a%2Bb/mcp"),
            Some(ScopedMcpRoute::Session("cutex.a+b".into()))
        );
        assert_eq!(
            parse_path("/v2/sessions/cutex.a/mcp/effective"),
            Some(ScopedMcpRoute::SessionEffective("cutex.a".into()))
        );
        for bad in [
            "/v2/projects//mcp",
            "/v2/projects/a/b/mcp",
            "/v2/projects/a%2Fb/mcp",
            "/v2/sessions/a/b/mcp",
            "/v2/sessions/a/mcpx",
            "/v2/sessions/a",
            "/v2/sessions/%zz/mcp",
        ] {
            assert_eq!(parse_path(bad), None, "{bad}");
        }
    }

    #[test]
    fn put_body_is_strict_and_view_redacts() {
        let body: ScopePutBody = serde_json::from_value(json!({
            "servers": { "gh": { "command": "x" } }, "disabled": ["a"], "expectedRevision": 3
        }))
        .unwrap();
        assert_eq!(body.expected_revision, Some(3));
        assert!(serde_json::from_value::<ScopePutBody>(json!({ "servers": {}, "profile": 1 })).is_err());
        assert!(serde_json::from_value::<ScopePutBody>(json!({ "disabled": [] })).is_err());

        let entry = core::ScopeEntry {
            revision: 2,
            servers: [("gh".to_string(), json!({ "command": "x", "env": { "T": "s" } }))].into(),
            disabled: vec!["g".into()],
            ..core::ScopeEntry::default()
        };
        let view = scope_view(Scope::Session, "cutex.a", &entry);
        assert_eq!(view["servers"]["gh"]["env"]["T"], core::REDACTED);
        assert_eq!(view["disabled"], json!(["g"]));
        assert_eq!(view["revision"], 2);
        let project = scope_view(Scope::Project, "p", &core::ScopeEntry::default());
        assert!(project.get("disabled").is_none());
        assert_eq!(project["revision"], 0);
    }

    #[test]
    fn managed_mcp_lists_cutex_tools() {
        let value = managed_mcp_resource(false, None);
        let cutex = &value["servers"][0];
        assert_eq!(cutex["name"], "cutex");
        assert_eq!(cutex["approvalMode"], "approve");
        let tools = cutex["tools"].as_array().unwrap();
        assert!(tools.iter().any(|tool| tool["name"] == "alarm_set"));
        assert!(tools.iter().all(|tool| tool["description"].is_string()
            && tool.as_object().unwrap().len() == 2));
        assert_eq!(value["servers"][1]["configured"], false);
        assert_eq!(value["reservedNames"], json!(["cutex", "cutex_job"]));
    }
}
