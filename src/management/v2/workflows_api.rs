//! Workflow runs in `cutex management serve` (cutex `workflows`, W1).
//!
//! - The real [`StepBackend`]: a step = a hidden quick-chat-style session
//!   (`HumanCreateSessionRequest.hidden`, the author's sandbox, approval policy
//!   `never` so a step never waits on an approval nobody sees) + an owner ask.
//!   Steps are archived when the run ends. The author's completion notice is
//!   an `automation` user input (`[workflow completion] …`), started at once
//!   when the author is idle, else after its turn.
//! - The service tick (1 s): approvals, runs submitted by the Agent Bus
//!   process, stop requests, retention. The store is created lazily (first
//!   submission); until then the tick only checks for the file.
//! - Routes (root credential):
//!   - `GET /v2/workflows/runs?state=open|closed|all&sessionId=&projectId=&limit=`
//!   - `GET /v2/workflows/runs/{id}[?include=steps,log,result,script]`
//!   - `GET /v2/workflows/runs/{id}/script` · `/result` · `/log?after=&limit=` · `/steps/{key}`
//!   - `POST /v2/workflows/runs` (owner start: `{script, args?, cwd, projectId?, name?, limits?, defaultProfile?, resumeFromRunId?}`)
//!   - `POST /v2/workflows/runs/{id}/stop` · `/resume`
//!   - `DELETE /v2/workflows/runs/{id}` (finished runs; archives step sessions)
//! - Host events: `cutex/workflow/run/changed`, `cutex/workflow/step/changed`,
//!   `cutex/workflow/log` (all with `hostId`), plus `cutex/approvals/changed`.

use std::collections::HashMap;
use std::net::TcpStream;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::management::server::ManagementRequestContext;
use crate::workflows::asks_bridge::{self, AskPoll};
use crate::workflows::engine::{self, ApiError, ExecMode, Facts, StartRequest, StepBackend, StepHandle, StepStart, WorkflowService};
use crate::workflows::facts::LibraryFacts;
use crate::workflows::model::*;
use crate::workflows::store::{RunFilter, WorkflowStore};

pub const PATH: &str = "/v2/workflows/runs";
const TICK: Duration = Duration::from_secs(1);

static CONTEXT: OnceLock<ManagementRequestContext> = OnceLock::new();

fn publish(method: &str, mut params: Value) {
    let Ok(repository) = super::repository::management_v2_repository() else { return };
    if let Some(object) = params.as_object_mut() {
        object.insert("hostId".into(), json!(crate::platform::host::current_host_name()));
    }
    if let Err(error) = super::server::append_cutex_event(
        repository,
        super::services::HOST_EVENT_SESSION_ID,
        super::model::EventCorrelation::default(),
        method,
        params,
    ) {
        eprintln!("workflows: could not publish {method}: {error:#}");
    }
}

fn run_error(error: &super::user_input::UserInputExecutionError) -> RunError {
    RunError { code: error.code.clone(), message: error.message.clone() }
}

struct ManagementSteps {
    context: ManagementRequestContext,
}

fn display_name(step: &StepStart) -> String {
    let label = step.label.clone().unwrap_or_else(|| crate::asks::title_of(&step.title));
    let raw = format!("⧉ {} #{} · {}", step.run_name, step.run_seq, label);
    let cleaned: String = raw.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let mut out: String = cleaned.trim().chars().take(64).collect();
    out = out.trim().to_string();
    if out.is_empty() {
        "workflow step".into()
    } else {
        out
    }
}

impl ManagementSteps {
    fn launch(&self, step: &StepStart, claude: bool) -> crate::config::new_thread::LaunchOverrides {
        let mut launch = crate::config::new_thread::LaunchOverrides { model: step.model.clone(), reasoning_effort: step.effort.clone(), ..Default::default() };
        if claude {
            return launch;
        }
        // Permissions ⊆ the author's; never wait on an approval prompt.
        launch.approval_policy = Some("never".into());
        if let Some(author) = &step.author_session {
            let record = crate::session::store::with_cutex_session_store(|store| store.sessions.get(author).cloned()).ok().flatten();
            if let Some(record) = record {
                launch.sandbox = record.sandbox_mode.clone();
                launch.network_access = record.sandbox_network_access;
                if !record.sandbox_writable_roots.is_empty() {
                    launch.writable_roots = Some(record.sandbox_writable_roots.clone());
                }
            }
        }
        if launch.sandbox.is_none() {
            launch.sandbox = Some("workspace-write".into());
        }
        launch
    }
}

