//! Owner formal-name routes, separate from display metadata.
//! POST /v2/human/agents/{fullId}/rename
//! GET  /v2/human/agents/{fullId}/name-history?before={sequence}&limit=50
use super::repository::EventRepository;
use crate::agent_names::{NameError, RenameRequest};
use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::management::server::ManagementRequestContext;
use serde_json::{json, Value};
use std::net::TcpStream;

pub(super) fn path_target(path: &str) -> Option<(&str, &str)> {
    let suffix = path.strip_prefix("/v2/human/agents/")?;
    let (id, operation) = suffix.split_once('/')?;
    if !id.starts_with("cutex.") || id.len() <= 6 || operation.contains('/') {
        return None;
    }
    matches!(operation, "rename" | "name-history").then_some((id, operation))
}

fn failure(stream: &mut TcpStream, error: anyhow::Error) -> anyhow::Result<()> {
    let typed = error.downcast_ref::<NameError>();
    let code = typed
        .map(|error| error.code)
        .unwrap_or("persistence_unavailable");
    let message = typed
        .map(|error| error.message.clone())
        .unwrap_or_else(|| format!("name ledger is unavailable: {error:#}"));
    let details = typed
        .map(|error| error.details.clone())
        .unwrap_or_else(|| json!({}));
    let status = match code {
        "invalid_request" | "agent_name_invalid" => 400,
        "session_not_found" | "agent_not_found" => 404,
        "persistence_unavailable" | "admission_busy" => 503,
        _ => 409,
    };
    super::server::write_v2_error(
        stream,
        status,
        super::server::http_reason_for(status),
        code,
        &message,
        status == 503,
        details,
    )
}

pub(super) fn handle(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    context: ManagementRequestContext,
    repository: &EventRepository,
) -> anyhow::Result<()> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let Some((session_id, operation)) = path_target(path) else {
        return Ok(());
    };
    match (request.method.as_str(), operation) {
        ("POST", "rename") => {
            let payload: RenameRequest = match serde_json::from_slice(&request.body) {
                Ok(payload) => payload,
                Err(error) => {
                    return failure(
                        stream,
                        NameError::new(
                            "invalid_request",
                            format!("strict rename request parsing failed: {error}"),
                        )
                        .into(),
                    )
                }
            };
            let result = (|| {
                payload.validate_typed()?;
                let provider = crate::agent_management::AgentManagementProvider::open_default()?;
                provider.rename_formal_name_for_management(
                    &crate::management::control_plane::HumanManagementPrincipal::authenticated(),
                    session_id,
                    &payload,
                )
            })();
            let committed = match result {
                Ok(committed) => committed,
                Err(error) => return failure(stream, error),
            };
            // The normal sessions watcher also emits the canonical invalidation
            // from the session-store/catalog revision. No runtime is restarted.
            let resource = (context.load_registry)()
                .ok()
                .and_then(|registry| {
                    super::session::session_resource_including_hidden(
                        session_id,
                        &registry,
                        context.load_runtime_status,
                        repository,
                    )
                    .ok()
                    .flatten()
                })
                .unwrap_or(Value::Null);
            write_json_response(
                stream,
                200,
                "OK",
                &json!({
                    "contractVersion":2,"requestId":payload.request_id,"cutexSessionId":session_id,
                    "event":committed.event,"replayed":committed.replayed,"session":resource,
                }),
            )
        }
        ("GET", "name-history") => {
            let result = (|| -> anyhow::Result<_> {
                let mut before = None;
                let mut limit = 50usize;
                let mut seen = std::collections::BTreeSet::new();
                if let Some((_, query)) = request.path.split_once('?') {
                    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
                        if !seen.insert(key.to_string()) {
                            return Err(NameError::new(
                                "invalid_request",
                                "duplicate history query field",
                            )
                            .into());
                        }
                        match key.as_ref() {
                            "before" => {
                                before = Some(
                                    value
                                        .parse::<u64>()
                                        .ok()
                                        .filter(|cursor| *cursor > 0)
                                        .ok_or_else(|| {
                                            NameError::new(
                                                "invalid_request",
                                                "before must be a positive event sequence",
                                            )
                                        })?,
                                );
                            }
                            "limit" => {
                                limit = value
                                    .parse::<usize>()
                                    .ok()
                                    .filter(|limit| (1..=100).contains(limit))
                                    .ok_or_else(|| {
                                        NameError::new(
                                            "invalid_request",
                                            "limit must be between 1 and 100",
                                        )
                                    })?;
                            }
                            _ => {
                                return Err(NameError::new(
                                    "invalid_request",
                                    "unknown history query field",
                                )
                                .into())
                            }
                        }
                    }
                }
                let path = crate::session::store::cutex_sessions_path()?;
                crate::session::store::formal_name_history_at(&path, session_id, before, limit)
            })();
            match result {
                Ok(page) => write_json_response(
                    stream,
                    200,
                    "OK",
                    &json!({"contractVersion":2,"cutexSessionId":session_id,"events":page.events,"next":page.next}),
                ),
                Err(error) => failure(stream, error),
            }
        }
        _ => super::server::write_v2_error(
            stream,
            405,
            "Method Not Allowed",
            "method_not_allowed",
            "use POST for rename and GET for name-history",
            false,
            json!({}),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn owner_paths_and_request_shape_are_separate_from_display_metadata() {
        assert_eq!(
            path_target("/v2/human/agents/cutex.a/rename"),
            Some(("cutex.a", "rename"))
        );
        assert_eq!(
            path_target("/v2/human/agents/cutex.a/name-history"),
            Some(("cutex.a", "name-history"))
        );
        assert!(path_target("/v2/human/agents/A/rename").is_none());
        assert!(path_target("/v2/human/agents/cutex.a/rename/extra").is_none());
        assert!(serde_json::from_value::<RenameRequest>(
            json!({"requestId":"r","name":"B","expectedRevision":1,"actor":"agent"})
        )
        .is_err());
        let class = super::super::access::classify("POST", "/v2/human/agents/cutex.a/rename");
        assert_eq!(class, Some(super::super::access::Class::OwnerOnly));
    }
}
