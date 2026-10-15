use crate::agent_management::{human_config_action, HumanConfigRequest};
use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::management::control_plane::HumanManagementPrincipal;
use serde_json::json;
use std::net::TcpStream;

pub(super) fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let payload = match serde_json::from_slice::<HumanConfigRequest>(&request.body) {
        Ok(payload) => payload,
        Err(error) => {
            return super::server::write_v2_error(
                stream,
                400,
                "Bad Request",
                "invalid_request",
                &error.to_string(),
                false,
                json!({}),
            )
        }
    };
    let result = crate::session::store::cutex_sessions_path().and_then(|path| {
        human_config_action(&HumanManagementPrincipal::authenticated(), &path, &payload)
    });
    match result {
        Ok(value) => write_json_response(stream, 200, "OK", &value),
        Err(error) => super::server::write_v2_error(
            stream,
            409,
            "Conflict",
            "configuration_not_applied",
            &format!("{error:#}"),
            false,
            json!({}),
        ),
    }
}