impl StepBackend for ManagementSteps {
    fn start(&self, step: &StepStart) -> Result<StepHandle, RunError> {
        let claude = match &step.profile {
            Some(profile) => LibraryFacts::new().profile_facts(std::slice::from_ref(profile))?.first().is_some_and(|f| f.runtime == "claude"),
            None => false,
        };
        let request_id = format!("wf-{}", crate::workflows::model::sha256_hex(step.action_id.as_bytes()));
        let request = super::human_sessions::HumanCreateSessionRequest {
            request_id,
            cwd: Some(step.cwd.clone()),
            profile: step.profile.clone(),
            display_name: Some(display_name(step)),
            start: true,
            launch: Some(self.launch(step, claude)),
            hidden: true,
        };
        let principal = crate::management::control_plane::HumanManagementPrincipal::authenticated();
        let session = (self.context.create_human_session)(&principal, &request).map_err(|e| run_error(&e))?;
        let store = crate::asks::store::AskStore::open_default().map_err(|e| RunError { code: "asks_unavailable".into(), message: format!("{e:#}") })?;
        let ask = asks_bridge::create_step_ask(&store, &session, &step.action_id, &step.title, &step.brief)?;
        super::asks::publish_pending_notices(&store);
        super::admission::changed(&session);
        Ok(StepHandle { session_id: session, ask_id: ask })
    }

    fn follow_up(&self, session_id: &str, action_id: &str, title: &str, text: &str) -> Result<String, RunError> {
        let store = crate::asks::store::AskStore::open_default().map_err(|e| RunError { code: "asks_unavailable".into(), message: format!("{e:#}") })?;
        let ask = asks_bridge::create_step_ask(&store, session_id, action_id, title, text)?;
        super::asks::publish_pending_notices(&store);
        super::admission::changed(session_id);
        Ok(ask)
    }

    fn poll(&self, ask_ids: &[String]) -> HashMap<String, AskPoll> {
        match crate::asks::store::AskStore::open_default() {
            Ok(store) => asks_bridge::poll(&store, ask_ids),
            Err(_) => HashMap::new(),
        }
    }

    fn stop(&self, ask_id: &str) {
        if let Ok(store) = crate::asks::store::AskStore::open_default() {
            asks_bridge::stop_ask(&store, ask_id);
            super::asks::publish_pending_notices(&store);
        }
    }

    fn release(&self, session_ids: &[String]) {
        for session in session_ids {
            if let Err(error) = (self.context.mutate_session)(session, "cutex/session/retire", json!({"reason": "workflow run ended"})) {
                if error.code != "session_not_found" {
                    eprintln!("workflows: could not archive step session {session}: {}", error.message);
                }
            }
        }
    }

    fn notify_author(&self, session_id: &str, run_id: &str, text: &str) -> Result<(), String> {
        let repository = super::repository::management_v2_repository().map_err(|e| format!("{e:#}"))?;
        let registry = (self.context.load_registry)().map_err(|e| format!("{e:#}"))?;
        let resource = super::session::session_resource_including_hidden(session_id, &registry, self.context.load_runtime_status, repository)
            .map_err(|e| format!("{e:#}"))?
            .ok_or("author session not found")?;
        let client_id = format!("workflow-completion-{run_id}");
        let request = super::server::ValidatedCutexRequest {
            request_id: client_id.clone(),
            method: "cutex/userInput/submit".into(),
            params: json!({
                "clientUserMessageId": client_id,
                "origin": {"kind": "automation", "clientId": "cutex-workflows"},
                "strategy": "auto",
                "input": [{"type": "text", "text": text, "text_elements": []}],
            }),
        };
        super::offline_submit::dispatch(
            repository,
            self.context,
            Some(crate::management::control_plane::HumanManagementPrincipal::authenticated()),
            session_id,
            &resource,
            &request,
        )
        .map(|_| ())
        .map_err(|e| format!("{}: {}", e.code, e.message))
    }
}

fn exec_mode() -> ExecMode {
    match std::env::current_exe() {
        Ok(exe) => ExecMode::Process(exe),
        Err(_) => ExecMode::InProcess,
    }
}

fn settings() -> engine::WorkflowSettings {
    crate::config::store::load_codez_config().workflows
}

static SERVICE_INIT: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The host's service; `create` opens (and creates) the store.
fn service(create: bool) -> Option<Arc<WorkflowService>> {
    if let Some(service) = engine::service() {
        return Some(service);
    }
    let context = *CONTEXT.get()?;
    let _guard = SERVICE_INIT.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(service) = engine::service() {
        return Some(service);
    }
    let root = WorkflowStore::default_root().ok()?;
    if !create && !root.join("runs.sqlite3").exists() {
        return None;
    }
    let store = match WorkflowStore::open(&root) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("workflows: store unavailable: {error:#}");
            return None;
        }
    };
    let service = WorkflowService::new(
        store,
        Arc::new(ManagementSteps { context }),
        Arc::new(LibraryFacts::new()),
        exec_mode(),
        crate::approvals::ApprovalStore::open_default().ok(),
        Arc::new(publish),
        Arc::new(settings),
    );
    engine::install(service.clone());
    service.recover();
    Some(service)
}

