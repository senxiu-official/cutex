//! Owner-only `GET|PUT /v2/sessions/{id}/claude-permission-rules` (capability
//! `claude-permission-rules`): the persistent "always allow" rules of a Claude
//! agent's workspace. Storage and propagation: `crate::runtime::claude::permission_rules`.
//!
//! GET -> `{cutexSessionId, workspace, revision, updatedAt, allow, supported}`
//! (`supported`: the agent's adapter reads the rules; older adapters keep
//! asking). PUT `{allow, expectedRevision?}` replaces the workspace's list
//! (shared by every Claude agent with the same working directory) and answers
//! the GET shape plus `sessions: [{cutexSessionId, status: live | nextStart |
//! unsupported}]`. Refusals: 404 session_not_found, 409 unsupported_runtime
//! (not a Claude agent) / revision_conflict, 400 invalid_rule.

use std::net::TcpStream;

use serde::Deserialize;
use serde_json::json;
use serde_json::Value;

use crate::http::server::write_json_response;
use crate::http::server::SimpleHttpRequest;
use crate::runtime::claude::permission_rules as rules;
use crate::session::model::CutexSessionRecord;
use crate::session::model::CutexSessionRuntimeBackend;

pub const CAPABILITY: &str = "claude-permission-rules";

/// `Some(cutexSessionId)` for `/v2/sessions/{id}/claude-permission-rules`.
pub fn session_id_from_path(path: &str) -> Option<String> {
    let encoded = path.split('?').next()?.strip_prefix("/v2/sessions/")?.strip_suffix("/claude-permission-rules")?;
    if encoded.is_empty() || encoded.contains('/') {
        return None;
    }
    let decoded = super::attachments::percent_decode(encoded)?;
    (!decoded.is_empty() && !decoded.contains('/') && !decoded.chars().any(char::is_control)).then_some(decoded)
}

pub fn is_path(path: &str) -> bool {
    session_id_from_path(path).is_some()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PutBody {
    allow: Vec<String>,
    #[serde(default)]
    expected_revision: Option<u64>,
}

fn error(stream: &mut TcpStream, status: u16, code: &str, message: &str, details: Value) -> anyhow::Result<()> {
    let reason = super::server::http_reason_for(status);
    super::server::write_v2_error(stream, status, reason, code, message, status == 503, details)
}

fn view(record: &CutexSessionRecord, entry: &rules::WorkspaceRules) -> Value {
    let supported = record.claude_session.as_ref().is_some_and(rules::adapter_supports);
    json!({
        "cutexSessionId": record.cutex_session_id,
        "workspace": rules::workspace_key(&record.cwd),
        "revision": entry.revision,
        "updatedAt": entry.updated_at,
        "allow": entry.allow,
        "supported": supported,
    })
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let Some(id) = session_id_from_path(&request.path) else {
        return error(stream, 400, "invalid_request", "an exact session id is required", json!({}));
    };
    let record = match crate::session::store::with_cutex_session_store(|store| store.sessions.get(&id).cloned()) {
        Ok(Some(record)) => record,
        Ok(None) => return error(stream, 404, "session_not_found", "no such session", json!({ "cutexSessionId": id })),
        Err(err) => return error(stream, 503, "session_store_unavailable", &format!("session store: {err:#}"), json!({})),
    };
    if record.runtime_backend != CutexSessionRuntimeBackend::ClaudeSubscription || record.claude_session.is_none() {
        return error(stream, 409, "unsupported_runtime", "persistent allow rules are for Claude agents", json!({}));
    }
    match request.method.as_str() {
        "GET" => match rules::rules_for(&record.cwd) {
            Ok(entry) => write_json_response(stream, 200, "OK", &view(&record, &entry)),
            Err(err) => error(stream, 503, "rules_store_unavailable", &format!("{err:#}"), json!({})),
        },
        "PUT" => {
            let body: PutBody = match serde_json::from_slice(&request.body) {
                Ok(body) => body,
                Err(err) => return error(stream, 400, "invalid_request", &format!("body must be {{allow, expectedRevision?}}: {err}"), json!({})),
            };
            match rules::replace(&record.cwd, body.allow, body.expected_revision) {
                Ok(entry) => {
                    let sessions = rules::sync_workspace(&record.cwd, &entry.allow);
                    let mut answer = view(&record, &entry);
                    answer["sessions"] = json!(sessions);
                    write_json_response(stream, 200, "OK", &answer)
                }
                Err(rules::UpdateError::Invalid(message)) => error(stream, 400, "invalid_rule", &message, json!({})),
                Err(rules::UpdateError::RevisionConflict { expected, actual }) => error(
                    stream,
                    409,
                    "revision_conflict",
                    "the rules changed; reload and retry",
                    json!({ "expectedRevision": expected, "currentRevision": actual }),
                ),
                Err(rules::UpdateError::Store(err)) => error(stream, 503, "rules_store_unavailable", &format!("{err:#}"), json!({})),
            }
        }
        _ => error(stream, 405, "method_not_allowed", "use GET or PUT for /v2/sessions/{id}/claude-permission-rules", json!({})),
    }
}

/// An approval answered "always allow in this project": keep the offered rules
/// the answer chose (`request_params` = the pending request's params) for the
/// session's workspace and hand them to every agent there. Best effort: the
/// answer itself already reached the runtime.
pub fn remember_from_answer(cutex_session_id: &str, request_params: &Value, result: &Value) {
    let Some(chosen) = rules::chosen_rules(request_params, result) else {
        return;
    };
    let Ok(Some(record)) = crate::session::store::with_cutex_session_store(|store| store.sessions.get(cutex_session_id).cloned()) else {
        return;
    };
    if record.runtime_backend != CutexSessionRuntimeBackend::ClaudeSubscription {
        return;
    }
    match rules::add(&record.cwd, &chosen) {
        Ok(entry) => {
            let _ = rules::sync_workspace(&record.cwd, &entry.allow);
        }
        Err(err) => eprintln!("claude permission rules: could not keep {chosen:?} for {cutex_session_id}: {err:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(session_id_from_path("/v2/sessions/cutex.s1/claude-permission-rules").as_deref(), Some("cutex.s1"));
        assert_eq!(session_id_from_path("/v2/sessions/cutex.s1/claude-permission-rules?x=1").as_deref(), Some("cutex.s1"));
        assert_eq!(session_id_from_path("/v2/sessions/a%2Fb/claude-permission-rules"), None);
        assert_eq!(session_id_from_path("/v2/sessions//claude-permission-rules"), None);
        assert_eq!(session_id_from_path("/v2/sessions/a/b/claude-permission-rules"), None);
        assert_eq!(session_id_from_path("/v2/sessions/a/sandbox-extras"), None);
    }

    #[test]
    fn put_body_is_strict() {
        assert!(serde_json::from_str::<PutBody>(r#"{"allow":["Bash"],"expectedRevision":2}"#).is_ok());
        assert!(serde_json::from_str::<PutBody>(r#"{"allow":["Bash"],"extra":1}"#).is_err());
        assert!(serde_json::from_str::<PutBody>(r#"{}"#).is_err());
    }
}
