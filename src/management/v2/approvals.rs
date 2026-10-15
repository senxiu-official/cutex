//! Owner approvals for agent actions (root credential only).
//!
//! - `GET  /v2/approvals?state=pending|all` (default `pending`) →
//!   `{hostId, approvals: [ApprovalRecord]}`, newest first
//! - `POST /v2/approvals/{id}/approve` body `{note?}` → `{approval}`
//! - `POST /v2/approvals/{id}/deny` body `{note?}` → `{approval}`;
//!   404 `approval_not_found`, 409 `approval_not_pending` (`details.state`)
//!
//! New requests and decisions are published as `cutex/approvals/changed`
//! `{reason: requested|approved|denied, approval}` on the host event stream
//! (`cutexSessionId` = [`super::services::HOST_EVENT_SESSION_ID`]). The agent
//! retries its call after a decision; see `crate::approvals`.

use std::net::TcpStream;

use serde_json::{json, Value};

use crate::approvals::{ApprovalRecord, ApprovalStore, DecideError};
use crate::http::server::{write_json_response, SimpleHttpRequest};

pub const PATH: &str = "/v2/approvals";
pub const APPROVALS_CHANGED_EVENT: &str = "cutex/approvals/changed";
const MAX_NOTE_CHARS: usize = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Approve,
    Deny,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    Collection,
    Decide(String, Decision),
}

pub fn parse_path(path: &str) -> Option<Route> {
    if path == PATH {
        return Some(Route::Collection);
    }
    let rest = path.strip_prefix("/v2/approvals/")?;
    let (id, action) = rest.split_once('/')?;
    let valid_id = id.starts_with("apr-")
        && id.len() <= 64
        && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
    if !valid_id {
        return None;
    }
    let decision = match action {
        "approve" => Decision::Approve,
        "deny" => Decision::Deny,
        _ => return None,
    };
    Some(Route::Decide(id.to_string(), decision))
}

pub fn is_approvals_path(path: &str) -> bool {
    path == PATH || path.starts_with("/v2/approvals/")
}

/// Publish an approval change for clients. Best effort: the store is the
/// source of truth and `GET /v2/approvals` always reflects it.
pub fn publish_changed(record: &ApprovalRecord, reason: &str) {
    let Ok(repository) = super::repository::management_v2_repository() else {
        return;
    };
    if let Err(error) = super::server::append_cutex_event(
        repository,
        super::services::HOST_EVENT_SESSION_ID,
        super::model::EventCorrelation::default(),
        APPROVALS_CHANGED_EVENT,
        json!({ "reason": reason, "approval": record }),
    ) {
        eprintln!("approvals: could not publish event: {error:#}");
    }
}

