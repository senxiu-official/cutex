//! Owner-only conversion of an existing quick chat into an Agent.
use crate::{
    agent_management::QuickChatPromoteRequest,
    agent_names::NameError,
    http::server::{write_json_response, SimpleHttpRequest},
    management::server::ManagementRequestContext,
};
use serde_json::json;
use std::net::TcpStream;

pub(super) fn path_target(path: &str) -> Option<&str> {
    let id = path
        .strip_prefix("/v2/human/sessions/")?
        .strip_suffix("/promote")?;
    (id.starts_with("cutex.") && id.len() > 6 && !id.contains('/')).then_some(id)
}

pub(super) fn handle(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    context: ManagementRequestContext,
) -> anyhow::Result<()> {
    let session = path_target(request.path.split('?').next().unwrap_or(&request.path))
        .expect("dispatched promotion route");
    let outcome = (|| {
        let payload: QuickChatPromoteRequest =
            serde_json::from_slice(&request.body).map_err(|e| {
                NameError::new(
                    "invalid_request",
                    format!("strict promotion request required: {e}"),
                )
            })?;
        payload.validate()?;
        (context.promote_quick_chat)(
            &crate::management::control_plane::HumanManagementPrincipal::authenticated(),
            session,
            &payload,
        )
    })();
    match outcome {
        Ok(result) => write_json_response(stream, 200, "OK", &serde_json::to_value(result)?),
        Err(error) => {
            let typed = error.downcast_ref::<NameError>();
            let code = typed.map(|e| e.code).unwrap_or("persistence_unavailable");
            let status = match code {
                "invalid_request" | "agent_name_invalid" => 400,
                "session_not_found" | "project_not_found" => 404,
                "persistence_unavailable" | "admission_busy" => 503,
                _ => 409,
            };
            super::server::write_v2_error(
                stream,
                status,
                super::server::http_reason_for(status),
                code,
                &error.to_string(),
                status == 503,
                typed.map(|e| e.details.clone()).unwrap_or(json!({})),
            )
        }
    }
}