/// Start the tick thread (management serve).
pub fn start(context: ManagementRequestContext) {
    let _ = CONTEXT.set(context);
    let spawned = std::thread::Builder::new().name("cutex-workflows".into()).spawn(|| loop {
        if let Some(service) = service(false) {
            service.tick();
        }
        std::thread::sleep(TICK);
    });
    if let Err(error) = spawned {
        eprintln!("workflows tick warning: {error}");
    }
}

// ---------------------------------------------------------------------------
// HTTP

#[derive(Clone, Debug, PartialEq, Eq)]
enum Route {
    Collection,
    Run(String),
    Script(String),
    Result(String),
    Log(String),
    Step(String, String),
    Stop(String),
    Resume(String),
}

fn valid_run_id(id: &str) -> bool {
    id.starts_with("wr-") && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn valid_step_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 64 && key.bytes().all(|b| b.is_ascii_hexdigit() || b == b'#')
}

fn parse_path(path: &str) -> Option<Route> {
    if path == PATH {
        return Some(Route::Collection);
    }
    let rest = path.strip_prefix("/v2/workflows/runs/")?;
    let mut parts = rest.splitn(3, '/');
    let id = parts.next()?.to_string();
    if !valid_run_id(&id) {
        return None;
    }
    match (parts.next(), parts.next()) {
        (None, _) => Some(Route::Run(id)),
        (Some("script"), None) => Some(Route::Script(id)),
        (Some("result"), None) => Some(Route::Result(id)),
        (Some("log"), None) => Some(Route::Log(id)),
        (Some("stop"), None) => Some(Route::Stop(id)),
        (Some("resume"), None) => Some(Route::Resume(id)),
        (Some("steps"), Some(key)) => {
            let key = key.replace("%23", "#");
            valid_step_key(&key).then_some(Route::Step(id, key))
        }
        _ => None,
    }
}

pub fn is_path(path: &str) -> bool {
    path == PATH || path.starts_with("/v2/workflows/")
}

fn query(path: &str) -> HashMap<String, String> {
    path.split_once('?')
        .map(|(_, q)| url::form_urlencoded::parse(q.as_bytes()).into_owned().collect())
        .unwrap_or_default()
}

fn reason(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        _ => "Service Unavailable",
    }
}

