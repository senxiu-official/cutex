//! Explicit full-scope owner reapproval of an agent-created session's paths.
use crate::http::server::{write_json_response, SimpleHttpRequest};
use serde::Deserialize;
use serde_json::json;
use std::net::TcpStream;

pub const CAPABILITY: &str = "session-path-reapproval";

pub fn session_id_from_path(path: &str) -> Option<String> {
    let encoded = path
        .split('?')
        .next()?
        .strip_prefix("/v2/sessions/")?
        .strip_suffix("/reapprove-paths")?;
    if encoded.is_empty() || encoded.contains('/') {
        return None;
    }
    let id = super::attachments::percent_decode(encoded)?;
    (!id.is_empty() && !id.contains('/') && !id.chars().any(char::is_control)).then_some(id)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest, id: &str) -> anyhow::Result<()> {
    if request.method != "POST" {
        return super::server::write_v2_error(
            stream,
            405,
            "Method Not Allowed",
            "method_not_allowed",
            "use POST",
            false,
            json!({}),
        );
    }
    if !request.body.is_empty() && serde_json::from_slice::<Request>(&request.body).is_err() {
        return super::server::write_v2_error(
            stream,
            400,
            "Bad Request",
            "invalid_request",
            "expected an empty object",
            false,
            json!({}),
        );
    }
    let lane = super::user_input::session_input_lock(id)?;
    let _reservation = match lane.reserve() {
        Ok(reservation) => reservation,
        Err(_) => {
            return super::server::write_v2_error(
                stream,
                503,
                "Service Unavailable",
                "admission_busy",
                "session operation is busy",
                true,
                json!({"cutexSessionId":id}),
            )
        }
    };
    let path = crate::session::store::cutex_sessions_path()?;
    match crate::agent_management::creator_paths::reapprove_at(&path, id) {
        Ok(record) => {
            let value = json!({"cutexSessionId":record.cutex_session_id, "ownerApproved":true, "revision":record.revision, "creatorPathSnapshot":record.creator_path_snapshot});
            write_json_response(stream, 200, "OK", &value)
        }
        Err(error) => {
            let code =
                if crate::agent_management::creator_paths::changed_session_id(&error).is_some() {
                    "creator_paths_changed"
                } else {
                    match error.to_string().as_str() {
                        "session_not_found" => "session_not_found",
                        "session_not_agent_created" => "session_not_agent_created",
                        "session_retired" => "session_retired",
                        "session_configuration_changed" => "session_configuration_changed",
                        _ => "session_store_unavailable",
                    }
                };
            let status = match code {
                "session_not_found" => 404,
                "session_store_unavailable" => 503,
                _ => 409,
            };
            super::server::write_v2_error(
                stream,
                status,
                super::server::http_reason_for(status),
                code,
                &format!("{error:#}"),
                status == 503,
                json!({"cutexSessionId":id}),
            )
        }
    }
}
