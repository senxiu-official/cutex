//! Root-only `GET /v2/sessions/{id}/claude-code` (capability
//! `claude-code-sources`): which Claude Code settings sources, claude.ai
//! connectors and external MCP servers one Claude agent gets — what its
//! running adapter was launched with (`launched`, null before its first
//! launch by this cutex), what the next launch would pass (`next`,
//! `servers` with each merged MCP layer entry and why one is left out), and
//! `restartNeeded` when the two differ while the runtime runs. No secret
//! values: MCP configs are not part of the answer. Logic in
//! `runtime::claude::code_sources`.

use std::net::TcpStream;

use serde_json::json;

use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::session::model::CutexSessionRuntimeBackend;

/// `Some(cutexSessionId)` for `/v2/sessions/{id}/claude-code`.
pub fn session_id_from_path(path: &str) -> Option<String> {
    let encoded = path.split('?').next()?.strip_prefix("/v2/sessions/")?.strip_suffix("/claude-code")?;
    if encoded.is_empty() || encoded.contains('/') {
        return None;
    }
    let decoded = super::attachments::percent_decode(encoded)?;
    (!decoded.is_empty() && !decoded.contains('/') && !decoded.chars().any(char::is_control)).then_some(decoded)
}

pub fn is_path(path: &str) -> bool {
    session_id_from_path(path).is_some()
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    if request.method != "GET" {
        return super::server::write_v2_error(stream, 405, "Method Not Allowed", "method_not_allowed",
            "use GET for /v2/sessions/{id}/claude-code", false, json!({}));
    }
    let Some(id) = session_id_from_path(&request.path) else {
        return super::server::write_v2_error(stream, 400, "Bad Request", "invalid_request",
            "an exact session id is required", false, json!({}));
    };
    let record = match crate::session::store::with_cutex_session_store(|store| store.sessions.get(&id).cloned()) {
        Ok(Some(record)) => record,
        Ok(None) => {
            return super::server::write_v2_error(stream, 404, "Not Found", "session_not_found",
                "no such session", false, json!({}));
        }
        Err(error) => {
            return super::server::write_v2_error(stream, 503, "Service Unavailable", "session_store_unavailable",
                &format!("session store: {error:#}"), true, json!({}));
        }
    };
    if record.runtime_backend != CutexSessionRuntimeBackend::ClaudeSubscription || record.claude_session.is_none() {
        return super::server::write_v2_error(stream, 409, "Conflict", "unsupported_runtime",
            "Claude Code sources apply to Claude agents only", false, json!({}));
    }
    match crate::runtime::claude::code_sources::view(&record) {
        Ok(body) => write_json_response(stream, 200, "OK", &body),
        Err(error) => super::server::write_v2_error(stream, 503, "Service Unavailable", "store_unavailable",
            &format!("{error:#}"), true, json!({})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_code_paths() {
        assert_eq!(session_id_from_path("/v2/sessions/cutex.s1/claude-code").as_deref(), Some("cutex.s1"));
        assert_eq!(session_id_from_path("/v2/sessions/cutex.s1/claude-code?x=1").as_deref(), Some("cutex.s1"));
        assert_eq!(session_id_from_path("/v2/sessions/a%2Fb/claude-code"), None);
        assert_eq!(session_id_from_path("/v2/sessions//claude-code"), None);
        assert!(!is_path("/v2/sessions/cutex.s1/harness"));
    }
}
