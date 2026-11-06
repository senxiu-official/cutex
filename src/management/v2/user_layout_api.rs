//! Personal layout routes. No host/session authority locks are acquired here.
use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::management::{
    user_layout::{self, LayoutStore},
    users::{UserError, UserStore},
};
use serde_json::{json, Value};
use std::net::TcpStream;

pub const PATH: &str = "/v2/users/me/layout";
pub fn is_path(path: &str) -> bool {
    path == PATH || path.starts_with("/v2/users/me/layout/")
}

/// Capture once before replay/subscription; never read another store per event.
pub fn event_doc_key() -> anyhow::Result<String> {
    let users = UserStore::open_default()?;
    user_layout::identity(&users)
        .map(|who| who.doc_key)
        .map_err(|e| anyhow::anyhow!("{}", e.message))
}

pub fn event_visible(event: &super::model::EventEnvelope, doc_key: &str) -> bool {
    event.cutex.as_ref().is_none_or(|message| {
        message.method != user_layout::CHANGED || message.params["docKey"].as_str() == Some(doc_key)
    })
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    match route(request) {
        Ok(value) => write_json_response(stream, 200, "OK", &value),
        Err(e) => {
            let reason = match e.status {
                400 => "Bad Request",
                403 => "Forbidden",
                405 => "Method Not Allowed",
                409 => "Conflict",
                413 => "Payload Too Large",
                _ => "Service Unavailable",
            };
            super::server::write_v2_error(
                stream,
                e.status,
                reason,
                e.code,
                &e.message,
                e.status == 503,
                e.details,
            )
        }
    }
}

fn route(request: &SimpleHttpRequest) -> Result<Value, UserError> {
    let (path, query) = request.path.split_once('?').unwrap_or((&request.path, ""));
    // Phase 1b is not available yet. Never silently write one's own layout
    // when a newer desktop asked to edit somebody else's document.
    if !query.is_empty() {
        return Err(UserError::new(
            400,
            "layout_switch_unavailable",
            "layout switching is not available on this host",
        ));
    }
    let users = UserStore::open_default().map_err(user_layout::unavailable)?;
    let who = user_layout::identity(&users)?;
    let store = LayoutStore::at(users.dir());
    if path == PATH {
        if request.method != "GET" {
            return Err(method());
        }
        let record = store.get(&who)?;
        return Ok(
            json!({"docKey": who.doc_key, "userId": who.user_id, "displayName": who.display_name,
            "ownerPersonConfirmed": who.owner_person_confirmed, "pointer": record.pointer, "sections": record.sections}),
        );
    }
    let name = path
        .strip_prefix("/v2/users/me/layout/")
        .unwrap_or_default();
    if !user_layout::valid_section(name) {
        return Err(user_layout::invalid("invalid section name"));
    }
    if (name == "pointer" && request.method != "PUT")
        || (name != "pointer" && !matches!(request.method.as_str(), "PUT" | "DELETE"))
    {
        return Err(method());
    }
    let body: Value = serde_json::from_slice(&request.body)
        .map_err(|_| user_layout::invalid("JSON object body required"))?;
    let body = body
        .as_object()
        .ok_or_else(|| user_layout::invalid("JSON object body required"))?;
    let allowed: &[&str] = if name == "pointer" {
        &["memoryHost", "label"]
    } else if request.method == "DELETE" {
        &["revision"]
    } else {
        &["revision", "data"]
    };
    if body.keys().any(|k| !allowed.contains(&k.as_str())) {
        return Err(user_layout::invalid("unsupported field"));
    }
    if name == "pointer" {
        let host = body
            .get("memoryHost")
            .and_then(Value::as_str)
            .ok_or_else(|| user_layout::invalid("memoryHost required"))?;
        let label = body
            .get("label")
            .and_then(Value::as_str)
            .ok_or_else(|| user_layout::invalid("label required"))?;
        return serde_json::to_value(store.set_pointer(&who, host, label)?)
            .map_err(user_layout::unavailable);
    }
    let revision = body
        .get("revision")
        .and_then(Value::as_u64)
        .ok_or_else(|| user_layout::invalid("revision must be a non-negative integer"))?;
    let data = if request.method == "DELETE" {
        Value::Null
    } else {
        body.get("data")
            .cloned()
            .ok_or_else(|| user_layout::invalid("data required"))?
    };
    let section = store.put(&who, name, revision, data)?;
    Ok(
        json!({"revision": section.revision, "updatedAt": if section.updated_at.is_empty() { Value::Null } else { json!(section.updated_at) }}),
    )
}

fn method() -> UserError {
    UserError::new(
        405,
        "method_not_allowed",
        "unsupported method for this route",
    )
}
