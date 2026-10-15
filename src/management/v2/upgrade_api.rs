//! `/v2/upgrade*` (capability `backend-upgrade`; contract in
//! docs/backend-upgrade.md, schema export `upgrade`). Owner only
//! (access.rs). The work lives in `crate::upgrade`; this module maps it to
//! HTTP, runs `stage` and the periodic check in the background, and
//! publishes host event `cutex/upgrade/updated` whenever
//! `<base>/upgrade/state.json` changes (also the runner's writes while this
//! daemon restarts).
//!
//! - `GET /v2/upgrade`: status (`control::status_view`).
//! - `POST /v2/upgrade/check`: read the channel now → status.
//! - `POST /v2/upgrade/stage` `{release?}`: 202 → background download +
//!   verify + preflight (`staging` in status; event when done).
//! - `POST /v2/upgrade/apply` `{release?, mode?, maxWaitSecs?,
//!   confirmOneWay?, packageRun?}`: 202 `{run}`.
//! - `POST /v2/upgrade/cancel`: 200 `{run}` (waiting phases only).
//! - `POST /v2/upgrade/rollback` `{to?, maxWaitSecs?, dryRun?}`: dry run 200
//!   `{preview}`, else 202 `{run, preview}`.
//! - `GET /v2/upgrade/runs?limit=`: `{runs}` newest first;
//!   `GET /v2/upgrade/runs/{id}`: `{run}`.
//! - `GET|PATCH /v2/upgrade/settings`: `{settings}` (the source URL is
//!   CLI-only: `cutex upgrade config --source`).

use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{json, Map, Value};

use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::upgrade::control::{self, ApplyRequest, RollbackRequest};
use crate::upgrade::layout::Layout;
use crate::upgrade::manifest::Mode;
use crate::upgrade::stage::{self, UpgradeError};
use crate::upgrade::state::{self, OperationLock, PackageRun, Settings, State, MAX_MAX_WAIT_SECS};

pub const PATH: &str = "/v2/upgrade";
pub const UPDATED_EVENT: &str = "cutex/upgrade/updated";
const STAGING_KEY: &str = "staging";
const WATCH_INTERVAL: Duration = Duration::from_secs(2);
const MAX_RUN_LIMIT: usize = 200;

static STAGING: AtomicBool = AtomicBool::new(false);

pub fn is_path(path: &str) -> bool {
    path == PATH || path.starts_with("/v2/upgrade/")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    Status,
    Check,
    Stage,
    Apply,
    Cancel,
    Rollback,
    Runs,
    Run(String),
    Settings,
}

/// `Err(true)` = known path, wrong method.
pub fn parse_route(method: &str, path: &str) -> Result<Route, bool> {
    let tail = path.strip_prefix(PATH).ok_or(false)?;
    let (route, methods): (Route, &[&str]) = match tail {
        "" => (Route::Status, &["GET"]),
        "/check" => (Route::Check, &["POST"]),
        "/stage" => (Route::Stage, &["POST"]),
        "/apply" => (Route::Apply, &["POST"]),
        "/cancel" => (Route::Cancel, &["POST"]),
        "/rollback" => (Route::Rollback, &["POST"]),
        "/runs" => (Route::Runs, &["GET"]),
        "/settings" => (Route::Settings, &["GET", "PATCH"]),
        other => match other.strip_prefix("/runs/") {
            Some(id) if crate::upgrade::layout::valid_label(id) => (Route::Run(id.to_string()), &["GET"]),
            _ => return Err(false),
        },
    };
    if methods.contains(&method) {
        Ok(route)
    } else {
        Err(true)
    }
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        502 => "Bad Gateway",
        _ => "Service Unavailable",
    }
}

fn error(stream: &mut TcpStream, status: u16, code: &str, message: &str, details: Value) -> anyhow::Result<()> {
    super::server::write_v2_error(stream, status, reason_phrase(status), code, message, status >= 500, details)
}

fn failure(stream: &mut TcpStream, failure: &anyhow::Error) -> anyhow::Result<()> {
    match failure.downcast_ref::<UpgradeError>() {
        Some(e) => error(stream, e.status, e.code, &e.message, e.details.clone()),
        None => error(stream, 503, "upgrade_unavailable", &format!("{failure:#}"), json!({})),
    }
}

