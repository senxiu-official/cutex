//! `/v2/components` (capability `components`; contract components/README.md).
//! Owner only (access.rs + root credential in server.rs). The work lives in
//! `crate::components`; this module maps it to HTTP.
//!
//! - `GET /v2/components`: `{managed, installFrom, components: [...], operation}`.
//! - `POST /v2/components/{name}/install` and `/remove`: empty body (any
//!   field → 400). Refusals a dry run can see (unmanaged, unknown, external,
//!   not in the release, in use, busy) answer synchronously; otherwise 202
//!   `{operation}` and the work runs on a background thread holding
//!   `upgrade.lock` (409 `upgrade_blocked` while an upgrade run or another
//!   component operation holds it). Progress: host event
//!   `cutex/components/updated` and `operation` in GET.
//!   Source installs and `--force` removal are CLI-only.

use std::net::TcpStream;
use std::sync::Mutex;

use serde_json::{json, Value};

use crate::components::{self, InstallRequest, Lock};
use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::upgrade::layout::Layout;
use crate::upgrade::stage::UpgradeError;
use crate::upgrade::state::OperationLock;

pub const PATH: &str = "/v2/components";

/// The last operation this daemon started (kept until the next one).
static OPERATION: Mutex<Option<Value>> = Mutex::new(None);

pub fn is_path(path: &str) -> bool {
    path == PATH || path.starts_with("/v2/components/")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Install,
    Remove,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Action::Install => "install",
            Action::Remove => "remove",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    List,
    Act(String, Action),
}

/// `Err(true)` = known path, wrong method.
pub fn parse_route(method: &str, path: &str) -> Result<Route, bool> {
    let tail = path.strip_prefix(PATH).ok_or(false)?;
    if tail.is_empty() {
        return if method == "GET" { Ok(Route::List) } else { Err(true) };
    }
    let rest = tail.strip_prefix('/').ok_or(false)?;
    let (name, action) = rest.split_once('/').ok_or(false)?;
    let action = match action {
        "install" => Action::Install,
        "remove" => Action::Remove,
        _ => return Err(false),
    };
    if !crate::upgrade::layout::valid_label(name) {
        return Err(false);
    }
    if method == "POST" {
        Ok(Route::Act(name.to_string(), action))
    } else {
        Err(true)
    }
}

/// The body must be empty (whitespace) or `{}`.
pub fn empty_body(body: &[u8]) -> Result<(), String> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(());
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(object)) if object.is_empty() => Ok(()),
        Ok(Value::Object(object)) => Err(format!("unsupported field: {}", object.keys().next().expect("non-empty"))),
        Ok(_) => Err("the body must be empty or {}".into()),
        Err(error) => Err(format!("invalid JSON body: {error}")),
    }
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        501 => "Not Implemented",
        _ => "Service Unavailable",
    }
}

fn error(stream: &mut TcpStream, status: u16, code: &str, message: &str, details: Value) -> anyhow::Result<()> {
    super::server::write_v2_error(stream, status, reason_phrase(status), code, message, status >= 500, details)
}

