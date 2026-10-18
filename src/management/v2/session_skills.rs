//! Root-only `GET /v2/sessions/{id}/skills` (capability
//! `claude-session-skills`): the skills a Claude agent's adapter loads (its
//! workspace `.agents/skills` / `.claude/skills`, the account's
//! `cutex-skills` and the host's `~/.cutex/claude-skills`), in the Codex
//! `skills/list` shape, read by cutex so it also works while the runtime is
//! offline. A client's skill picker sends these as `skill` inputs; the
//! adapter accepts only paths inside those roots. Codex agents answer 409
//! `unsupported_runtime` (their catalog is `POST /v2/codex/requests skills/list`).

use std::net::TcpStream;

use serde_json::json;

use crate::http::server::{write_json_response, SimpleHttpRequest};

/// `Some(cutexSessionId)` for `/v2/sessions/{id}/skills`.
pub fn session_id_from_path(path: &str) -> Option<String> {
    let encoded = path.split('?').next()?.strip_prefix("/v2/sessions/")?.strip_suffix("/skills")?;
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
    use super::server::write_v2_error;
    if request.method != "GET" {
        return write_v2_error(stream, 405, "Method Not Allowed", "method_not_allowed",
            "use GET for /v2/sessions/{id}/skills", false, json!({}));
    }
    let Some(id) = session_id_from_path(&request.path) else {
        return write_v2_error(stream, 400, "Bad Request", "invalid_request",
            "an exact session id is required", false, json!({}));
    };
    let record = match crate::session::store::with_cutex_session_store(|store| store.sessions.get(&id).cloned()) {
        Ok(Some(record)) => record,
        Ok(None) => {
            return write_v2_error(stream, 404, "Not Found", "session_not_found", "no such session", false, json!({}));
        }
        Err(error) => {
            return write_v2_error(stream, 503, "Service Unavailable", "session_store_unavailable",
                &format!("session store: {error:#}"), true, json!({}));
        }
    };
    if record.claude_session.is_none() {
        return write_v2_error(stream, 409, "Conflict", "unsupported_runtime",
            "only Claude agents have a session skill catalog; Codex agents use POST /v2/codex/requests skills/list",
            false, json!({}));
    }
    match crate::runtime::claude::skill_catalog::list(&record) {
        Ok(body) => write_json_response(stream, 200, "OK", &body),
        Err(error) => write_v2_error(stream, 409, "Conflict", "skills_unavailable",
            &format!("{error:#}"), false, json!({})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skills_paths() {
        assert_eq!(session_id_from_path("/v2/sessions/cutex.s1/skills").as_deref(), Some("cutex.s1"));
        assert_eq!(session_id_from_path("/v2/sessions/cutex.s1/skills?x=1").as_deref(), Some("cutex.s1"));
        assert_eq!(session_id_from_path("/v2/sessions/a%2Fb/skills"), None);
        assert_eq!(session_id_from_path("/v2/sessions//skills"), None);
        assert!(!is_path("/v2/sessions/cutex.s1/harness"));
        assert!(!is_path("/v2/codex/skills/archive"));
    }
}