/// The managed install this daemon runs from, `None` when unmanaged (no
/// `install.json` where the executable sits). Never the platform default
/// base by itself (same rule as `Layout::detect` and `cutex upgrade
/// status`): a managed install elsewhere on the machine must not make a
/// hand-deployed daemon report itself managed or let the scheduler act on
/// it. Never a stand-in path either (a shared temp dir would let any local
/// user create `install.json` there and make this daemon "managed").
pub fn layout() -> Option<Layout> {
    Layout::detect()
}

fn unmanaged_error() -> anyhow::Error {
    stage::blocked("unmanaged_install", "this daemon does not run from a managed install (no install.json)")
}

/// An unmanaged daemon: reads answer `managed: false` / empty, every
/// mutation is refused with `unmanaged_install`. Nothing on disk is read.
fn unmanaged(method: &str, route: &Route) -> anyhow::Result<(u16, Value)> {
    match route {
        Route::Status => Ok((200, control::unmanaged_status_view())),
        Route::Settings if method == "GET" => Ok((200, json!({ "settings": Settings::default().unmanaged_view() }))),
        Route::Settings => Err(unmanaged_error()),
        Route::Runs => Ok((200, json!({ "runs": [] }))),
        Route::Run(id) => Err(stage::UpgradeError::boxed(404, "run_not_found", "no upgrade run has this id", json!({ "runId": id }))),
        Route::Check | Route::Stage | Route::Apply | Route::Cancel | Route::Rollback => Err(unmanaged_error()),
    }
}

fn body_object(body: &[u8], allowed: &[&str]) -> Result<Map<String, Value>, String> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(Map::new());
    }
    let value: Value = serde_json::from_slice(body).map_err(|e| format!("invalid JSON body: {e}"))?;
    let Value::Object(object) = value else { return Err("the body must be a JSON object".into()) };
    if let Some(key) = object.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(format!("unsupported field: {key}"));
    }
    Ok(object)
}

fn opt_string(object: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if !s.is_empty() && s.len() <= 64 => Ok(Some(s.clone())),
        _ => Err(format!("{key} must be a non-empty string")),
    }
}

fn opt_u64(object: &Map<String, Value>, key: &str, max: u64) -> Result<Option<u64>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_u64().filter(|v| *v <= max).map(Some).ok_or_else(|| format!("{key} must be an integer 0..={max}")),
    }
}

fn opt_bool(object: &Map<String, Value>, key: &str) -> Result<Option<bool>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        _ => Err(format!("{key} must be a boolean")),
    }
}

pub fn parse_apply(body: &[u8]) -> Result<ApplyRequest, String> {
    let object = body_object(body, &["release", "mode", "maxWaitSecs", "confirmOneWay", "packageRun"])?;
    let mode = match opt_string(&object, "mode")?.as_deref() {
        None => None,
        Some("reload") => Some(Mode::Reload),
        Some("full") => Some(Mode::Full),
        Some(_) => return Err("mode must be reload or full".into()),
    };
    let package_run = match opt_string(&object, "packageRun")? {
        None => None,
        Some(text) => Some(PackageRun::parse(&text).ok_or("packageRun must be after, during or none")?),
    };
    Ok(ApplyRequest {
        release: opt_string(&object, "release")?,
        mode,
        max_wait_secs: opt_u64(&object, "maxWaitSecs", MAX_MAX_WAIT_SECS)?,
        confirm_one_way: opt_string(&object, "confirmOneWay")?,
        package_run,
        requested_by: Some("management-api".into()),
    })
}

pub fn parse_rollback(body: &[u8]) -> Result<RollbackRequest, String> {
    let object = body_object(body, &["to", "maxWaitSecs", "dryRun"])?;
    Ok(RollbackRequest {
        to: opt_string(&object, "to")?,
        max_wait_secs: opt_u64(&object, "maxWaitSecs", MAX_MAX_WAIT_SECS)?,
        dry_run: opt_bool(&object, "dryRun")?.unwrap_or(false),
        requested_by: Some("management-api".into()),
    })
}

