//! r290 P5 owner-created schedules. The server authenticates the owner before dispatch.
use crate::{
    http::server::{write_json_response, SimpleHttpRequest},
    schedules::{self, Error, Store},
};
use chrono::{Local, Utc};
use serde_json::{json, Value};
use std::net::TcpStream;

pub fn is_path(path: &str) -> bool {
    path == "/v2/schedules" || path.starts_with("/v2/schedules/")
}
pub fn owner_allowed(authenticated: bool, owner: bool, full: bool, method: &str) -> bool {
    authenticated && owner && (method == "GET" || full)
}
pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let result = dispatch(&request.method, &request.path, &request.body);
    match result {
        Ok((status, value)) => write_json_response(
            stream,
            status,
            if status == 201 { "Created" } else { "OK" },
            &value,
        ),
        Err(error) => {
            let (status, code) = match error.downcast_ref::<Error>() {
                Some(Error::Invalid(_)) => (400, "invalid_request"),
                Some(Error::NotFound) => (404, "schedule_not_found"),
                Some(Error::Conflict(_)) => (409, "schedule_conflict"),
                None => (503, "schedules_unavailable"),
            };
            super::server::write_v2_error(
                stream,
                status,
                match status {
                    400 => "Bad Request",
                    404 => "Not Found",
                    409 => "Conflict",
                    _ => "Service Unavailable",
                },
                code,
                &error.to_string(),
                status == 503,
                json!({}),
            )
        }
    }
}
fn invalid(message: impl ToString) -> anyhow::Error {
    Error::Invalid(message.to_string()).into()
}
fn decode<T: serde::de::DeserializeOwned>(body: &[u8]) -> anyhow::Result<T> {
    serde_json::from_slice(body).map_err(invalid)
}
fn valid_id(id: &str) -> bool {
    id.strip_prefix("schedule-")
        .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
}
fn dispatch(method: &str, path: &str, body: &[u8]) -> anyhow::Result<(u16, Value)> {
    let clean = path.split('?').next().unwrap_or(path);
    let tail = clean
        .strip_prefix("/v2/schedules")
        .ok_or_else(|| invalid("invalid schedule route"))?;
    let limit = crate::http::query::query_value(path, "limit")
        .map(|s| s.parse::<usize>().map_err(invalid))
        .transpose()?
        .unwrap_or(50);
    let cursor = crate::http::query::query_value(path, "cursor");
    if cursor.as_ref().is_some_and(|s| s.len() > 100) {
        return Err(invalid("invalid cursor"));
    }
    let store = Store::open_default()?;
    let validate = |target: &str| {
        schedules::runtime::validate_target(target).map_err(|e| {
            if e.is::<schedules::runtime::Unavailable>() {
                invalid(e)
            } else {
                e
            }
        })
    };
    let now = Utc::now();
    if tail.is_empty() {
        return match method {
            "GET" => {
                let target = crate::http::query::query_value(path, "targetSessionId");
                Ok((
                    200,
                    serde_json::to_value(store.list(
                        target.as_deref(),
                        cursor.as_deref(),
                        limit,
                    )?)?,
                ))
            }
            "POST" => Ok((
                201,
                json!({"schedule":store.create(decode(body)?,now,&Local,validate)?}),
            )),
            _ => Err(invalid("unsupported schedule method")),
        };
    }
    let parts: Vec<_> = tail.trim_start_matches('/').split('/').collect();
    if parts.is_empty() || !valid_id(parts[0]) {
        return Err(invalid("invalid schedule ID"));
    }
    let id = parts[0];
    if parts.len() == 2 && parts[1] == "runs" && method == "GET" {
        let before = cursor
            .map(|s| s.parse::<u64>().map_err(invalid))
            .transpose()?;
        return Ok((200, serde_json::to_value(store.runs(id, before, limit)?)?));
    }
    if parts.len() != 1 {
        return Err(invalid("invalid schedule route"));
    }
    match method {
        "GET" => Ok((200, json!({"schedule":store.get(id)?}))),
        "PUT" => Ok((
            200,
            json!({"schedule":store.update(id,decode(body)?,now,&Local,validate)?}),
        )),
        "DELETE" => Ok((200, json!({"schedule":store.delete(id,decode(body)?,now)?}))),
        _ => Err(invalid("unsupported schedule method")),
    }
}
