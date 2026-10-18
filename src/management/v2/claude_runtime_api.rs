//! Owner view and action for moving one Claude agent to the host's selected
//! Claude runtime (capability `claude-runtime-repin`); the HTTP form of
//! `cutex human claude repin <id> [--dry-run]`. The rules are
//! `runtime::claude_pin::repin` (shared with the CLI, nothing re-implemented
//! here):
//!
//! - `GET /v2/sessions/{id}/claude-runtime` (root): the session's pinned
//!   runtime, the selected runtime for new sessions, a pending change, and
//!   what a repin now would do (`dryRun`: `applied` / `deferred` /
//!   `unchanged` / `skipped` with the refusal reason). Nothing changes.
//! - `POST /v2/sessions/{id}/claude-runtime/repin` (owner only) `{target?}`:
//!   performs it. An offline agent moves at once; a running agent is never
//!   restarted or interrupted: the change is saved as pending and applies at
//!   its next fresh runtime start (`restartRequired: true`; the client may
//!   offer the existing runtime restart). Refusals (another Claude config
//!   home, a schema-2 adapter state on a pre-r8 target, a lifecycle
//!   operation in progress) answer 409 `claude_repin_refused` and change
//!   nothing. `target` (a release label such as `claude-r19`) guards against
//!   the selected runtime changing after the client showed it (409
//!   `claude_runtime_changed`).
//!
//! Releases are runtime directory labels (`claude-r19`), never full paths.

use std::net::TcpStream;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::runtime::claude::ClaudeDeployment;
use crate::runtime::claude_pin::{self, PendingPin, RepinOutcome, RepinReport, PENDING_FILE};
use crate::session::model::{CutexSessionRecord, CutexSessionRuntimeBackend};

pub const CAPABILITY: &str = "claude-runtime-repin";

pub const EXPORTED_SCHEMA: super::schema_export::ExportedSchema = super::schema_export::ExportedSchema {
    name: "claude-runtime",
    title: "Claude agent runtime pin",
    routes: &[
        "GET /v2/sessions/{id}/claude-runtime",
        "POST /v2/sessions/{id}/claude-runtime/repin",
    ],
    text: include_str!("schema/export/claude-runtime.json"),
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    View(String),
    Repin(String),
}

fn decode_id(encoded: &str) -> Option<String> {
    if encoded.is_empty() || encoded.contains('/') {
        return None;
    }
    let decoded = super::attachments::percent_decode(encoded)?;
    (!decoded.is_empty() && !decoded.contains('/') && !decoded.chars().any(char::is_control)).then_some(decoded)
}

pub fn parse_path(path: &str) -> Option<Route> {
    let rest = path.split('?').next()?.strip_prefix("/v2/sessions/")?;
    if let Some(encoded) = rest.strip_suffix("/claude-runtime/repin") {
        return decode_id(encoded).map(Route::Repin);
    }
    rest.strip_suffix("/claude-runtime").and_then(decode_id).map(Route::View)
}

pub fn is_path(path: &str) -> bool {
    parse_path(path).is_some()
}

/// `claude-r19` for a deployment (its runtime directory).
pub fn release_of(deployment: &ClaudeDeployment) -> String {
    super::runtimes_api::claude_runtime_label(&deployment.adapter_source.to_string_lossy())
}

/// `upgrade` / `downgrade` by release number, `change` when the labels are
/// equal but the pin differs (another CLI or TUI), `none` when nothing would
/// change.
fn direction(current: &str, target: &str, changes: bool) -> &'static str {
    use super::runtimes_api::natural_key;
    if !changes {
        return "none";
    }
    match natural_key(Some(target)).cmp(&natural_key(Some(current))) {
        std::cmp::Ordering::Greater => "upgrade",
        std::cmp::Ordering::Less => "downgrade",
        std::cmp::Ordering::Equal => "change",
    }
}

fn outcome_name(outcome: RepinOutcome) -> &'static str {
    match outcome {
        RepinOutcome::Unchanged => "unchanged",
        RepinOutcome::Applied => "applied",
        RepinOutcome::Deferred => "deferred",
        RepinOutcome::Skipped => "skipped",
    }
}

