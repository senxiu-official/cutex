//! Private debug-only creation surface for Phase 1 acceptance. No profile/UI.
use crate::http::server::{write_json_response, SimpleHttpRequest};
use serde_json::json;
use std::net::TcpStream;

pub const PATH: &str = "/v2/experimental/dsh/sessions";

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    if let Err(error) = crate::runtime::dsh::require_spike() {
        return super::server::write_v2_error(
            stream,
            404,
            "Not Found",
            "spike_disabled",
            &format!("{error:#}"),
            false,
            json!({}),
        );
    }
    if request.method != "POST" {
        return super::server::write_v2_error(
            stream,
            405,
            "Method Not Allowed",
            "method_not_allowed",
            "POST required",
            false,
            json!({}),
        );
    }
    let body: crate::runtime::dsh::Create = match serde_json::from_slice(&request.body) {
        Ok(body) => body,
        Err(_) => {
            return super::server::write_v2_error(
                stream,
                400,
                "Bad Request",
                "invalid_request",
                "invalid DSH spike creation body",
                false,
                json!({}),
            )
        }
    };
    match crate::runtime::dsh::create(body) {
        Ok(record) => write_json_response(
            stream,
            201,
            "Created",
            &json!({"contractVersion":2,"cutexSessionId":record.cutex_session_id,"threadId":record.app_server_thread_id(),"runtimeGeneration":record.runtime_generation}),
        ),
        Err(error) => super::server::write_v2_error(
            stream,
            409,
            "Conflict",
            "dsh_spike_failed",
            &format!("{error:#}"),
            false,
            json!({}),
        ),
    }
}