/// Apply a settings PATCH; `Err((code, message))`.
pub fn patch_settings(settings: &mut Settings, body: &[u8]) -> Result<(), (&'static str, String)> {
    let invalid = |m: String| ("invalid_request", m);
    let object = body_object(
        body,
        &["channel", "source", "checkIntervalHours", "autoStage", "autoApply", "packageRun", "packageRunBatch", "maxWaitSecs"],
    )
    .map_err(invalid)?;
    if object.contains_key("source") {
        return Err(("source_not_editable", "the release source is set with `cutex upgrade config --source` on the host".into()));
    }
    if let Some(channel) = opt_string(&object, "channel").map_err(invalid)? {
        let normalized = crate::build_info::normalize_channel(&channel).ok_or_else(|| invalid("channel must be release or main".into()))?;
        settings.channel = normalized.into();
    }
    if let Some(hours) = opt_u64(&object, "checkIntervalHours", 24 * 7).map_err(invalid)? {
        settings.check_interval_hours = hours as u32;
    }
    if let Some(value) = opt_bool(&object, "autoStage").map_err(invalid)? {
        settings.auto_stage = value;
    }
    if let Some(value) = opt_bool(&object, "autoApply").map_err(invalid)? {
        settings.auto_apply = value;
    }
    if let Some(text) = opt_string(&object, "packageRun").map_err(invalid)? {
        settings.package_run = PackageRun::parse(&text).ok_or_else(|| invalid("packageRun must be after, during or none".into()))?;
    }
    if let Some(batch) = opt_u64(&object, "packageRunBatch", 50).map_err(invalid)? {
        if batch == 0 {
            return Err(invalid("packageRunBatch must be at least 1".into()));
        }
        settings.package_run_batch = batch as usize;
    }
    if let Some(wait) = opt_u64(&object, "maxWaitSecs", MAX_MAX_WAIT_SECS).map_err(invalid)? {
        settings.max_wait_secs = wait;
    }
    Ok(())
}

fn managed(layout: &Layout) -> anyhow::Result<()> {
    if layout.is_managed() {
        Ok(())
    } else {
        Err(unmanaged_error())
    }
}

fn check_now(layout: &Layout) -> anyhow::Result<()> {
    managed(layout)?;
    let settings = Settings::load(layout);
    let source = settings.source()?;
    stage::check(layout, &source, &crate::upgrade::keys::trusted_keys(), settings.channel(layout), &stage::LocalRuntimes::read())?;
    Ok(())
}

fn set_staging(layout: &Layout, value: Value) {
    let _ = State::update(layout, |state| {
        if value.is_null() {
            state.extra.remove(STAGING_KEY);
        } else {
            state.extra.insert(STAGING_KEY.into(), value);
        }
    });
}

