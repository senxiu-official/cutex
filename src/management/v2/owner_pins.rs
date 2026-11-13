//! 备忘 for the owner (capability `owner-pins`): `GET /v2/owner-pins
//! ?state=open|all&agentId=&limit=` (every open pin, plus up to `limit`
//! resolved ones newest first with `all`), `PATCH /v2/owner-pins/{id}`
//! `{state: open|done|dropped, expectedRevision?}` and `DELETE
//! /v2/owner-pins/{id}`. Root only (incl. device / client tokens), like
//! owner notifications. Changes publish host event `cutex/ownerPin/changed`
//! `{pin}` or `{pinId, deleted: true}`. Persistence: crate::owner_pins.
use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::owner_pins::{publish_deleted, publish_pin, OwnerPinsStore, PinError, State, RESOLVED_PAGE};
use serde::Deserialize;
use serde_json::{json, Value};
use std::net::TcpStream;

pub const PATH: &str = "/v2/owner-pins";
const MAX_REQUEST_BYTES: usize = 4096;

pub fn is_path(path: &str) -> bool {
    let path = path.split('?').next().unwrap_or(path);
    path == PATH || path.strip_prefix("/v2/owner-pins/").is_some_and(|id| !id.is_empty() && !id.contains('/'))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Patch {
    state: String,
    #[serde(default)]
    expected_revision: Option<u64>,
}

fn valid_id(id: &str) -> bool {
    id.len() <= 80 && id.strip_prefix("pin-").is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
}

fn bad(message: impl Into<String>) -> PinError {
    PinError::Invalid(message.into())
}

fn execute(request: &SimpleHttpRequest) -> Result<Value, PinError> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    if path == PATH {
        let q = |key| crate::http::query::query_value(&request.path, key).filter(|v| !v.is_empty());
        let all = match q("state").as_deref() {
            None | Some("open") => false,
            Some("all") => true,
            Some(_) => return Err(bad("state must be open or all")),
        };
        let agent = q("agentId");
        if agent.as_deref().is_some_and(|a| a.len() > 200 || !a.bytes().all(|b| b.is_ascii_alphanumeric() || b".-_:".contains(&b))) {
            return Err(bad("agentId: a cutex session id"));
        }
        let limit = match q("limit") {
            None => RESOLVED_PAGE,
            Some(v) => v.parse::<usize>().ok().filter(|n| (1..=RESOLVED_PAGE).contains(n)).ok_or_else(|| bad(format!("limit must be 1..{RESOLVED_PAGE}")))?,
        };
        let list = OwnerPinsStore::open_default()?.owner_list(all, agent.as_deref(), limit)?;
        return Ok(json!({"hostId": crate::platform::host::current_host_name(), "pins": list.pins, "openCount": list.open_count}));
    }
    if request.path.contains('?') {
        return Err(bad("pin-by-id operations do not accept query parameters"));
    }
    let id = path.strip_prefix("/v2/owner-pins/").unwrap_or_default();
    if !valid_id(id) {
        return Err(bad("a pin id is required"));
    }
    let store = OwnerPinsStore::open_default()?;
    if request.method == "DELETE" {
        store.owner_delete(id)?;
        publish_deleted(id);
        return Ok(json!({"deleted": true, "pinId": id}));
    }
    if request.body.len() > MAX_REQUEST_BYTES {
        return Err(bad("request body is too large"));
    }
    let patch: Patch = serde_json::from_slice(&request.body).map_err(|error| bad(format!("invalid request: {error}")))?;
    let state = State::parse(&patch.state).ok_or_else(|| bad("state must be open, done or dropped"))?;
    let pin = store.owner_set_state(id, state, patch.expected_revision, chrono::Utc::now())?;
    publish_pin(&pin);
    Ok(json!({"pin": pin}))
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let allowed = if path == PATH { request.method == "GET" } else { request.method == "PATCH" || request.method == "DELETE" };
    if !allowed {
        return super::server::write_v2_error(stream, 405, "Method Not Allowed", "method_not_allowed",
            "use GET for /v2/owner-pins and PATCH or DELETE for /v2/owner-pins/{id}", false, json!({}));
    }
    match execute(request) {
        Ok(value) => write_json_response(stream, 200, "OK", &value),
        Err(error) => {
            let (status, reason) = match &error {
                PinError::Invalid(_) => (400, "Bad Request"),
                PinError::NotFound => (404, "Not Found"),
                PinError::RevisionConflict(_) | PinError::NotOpen | PinError::IdempotencyConflict => (409, "Conflict"),
                PinError::TooManyOpen(_) | PinError::HostFull | PinError::RateLimited => (429, "Too Many Requests"),
                PinError::Storage(_) => (503, "Service Unavailable"),
            };
            let (code, message) = error.code();
            let code = match code {
                "invalid_arguments" => "invalid_request",
                "persistence_unavailable" => "owner_pins_unavailable",
                other => other,
            };
            let details = match error {
                PinError::RevisionConflict(current) => json!({"currentRevision": current}),
                _ => json!({}),
            };
            super::server::write_v2_error(stream, status, reason, code, &message, matches!(status, 503), details)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert!(is_path("/v2/owner-pins") && is_path("/v2/owner-pins?state=all") && is_path("/v2/owner-pins/pin-1"));
        assert!(!is_path("/v2/owner-pinsx") && !is_path("/v2/owner-pins/a/b") && !is_path("/v2/owner-pins/"));
        assert!(valid_id("pin-0a1b-2c") && !valid_id("pin-") && !valid_id("pin-../x") && !valid_id("note-1"));
    }
}