fn report_json(report: &RepinReport) -> Value {
    json!({
        "outcome": outcome_name(report.outcome),
        "reason": report.reason,
        "restartRequired": report.outcome == RepinOutcome::Deferred,
    })
}

fn read_pending(record: &CutexSessionRecord) -> Option<PendingPin> {
    let session = record.claude_session.as_ref()?;
    let raw = std::fs::read(session.state_dir.join(PENDING_FILE)).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// Capability `session-claude-runtime`: session resources carry their pin
/// and `GET /v2/sessions` the host's selected release, so a client can
/// decide whether to offer the upgrade without this route.
pub const SESSION_FIELD_CAPABILITY: &str = "session-claude-runtime";

/// `runtime.claudeRuntime` of a Claude session's resource: `{release,
/// pendingRelease}` (pending = a deferred repin waiting for the next fresh
/// start; null when none or stale). None for other sessions.
pub fn session_pin_json(record: &CutexSessionRecord) -> Option<Value> {
    let session = record.claude_session.as_ref()?;
    let pending = read_pending(record)
        .filter(|pending| pending.previous == session.deployment)
        .map(|pending| release_of(&pending.deployment));
    Some(json!({"release": release_of(&session.deployment), "pendingRelease": pending}))
}

/// The `claudeRuntime` block of `GET /v2/sessions`: the release the host
/// uses for new Claude sessions (null when none is installed or usable).
pub fn host_selected_json() -> Value {
    json!({"selectedRelease": ClaudeDeployment::selected().ok().map(|deployment| release_of(&deployment))})
}

/// A refused request: HTTP status, error code, message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
    pub details: Value,
}

impl Refusal {
    fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self { status, code, message: message.into(), details: json!({}) }
    }
}

/// The session record, if it is a local Claude agent.
pub fn claude_record(id: &str) -> Result<CutexSessionRecord, Refusal> {
    let record = match crate::session::store::with_cutex_session_store(|store| store.sessions.get(id).cloned()) {
        Ok(Some(record)) => record,
        Ok(None) => return Err(Refusal::new(404, "session_not_found", "no such session")),
        Err(error) => return Err(Refusal::new(503, "session_store_unavailable", format!("session store: {error:#}"))),
    };
    if record.runtime_backend != CutexSessionRuntimeBackend::ClaudeSubscription || record.claude_session.is_none() {
        return Err(Refusal::new(409, "unsupported_runtime", "the Claude runtime applies to Claude agents only"));
    }
    if !crate::runtime::lifecycle::cutex_session_host_is_local(&record.host_id, &crate::platform::host::current_host_name()) {
        return Err(Refusal::new(409, "remote_session", format!("the agent runs on host {}", record.host_id)));
    }
    Ok(record)
}