/// Stage `release` (default: the available one) on a background thread.
pub fn start_stage(layout: &Layout, release: Option<String>) -> anyhow::Result<String> {
    managed(layout)?;
    let release = match release.or_else(|| State::load(layout).available.map(|a| a.release)) {
        Some(release) => release,
        None => return Err(stage::upgrade_error(409, "nothing_available", "no newer release is known; check first")),
    };
    crate::upgrade::layout::ensure_label(&release, "release").map_err(|e| stage::upgrade_error(400, "invalid_request", format!("{e:#}")))?;
    if STAGING.swap(true, Ordering::SeqCst) {
        return Err(stage::blocked("stage_in_progress", "a release is being staged"));
    }
    let Some(lock) = OperationLock::try_acquire(layout)? else {
        STAGING.store(false, Ordering::SeqCst);
        return Err(stage::blocked("run_in_progress", "another upgrade operation is running"));
    };
    set_staging(layout, json!({ "release": release, "state": "running", "startedAt": now() }));
    let thread_layout = layout.clone();
    let thread_release = release.clone();
    let spawned = std::thread::Builder::new().name("cutex-upgrade-stage".into()).spawn(move || {
        let _lock = lock;
        let settings = Settings::load(&thread_layout);
        let result = settings.source().and_then(|source| {
            stage::stage(
                &thread_layout,
                &source,
                &crate::upgrade::keys::trusted_keys(),
                settings.channel(&thread_layout),
                &thread_release,
                &stage::LocalRuntimes::read(),
            )
        });
        match result {
            Ok(_) => set_staging(&thread_layout, Value::Null),
            Err(error) => {
                let code = error.downcast_ref::<UpgradeError>().map_or("stage_failed", |e| e.code);
                set_staging(&thread_layout, json!({
                    "release": thread_release, "state": "failed", "finishedAt": now(),
                    "error": { "code": code, "message": format!("{error:#}") },
                }));
            }
        }
        STAGING.store(false, Ordering::SeqCst);
    });
    if let Err(error) = spawned {
        STAGING.store(false, Ordering::SeqCst);
        set_staging(layout, Value::Null);
        return Err(anyhow::anyhow!("could not start staging: {error}"));
    }
    Ok(release)
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let route = match parse_route(&request.method, path) {
        Ok(route) => route,
        Err(true) => return error(stream, 405, "method_not_allowed", "method not allowed on this upgrade route", json!({})),
        Err(false) => return error(stream, 404, "route_not_found", "management v2 route not found", json!({ "path": path })),
    };
    let Some(layout) = layout() else {
        return match unmanaged(&request.method, &route) {
            Ok((status, body)) => write_json_response(stream, status, "OK", &body),
            Err(e) => failure(stream, &e),
        };
    };
    let result: anyhow::Result<(u16, Value)> = match route {
        Route::Status => Ok((200, control::status_view(&layout))),
        Route::Check => check_now(&layout).map(|_| (200, control::status_view(&layout))),
        Route::Stage => {
            let release = match body_object(&request.body, &["release"]).and_then(|o| opt_string(&o, "release")) {
                Ok(release) => release,
                Err(message) => return error(stream, 400, "invalid_request", &message, json!({})),
            };
            start_stage(&layout, release).map(|release| (202, json!({ "staging": { "release": release, "state": "running" } })))
        }
        Route::Apply => {
            let request = match parse_apply(&request.body) {
                Ok(request) => request,
                Err(message) => return error(stream, 400, "invalid_request", &message, json!({})),
            };
            control::prepare_apply(&layout, &crate::upgrade::keys::trusted_keys(), &request)
                .and_then(|run| control::start(&layout, &crate::upgrade::keys::trusted_keys(), run))
                .map(|run| (202, json!({ "run": run })))
        }
        Route::Cancel => control::cancel(&layout).map(|run| (200, json!({ "run": run }))),
        Route::Rollback => {
            let request = match parse_rollback(&request.body) {
                Ok(request) => request,
                Err(message) => return error(stream, 400, "invalid_request", &message, json!({})),
            };
            control::prepare_rollback(&layout, &request).and_then(|(run, preview)| match run {
                None => Ok((200, json!({ "preview": preview }))),
                Some(run) => control::start(&layout, &crate::upgrade::keys::trusted_keys(), run).map(|run| (202, json!({ "run": run, "preview": preview }))),
            })
        }
        Route::Runs => {
            let limit = match crate::http::query::query_value(&request.path, "limit") {
                None => 20,
                Some(raw) => match raw.parse::<usize>() {
                    Ok(limit) if limit > 0 => limit.min(MAX_RUN_LIMIT),
                    _ => return error(stream, 400, "invalid_request", "limit must be a positive integer", json!({})),
                },
            };
            let mut runs = state::history(&layout, limit);
            if let Some(current) = State::load(&layout).run.filter(|r| !r.phase.terminal()) {
                runs.insert(0, current);
                runs.truncate(limit);
            }
            Ok((200, json!({ "runs": runs })))
        }
        Route::Run(id) => match state::read_run(&layout, &id) {
            Some(run) => Ok((200, json!({ "run": run }))),
            None => return error(stream, 404, "run_not_found", "no upgrade run has this id", json!({ "runId": id })),
        },
        Route::Settings if request.method == "GET" => Ok((200, json!({ "settings": Settings::load(&layout).view(&layout) }))),
        Route::Settings => managed(&layout).and_then(|_| {
            let mut settings = Settings::load(&layout);
            if let Err((code, message)) = patch_settings(&mut settings, &request.body) {
                return Err(stage::upgrade_error(400, code, message));
            }
            settings.save(&layout)?;
            Ok((200, json!({ "settings": settings.view(&layout) })))
        }),
    };
    match result {
        Ok((status, body)) => write_json_response(stream, status, if status == 202 { "Accepted" } else { "OK" }, &body),
        Err(e) => failure(stream, &e),
    }
}