fn failure(stream: &mut TcpStream, failure: &anyhow::Error) -> anyhow::Result<()> {
    match failure.downcast_ref::<UpgradeError>() {
        Some(e) => error(stream, e.status, e.code, &e.message, e.details.clone()),
        None => error(stream, 503, "component_unavailable", &format!("{failure:#}"), json!({})),
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn operation() -> Option<Value> {
    OPERATION.lock().ok().and_then(|op| op.clone())
}

fn component_state(name: &str) -> Value {
    let Ok(handler) = components::handler(name) else { return Value::Null };
    let layout = super::upgrade_api::layout();
    components::status_of(layout.as_ref(), &crate::upgrade::keys::trusted_keys(), handler.as_ref(), operation().as_ref())["state"].clone()
}

/// Store the operation and publish `{name, reason: operation, state, operation}`.
fn set_operation(value: Value) {
    if let Ok(mut op) = OPERATION.lock() {
        *op = Some(value.clone());
    }
    let name = value["name"].as_str().unwrap_or_default().to_string();
    components::publish(components::event_params(&name, "operation", &component_state(&name), Some(&value)));
}

/// `crate::components` progress hook: the running operation's `step`.
fn on_step(step: &str) {
    let Some(mut op) = operation() else { return };
    if op["state"] != "running" || op["step"] == step {
        return;
    }
    op["step"] = json!(step);
    set_operation(op);
}

/// Dry run (synchronous refusals), take `upgrade.lock`, then run on a thread.
pub fn start(layout: Option<Layout>, name: &str, action: Action) -> anyhow::Result<Value> {
    let keys = crate::upgrade::keys::trusted_keys();
    let exec = crate::upgrade::service::RealExec;
    match action {
        Action::Install => {
            components::install(layout.as_ref(), &keys, name, &InstallRequest { dry_run: true, ..Default::default() }, &exec, Lock::Held)?;
        }
        Action::Remove => {
            components::remove(layout.as_ref(), &keys, name, false, true, &exec, Lock::Held)?;
        }
    }
    let layout = layout.expect("the dry run refuses unmanaged hosts");
    let Some(lock) = OperationLock::try_acquire(&layout)? else {
        return Err(crate::upgrade::stage::blocked("run_in_progress", "a backend upgrade or another component operation is running"));
    };
    let id = format!("cop-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
    let op = json!({ "id": id, "name": name, "action": action.as_str(), "state": "running", "step": null, "error": null, "startedAt": now() });
    set_operation(op.clone());
    let thread_name = name.to_string();
    std::thread::Builder::new().name("cutex-component-op".into()).spawn(move || {
        let _lock = lock;
        let keys = crate::upgrade::keys::trusted_keys();
        let exec = crate::upgrade::service::RealExec;
        components::set_progress_hook(Some(on_step));
        let result = match action {
            Action::Install => components::install(Some(&layout), &keys, &thread_name, &InstallRequest::default(), &exec, Lock::Held),
            Action::Remove => components::remove(Some(&layout), &keys, &thread_name, false, false, &exec, Lock::Held),
        };
        components::set_progress_hook(None);
        let mut done = operation().filter(|o| o["id"] == op["id"]).unwrap_or(op);
        done["finishedAt"] = json!(now());
        match result {
            Ok(_) => done["state"] = json!("done"),
            Err(error) => {
                done["state"] = json!("failed");
                let code = error.downcast_ref::<UpgradeError>().map_or("component_failed", |e| e.code);
                done["error"] = json!(format!("{error:#}"));
                done["errorCode"] = json!(code);
            }
        }
        set_operation(done);
    })?;
    Ok(operation().unwrap_or(Value::Null))
}

/// Management start: one `startup` event per component.
pub fn announce_startup(layout: &Layout) {
    let keys = crate::upgrade::keys::trusted_keys();
    for handler in components::handlers() {
        let status = components::status_of(Some(layout), &keys, handler.as_ref(), None);
        components::publish(components::event_params(&handler.metadata().name, "startup", &status["state"], None));
    }
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let route = match parse_route(&request.method, path) {
        Ok(route) => route,
        Err(true) => return error(stream, 405, "method_not_allowed", "method not allowed on this components route", json!({})),
        Err(false) => return error(stream, 404, "route_not_found", "management v2 route not found", json!({ "path": path })),
    };
    let layout = super::upgrade_api::layout();
    match route {
        Route::List => {
            let body = components::list(layout.as_ref(), &crate::upgrade::keys::trusted_keys(), operation().as_ref());
            write_json_response(stream, 200, "OK", &body)
        }
        Route::Act(name, action) => {
            if let Err(message) = empty_body(&request.body) {
                return error(stream, 400, "invalid_request", &message, json!({}));
            }
            match start(layout, &name, action) {
                Ok(op) => write_json_response(stream, 202, "Accepted", &json!({ "operation": op })),
                Err(e) => failure(stream, &e),
            }
        }
    }
}

/// Management's watcher: apply deferred restarts once their gate opens.
pub fn settle(layout: &Layout) {
    if components::load_state(layout).is_empty() {
        return;
    }
    components::settle_pending(layout, &crate::upgrade::service::RealExec);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_and_bodies() {
        assert_eq!(parse_route("GET", "/v2/components"), Ok(Route::List));
        assert_eq!(parse_route("POST", "/v2/components"), Err(true));
        assert_eq!(parse_route("POST", "/v2/components/job-service/install"), Ok(Route::Act("job-service".into(), Action::Install)));
        assert_eq!(parse_route("POST", "/v2/components/job-service/remove"), Ok(Route::Act("job-service".into(), Action::Remove)));
        assert_eq!(parse_route("GET", "/v2/components/job-service/install"), Err(true));
        assert_eq!(parse_route("POST", "/v2/components/job-service/start"), Err(false));
        assert_eq!(parse_route("POST", "/v2/components/../install"), Err(false));
        assert!(is_path("/v2/components") && is_path("/v2/components/x/install") && !is_path("/v2/componentsx"));
        assert!(empty_body(b"").is_ok() && empty_body(b" \n").is_ok() && empty_body(b"{}").is_ok());
        assert_eq!(empty_body(br#"{"force": true}"#).unwrap_err(), "unsupported field: force");
        assert!(empty_body(b"[]").is_err() && empty_body(b"{").is_err());
    }

    #[test]
    fn unmanaged_hosts_are_refused_synchronously() {
        let error = start(None, "job-service", Action::Install).unwrap_err();
        let code = error.downcast_ref::<UpgradeError>().map(|e| e.code);
        let expected = if cfg!(all(target_os = "linux", target_arch = "x86_64")) { "unmanaged_install" } else { "component_unsupported_platform" };
        assert_eq!(code, Some(expected));
        let error = start(None, "no-such", Action::Remove).unwrap_err();
        assert_eq!(error.downcast_ref::<UpgradeError>().map(|e| e.code), Some("component_unknown"));
    }
}