fn parse_note(body: &[u8]) -> Result<Option<String>, String> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let value: Value =
        serde_json::from_slice(body).map_err(|error| format!("JSON object body required: {error}"))?;
    let object = value.as_object().ok_or("JSON object body required")?;
    if let Some(key) = object.keys().find(|key| *key != "note") {
        return Err(format!("unsupported field: {key}"));
    }
    match object.get("note") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(note)) => {
            let note = note.trim();
            if note.chars().count() > MAX_NOTE_CHARS
                || note.chars().any(|c| c.is_control() && c != '\n')
            {
                return Err(format!("note: at most {MAX_NOTE_CHARS} characters"));
            }
            Ok((!note.is_empty()).then(|| note.to_string()))
        }
        Some(_) => Err("note: must be a string or null".into()),
    }
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let store = match ApprovalStore::open_default() {
        Ok(store) => store,
        Err(error) => return unavailable(stream, &error),
    };
    let now = chrono::Utc::now();
    match (request.method.as_str(), parse_path(path)) {
        ("GET", Some(Route::Collection)) => {
            let include_decided = match crate::http::query::query_value(&request.path, "state")
                .as_deref()
            {
                None | Some("pending") => false,
                Some("all") => true,
                Some(_) => {
                    return super::server::write_v2_error(
                        stream,
                        400,
                        "Bad Request",
                        "invalid_request",
                        "state must be pending or all",
                        false,
                        json!({}),
                    )
                }
            };
            match store.list(include_decided, now) {
                Ok(approvals) => write_json_response(
                    stream,
                    200,
                    "OK",
                    &json!({
                        "hostId": crate::platform::host::current_host_name(),
                        "approvals": approvals,
                    }),
                ),
                Err(error) => unavailable(stream, &error),
            }
        }
        ("POST", Some(Route::Decide(id, decision))) => {
            let note = match parse_note(&request.body) {
                Ok(note) => note,
                Err(message) => {
                    return super::server::write_v2_error(
                        stream,
                        400,
                        "Bad Request",
                        "invalid_request",
                        &message,
                        false,
                        json!({}),
                    )
                }
            };
            match store.decide(&id, decision == Decision::Approve, note, now) {
                Ok(record) => {
                    publish_changed(
                        &record,
                        if decision == Decision::Approve { "approved" } else { "denied" },
                    );
                    write_json_response(stream, 200, "OK", &json!({ "approval": record }))
                }
                Err(DecideError::NotFound) => super::server::write_v2_error(
                    stream,
                    404,
                    "Not Found",
                    "approval_not_found",
                    "no approval has this id",
                    false,
                    json!({ "approvalId": id }),
                ),
                Err(DecideError::NotPending(state)) => super::server::write_v2_error(
                    stream,
                    409,
                    "Conflict",
                    "approval_not_pending",
                    "the approval was already decided or expired",
                    false,
                    json!({ "approvalId": id, "state": state }),
                ),
                Err(DecideError::Store(error)) => super::server::write_v2_error(
                    stream,
                    503,
                    "Service Unavailable",
                    "approvals_unavailable",
                    "the approval store could not be updated",
                    true,
                    json!({ "diagnostic": error }),
                ),
            }
        }
        _ => super::server::write_v2_error(
            stream,
            405,
            "Method Not Allowed",
            "method_not_allowed",
            "GET /v2/approvals or POST /v2/approvals/{id}/approve|deny",
            false,
            json!({}),
        ),
    }
}

fn unavailable(stream: &mut TcpStream, error: &anyhow::Error) -> anyhow::Result<()> {
    super::server::write_v2_error(
        stream,
        503,
        "Service Unavailable",
        "approvals_unavailable",
        "the approval store could not be read",
        true,
        json!({ "diagnostic": format!("{error:#}") }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_parse_exact_ids_and_actions() {
        assert_eq!(parse_path("/v2/approvals"), Some(Route::Collection));
        assert_eq!(
            parse_path("/v2/approvals/apr-0123abcd/approve"),
            Some(Route::Decide("apr-0123abcd".into(), Decision::Approve))
        );
        assert_eq!(
            parse_path("/v2/approvals/apr-0123abcd/deny"),
            Some(Route::Decide("apr-0123abcd".into(), Decision::Deny))
        );
        for bad in [
            "/v2/approvals/",
            "/v2/approvals/apr-1",
            "/v2/approvals/x-1/approve",
            "/v2/approvals/apr-1/cancel",
            "/v2/approvals/apr-1/approve/extra",
            "/v2/approvals/apr-../approve",
        ] {
            assert_eq!(parse_path(bad), None, "{bad}");
        }
        assert!(is_approvals_path("/v2/approvals/apr-1/approve"));
    }

    #[test]
    fn notes_are_optional_and_bounded() {
        assert_eq!(parse_note(b""), Ok(None));
        assert_eq!(parse_note(br#"{"note":" ok "}"#), Ok(Some("ok".into())));
        assert_eq!(parse_note(br#"{"note":null}"#), Ok(None));
        assert!(parse_note(br#"{"reason":"x"}"#).is_err());
        assert!(parse_note(br#"{"note":1}"#).is_err());
        let long = format!(r#"{{"note":"{}"}}"#, "x".repeat(MAX_NOTE_CHARS + 1));
        assert!(parse_note(long.as_bytes()).is_err());
    }
}