/// Event params for a state change (small; clients re-read GET /v2/upgrade).
pub fn event_params(state: &State, reason: &str) -> Value {
    json!({
        "reason": reason,
        "hostId": crate::platform::host::current_host_name(),
        "run": state.run.as_ref().map(|run| json!({
            "runId": run.run_id,
            "kind": run.kind,
            "phase": run.phase,
            "fromRelease": run.from_release,
            "toRelease": run.to_release,
            "servicesDown": run.phase.services_down(),
        })),
        "available": state.available.as_ref().map(|a| a.release.clone()),
        "staged": state.staged.as_ref().map(|s| s.release.clone()),
        "staging": state.extra.get(STAGING_KEY).cloned(),
    })
}

fn publish(params: Value) {
    let Ok(repository) = super::repository::management_v2_repository() else { return };
    if let Err(error) = super::server::append_cutex_event(
        repository,
        super::services::HOST_EVENT_SESSION_ID,
        super::model::EventCorrelation::default(),
        UPDATED_EVENT,
        params,
    ) {
        eprintln!("upgrade: could not publish event: {error:#}");
    }
}

fn due(last_check_at: Option<&str>, hours: u32) -> bool {
    if hours == 0 {
        return false;
    }
    match last_check_at.and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()) {
        None => true,
        Some(at) => chrono::Utc::now() - at.with_timezone(&chrono::Utc) >= chrono::Duration::hours(hours as i64),
    }
}

/// Periodic check → auto-stage → auto-apply (owner settings; managed
/// installs with a trusted key only).
fn scheduled(layout: &Layout) {
    if !layout.is_managed() || crate::upgrade::keys::trusted_keys().is_empty() {
        return;
    }
    let settings = Settings::load(layout);
    let state = State::load(layout);
    if !due(state.last_check_at.as_deref(), settings.check_interval_hours) || OperationLock::held(layout) {
        return;
    }
    if check_now(layout).is_err() {
        return;
    }
    let state = State::load(layout);
    let Some(available) = state.available else { return };
    // A release the owner rolled back, or whose apply rolled back / failed
    // here (transient failures: a few retries), is neither staged nor
    // applied automatically again; the owner still can.
    if crate::upgrade::state::auto_upgrade_blocked(layout, &available.release).is_some() {
        return;
    }
    let staged = state.staged.as_ref().is_some_and(|s| s.release == available.release);
    if settings.auto_stage && !staged && available.below_floor.is_none() {
        let _ = start_stage(layout, Some(available.release.clone()));
        return;
    }
    if settings.auto_apply && staged && available.class != crate::upgrade::manifest::MigrationClass::OneWay {
        let request = ApplyRequest { release: Some(available.release), requested_by: Some("auto-apply".into()), ..Default::default() };
        if let Ok(run) = control::prepare_apply(layout, &crate::upgrade::keys::trusted_keys(), &request) {
            let _ = control::start(layout, &crate::upgrade::keys::trusted_keys(), run);
        }
    }
}

/// Management serve: state watcher (events) + scheduler.
pub fn start_watcher() -> anyhow::Result<()> {
    std::thread::Builder::new().name("cutex-upgrade-watch".into()).spawn(|| {
        let mut stamp: Option<SystemTime> = None;
        let mut last_schedule = Instant::now() - Duration::from_secs(3600);
        // A run this daemon restarted for: announce its state once.
        let mut first = true;
        loop {
            // Detected every round: an unmanaged daemon has nothing to watch
            // (and never a stand-in path to read or act on).
            let Some(layout) = layout() else {
                first = false;
                std::thread::sleep(WATCH_INTERVAL);
                continue;
            };
            let current = std::fs::metadata(layout.state_path()).and_then(|m| m.modified()).ok();
            if current != stamp || (first && current.is_some()) {
                stamp = current;
                if current.is_some() {
                    publish(event_params(&State::load(&layout), if first { "startup" } else { "state" }));
                }
            }
            first = false;
            if last_schedule.elapsed() >= Duration::from_secs(300) {
                last_schedule = Instant::now();
                scheduled(&layout);
            }
            std::thread::sleep(WATCH_INTERVAL);
        }
    })?;
    Ok(())
}

#[cfg(test)]
#[path = "upgrade_api_tests.rs"]
mod tests;