fn api_error(stream: &mut TcpStream, error: ApiError) -> anyhow::Result<()> {
    let details = if error.details.is_null() { json!({}) } else { error.details.clone() };
    super::server::write_v2_error(stream, error.status, reason(error.status), &error.code, &error.message, error.status == 503, details)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OwnerStart {
    script: String,
    #[serde(default)]
    args: Value,
    cwd: String,
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    limits: Option<LimitsRequest>,
    #[serde(default)]
    default_profile: Option<String>,
    #[serde(default)]
    resume_from_run_id: Option<String>,
}

/// Socket entry point; the caller has verified the root credential.
pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let Some(route) = parse_path(path) else {
        return api_error(stream, ApiError::new(404, "not_found", "unknown workflows route"));
    };
    let query = query(&request.path);
    // Reads without a store answer empty; writes create it.
    let write = request.method != "GET";
    let Some(service) = service(write) else {
        return match (request.method.as_str(), &route) {
            ("GET", Route::Collection) => write_json_response(stream, 200, "OK", &json!({"hostId": crate::platform::host::current_host_name(), "runs": []})),
            _ if CONTEXT.get().is_none() => api_error(stream, ApiError::new(503, "workflows_unavailable", "workflows are not running in this process")),
            _ => api_error(stream, ApiError::new(404, "run_not_found", "no such workflow run")),
        };
    };
    let store = service.store();
    let result: Result<Value, ApiError> = match (request.method.as_str(), route) {
        ("GET", Route::Collection) => {
            let open = match query.get("state").map(String::as_str) {
                None | Some("all") => None,
                Some("open") => Some(true),
                Some("closed") => Some(false),
                Some(_) => return api_error(stream, ApiError::new(400, "invalid_request", "state: open, closed or all")),
            };
            let limit = query.get("limit").and_then(|l| l.parse::<usize>().ok()).unwrap_or(50).clamp(1, 500);
            store
                .runs(&RunFilter { open, author_session: query.get("sessionId").cloned(), project_id: query.get("projectId").cloned(), limit })
                .map(|runs| json!({"hostId": crate::platform::host::current_host_name(), "runs": runs.iter().map(engine::run_summary).collect::<Vec<_>>()}))
                .map_err(|e| ApiError::new(503, "workflows_unavailable", format!("{e:#}")))
        }
        ("GET", Route::Run(id)) => {
            let include: Vec<String> = query.get("include").map(|v| v.split(',').map(str::to_string).collect()).unwrap_or_default();
            let include: Vec<&str> = include.iter().map(String::as_str).collect();
            service.run_view(&id, &include).map(|run| json!({"run": run}))
        }
        ("GET", Route::Script(id)) => match store.run(&id) {
            Ok(Some(run)) => store
                .read_script(&run.script_sha256)
                .map(|script| json!({"runId": id, "script": script, "scriptSha256": run.script_sha256, "draftPath": run.draft_path}))
                .map_err(|e| ApiError::new(503, "workflows_unavailable", format!("{e:#}"))),
            Ok(None) => Err(ApiError::new(404, "run_not_found", "no such workflow run")),
            Err(e) => Err(ApiError::new(503, "workflows_unavailable", format!("{e:#}"))),
        },
        ("GET", Route::Result(id)) => match store.run(&id) {
            Ok(Some(run)) => Ok(json!({"runId": id, "state": run.state, "result": run.result})),
            Ok(None) => Err(ApiError::new(404, "run_not_found", "no such workflow run")),
            Err(e) => Err(ApiError::new(503, "workflows_unavailable", format!("{e:#}"))),
        },
        ("GET", Route::Log(id)) => {
            let after = query.get("after").and_then(|v| v.parse().ok()).unwrap_or(0);
            let limit = query.get("limit").and_then(|v| v.parse().ok()).unwrap_or(500usize).clamp(1, 2000);
            store.logs(&id, after, limit).map(|lines| json!({"runId": id, "lines": lines})).map_err(|e| ApiError::new(503, "workflows_unavailable", format!("{e:#}")))
        }
        ("GET", Route::Step(id, key)) => match store.step(&id, &key) {
            Ok(Some(step)) => Ok(json!({"step": step})),
            Ok(None) => Err(ApiError::new(404, "step_not_found", "no such step")),
            Err(e) => Err(ApiError::new(503, "workflows_unavailable", format!("{e:#}"))),
        },
        ("POST", Route::Collection) => match serde_json::from_slice::<OwnerStart>(&request.body) {
            Ok(body) => service
                .submit(StartRequest {
                    author: Some(Author::Owner),
                    script: body.script,
                    draft_path: None,
                    args: body.args,
                    name: body.name,
                    limits: body.limits.unwrap_or_default(),
                    default_profile: body.default_profile,
                    resume_from_run_id: body.resume_from_run_id,
                    cwd: Some(body.cwd),
                    project_id: body.project_id,
                    start_action_id: None,
                })
                .map(|outcome| json!({"run": engine::run_summary(&outcome.run), "warnings": outcome.warnings})),
            Err(error) => Err(ApiError::new(400, "invalid_request", error.to_string())),
        },
        ("POST", Route::Stop(id)) => service.stop(&id, StopReason::User).map(|run| json!({"run": engine::run_summary(&run)})),
        ("POST", Route::Resume(id)) => service.resume(&id, true).map(|outcome| json!({"run": engine::run_summary(&outcome.run), "warnings": outcome.warnings})),
        ("DELETE", Route::Run(id)) => service.delete(&id).map(|()| json!({"runId": id, "deleted": true})),
        _ => Err(ApiError::new(405, "method_not_allowed", "method not allowed for this workflows route")),
    };
    match result {
        Ok(body) => write_json_response(stream, 200, "OK", &body),
        Err(error) => api_error(stream, error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_parse_strictly() {
        assert_eq!(parse_path("/v2/workflows/runs"), Some(Route::Collection));
        assert_eq!(parse_path("/v2/workflows/runs/wr-abc"), Some(Route::Run("wr-abc".into())));
        assert_eq!(parse_path("/v2/workflows/runs/wr-abc/stop"), Some(Route::Stop("wr-abc".into())));
        assert_eq!(parse_path("/v2/workflows/runs/wr-abc/steps/0a1b%231"), Some(Route::Step("wr-abc".into(), "0a1b#1".into())));
        assert_eq!(parse_path("/v2/workflows/runs/../x"), None);
        assert_eq!(parse_path("/v2/workflows/runs/wr-a/other"), None);
        assert!(is_path("/v2/workflows/runs/wr-a"));
    }

    #[test]
    fn step_names_are_bounded_and_clean() {
        let step = StepStart {
            run_id: "wr-1".into(),
            run_name: "a\nvery long workflow name that keeps going and going and going".into(),
            run_seq: 12,
            step_key: "k#1".into(),
            label: Some("label".into()),
            profile: None,
            model: None,
            effort: None,
            cwd: "/tmp".into(),
            author_session: None,
            title: "t".into(),
            brief: "b".into(),
            action_id: "a".into(),
        };
        let name = display_name(&step);
        assert!(name.chars().count() <= 64 && !name.contains('\n') && name.starts_with("⧉ a very"), "{name}");
    }
}