/// The GET body for `record` against `selected` (the host's deployment for
/// new sessions, or why there is none). Runs the dry run of `repin`.
pub fn view(record: &CutexSessionRecord, selected: &anyhow::Result<ClaudeDeployment>) -> Value {
    let session = record.claude_session.as_ref().expect("claude session");
    let current = release_of(&session.deployment);
    let pending = read_pending(record).filter(|pending| pending.previous == session.deployment);
    let pending_json = pending.as_ref().map(|pending| json!({
        "release": release_of(&pending.deployment),
        "requestedAt": pending.requested_at,
    }));
    let mut body = json!({
        "cutexSessionId": record.cutex_session_id,
        "name": crate::session::metadata::cutex_session_presented_name(record),
        "current": {"release": current, "missing": !session.deployment.adapter_source.is_dir()},
        "pending": pending_json,
        "selected": null,
        "selectedError": null,
        "direction": "none",
        "upgradeAvailable": false,
        "dryRun": null,
    });
    let target = match selected {
        Ok(target) => target,
        Err(error) => {
            body["selectedError"] = json!(format!("{error:#}"));
            return body;
        }
    };
    let target_release = release_of(target);
    body["selected"] = json!({"release": target_release});
    let next = claude_pin::plan_pin(&session.deployment, &claude_pin::for_session(&session.deployment, target))
        .ok()
        .flatten();
    let dry = claude_pin::repin(&record.cutex_session_id, target, true);
    let changes = match &dry {
        Ok(report) => report.outcome != RepinOutcome::Unchanged,
        Err(_) => next.is_some(),
    };
    let direction = direction(&current, &target_release, changes);
    body["direction"] = json!(direction);
    let already_pending = pending.as_ref().zip(next.as_ref()).is_some_and(|(pending, next)| pending.deployment == *next);
    body["upgradeAvailable"] = json!(matches!(direction, "upgrade" | "change") && !already_pending);
    body["dryRun"] = match &dry {
        Ok(report) => report_json(report),
        Err(error) => json!({"outcome": "skipped", "reason": format!("{error:#}"), "restartRequired": false}),
    };
    body
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepinRequest {
    /// The release the client showed (`claude-r19`); refused when the
    /// selected runtime is another one by now.
    #[serde(default)]
    pub target: Option<String>,
}

/// Perform the repin to `selected` (the POST body on success).
pub fn repin_now(
    id: &str,
    request: &RepinRequest,
    selected: anyhow::Result<ClaudeDeployment>,
) -> Result<Value, Refusal> {
    let before = claude_record(id)?;
    let target = selected.map_err(|error| {
        Refusal::new(409, "claude_runtime_not_installed", format!("no usable Claude runtime is selected for new sessions: {error:#}"))
    })?;
    let target_release = release_of(&target);
    if let Some(expected) = request.target.as_deref() {
        if expected != target_release {
            let mut refusal = Refusal::new(409, "claude_runtime_changed",
                format!("the selected Claude runtime is {target_release} now, not {expected}"));
            refusal.details = json!({"selected": target_release});
            return Err(refusal);
        }
    }
    let previous = release_of(&before.claude_session.as_ref().unwrap().deployment);
    let report = claude_pin::repin(id, &target, false)
        .map_err(|error| Refusal::new(409, "claude_repin_refused", format!("{error:#}")))?;
    if report.outcome == RepinOutcome::Skipped {
        let reason = report.reason.clone().unwrap_or_else(|| "refused".into());
        let mut refusal = Refusal::new(409, "claude_repin_refused", reason.clone());
        refusal.details = json!({"outcome": "skipped", "reason": reason, "previous": previous, "target": target_release});
        return Err(refusal);
    }
    let after = claude_record(id)?;
    let mut result = report_json(&report);
    result["previous"] = json!(previous);
    result["target"] = json!(target_release);
    Ok(json!({"result": result, "runtime": view(&after, &Ok(target))}))
}

fn write_refusal(stream: &mut TcpStream, refusal: Refusal) -> anyhow::Result<()> {
    let reason = match refusal.status {
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        _ => "Service Unavailable",
    };
    super::server::write_v2_error(stream, refusal.status, reason, refusal.code, &refusal.message,
        refusal.status == 503, refusal.details)
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    match (parse_path(&request.path), request.method.as_str()) {
        (Some(Route::View(id)), "GET") => match claude_record(&id) {
            Ok(record) => write_json_response(stream, 200, "OK", &view(&record, &ClaudeDeployment::selected())),
            Err(refusal) => write_refusal(stream, refusal),
        },
        (Some(Route::Repin(id)), "POST") => {
            let body: RepinRequest = if request.body.iter().all(u8::is_ascii_whitespace) {
                RepinRequest::default()
            } else {
                match serde_json::from_slice(&request.body) {
                    Ok(body) => body,
                    Err(error) => return write_refusal(stream, Refusal::new(400, "invalid_request", format!("invalid body: {error}"))),
                }
            };
            match repin_now(&id, &body, ClaudeDeployment::selected()) {
                Ok(body) => write_json_response(stream, 200, "OK", &body),
                Err(refusal) => write_refusal(stream, refusal),
            }
        }
        (Some(Route::View(_)), _) => write_refusal(stream,
            Refusal::new(405, "method_not_allowed", "use GET for /v2/sessions/{id}/claude-runtime")),
        (Some(Route::Repin(_)), _) => write_refusal(stream,
            Refusal::new(405, "method_not_allowed", "use POST for /v2/sessions/{id}/claude-runtime/repin")),
        (None, _) => write_refusal(stream, Refusal::new(400, "invalid_request", "an exact session id is required")),
    }
}

#[cfg(test)]
#[path = "claude_runtime_api_tests.rs"]
mod tests;
