//! The run engine (design §3): submission, approval, the per-run driver,
//! limits, journal, stop, restart recovery and resume.
//!
//! One driver thread per running run owns the run record while it runs. It
//! spawns the script child, answers its `agent()` requests (cache, limits,
//! queueing), starts steps through a [`StepBackend`] (hidden step session +
//! owner ask), polls them, validates schema-shaped answers (one repair ask),
//! and finally notifies the author.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::asks_bridge::AskPoll;
use super::model::*;
use super::script;
use super::store::{RunFilter, WorkflowStore};
use crate::approvals::{ApprovalKind, ApprovalRequest, ApprovalState, ApprovalStore, GateOutcome};

pub const RUN_CHANGED_EVENT: &str = "cutex/workflow/run/changed";
pub const STEP_CHANGED_EVENT: &str = "cutex/workflow/step/changed";
pub const LOG_EVENT: &str = "cutex/workflow/log";
/// Completion notices carry the result inline up to this size.
pub const NOTICE_RESULT_BYTES: usize = 16 * 1024;
const POLL_EVERY: Duration = Duration::from_millis(1000);
const STOP_SETTLE: Duration = Duration::from_secs(90);
/// Finished runs (and their step sessions) are deleted this long after they
/// end; `None` keeps them until the owner deletes them (owner decision
/// 2026-10-23: user data is not deleted on a timer; was 30 days). The
/// storage page will make this a setting.
pub const FINISHED_RUN_RETENTION_DAYS: Option<i64> = None;
const PROVIDER_BLOCK_GRACE: Duration = Duration::from_secs(120);
const ALLOWED_OPTS: &[&str] = &["label", "phase", "schema", "profile", "model", "effort"];

/// Host settings (`config.json` `workflows`, design §3.3 / §5.5).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WorkflowSettings {
    pub default_max_agents: u32,
    pub default_max_concurrency: u32,
    pub default_max_tokens: u64,
    /// Highest `maxTokens` a run may ask for (owner setting).
    pub max_tokens_ceiling: u64,
    /// All runs together.
    pub max_concurrent_steps: u32,
    /// Agent-started runs need the owner's approval of the plan.
    pub require_approval: bool,
    pub auto_resume_after_restart: bool,
}

impl Default for WorkflowSettings {
    fn default() -> Self {
        Self {
            default_max_agents: DEFAULT_MAX_AGENTS,
            default_max_concurrency: DEFAULT_MAX_CONCURRENCY,
            default_max_tokens: DEFAULT_MAX_TOKENS,
            max_tokens_ceiling: 20_000_000,
            max_concurrent_steps: DEFAULT_HOST_MAX_CONCURRENT_STEPS,
            require_approval: true,
            auto_resume_after_restart: true,
        }
    }
}

/// (min, max) of each numeric setting.
const SETTING_RANGES: &[(&str, u64, u64)] = &[
    ("defaultMaxAgents", 1, CEILING_MAX_AGENTS as u64),
    ("defaultMaxConcurrency", 1, CEILING_MAX_CONCURRENCY as u64),
    ("defaultMaxTokens", 10_000, 1_000_000_000),
    ("maxTokensCeiling", 10_000, 1_000_000_000),
    ("maxConcurrentSteps", 1, 64),
];

impl WorkflowSettings {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    /// Strict partial update (`PATCH /v2/settings` `workflows`).
    pub fn patched(&self, value: &Value) -> Result<Self, String> {
        let object = value.as_object().ok_or("workflows must be an object")?;
        let mut next = serde_json::to_value(self).map_err(|e| e.to_string())?;
        for (key, item) in object {
            if let Some((_, min, max)) = SETTING_RANGES.iter().find(|(name, ..)| name == key) {
                let n = item.as_u64().ok_or(format!("workflows.{key} must be an integer"))?;
                if n < *min || n > *max {
                    return Err(format!("workflows.{key} must be between {min} and {max}"));
                }
            } else if matches!(key.as_str(), "requireApproval" | "autoResumeAfterRestart") {
                if !item.is_boolean() {
                    return Err(format!("workflows.{key} must be a boolean"));
                }
            } else {
                return Err(format!("unknown workflows setting {key}"));
            }
            next[key] = item.clone();
        }
        let next: Self = serde_json::from_value(next).map_err(|e| e.to_string())?;
        if next.default_max_tokens > next.max_tokens_ceiling {
            return Err("workflows.defaultMaxTokens must not exceed maxTokensCeiling".into());
        }
        Ok(next)
    }

    /// `{workflows, workflowsDefault, workflowsRange}` for `GET /v2/settings`.
    pub fn view(&self) -> Value {
        let ranges: serde_json::Map<String, Value> = SETTING_RANGES.iter().map(|(k, min, max)| (k.to_string(), json!([min, max]))).collect();
        json!({
            "workflows": self,
            "workflowsDefault": Self::default(),
            "workflowsRange": ranges,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileFact {
    pub name: String,
    /// `codex` | `claude`.
    pub runtime: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AuthorFacts {
    pub name: Option<String>,
    pub profile: Option<String>,
    pub cwd: String,
    pub project_id: Option<String>,
    /// Step sessions never get workflow tools (one level only).
    pub is_step_session: bool,
}

#[derive(Clone, Debug)]
pub struct StepStart {
    pub run_id: String,
    pub run_name: String,
    pub run_seq: u64,
    pub step_key: String,
    pub label: Option<String>,
    pub profile: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub cwd: String,
    pub author_session: Option<String>,
    pub title: String,
    pub brief: String,
    pub action_id: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StepHandle {
    pub session_id: String,
    pub ask_id: String,
}

/// Facts a submission needs (both processes). [`LibraryFacts`] reads the
/// profile and session stores; tests use a fake.
pub trait Facts: Send + Sync {
    fn profile_facts(&self, names: &[String]) -> Result<Vec<ProfileFact>, RunError>;
    fn author_facts(&self, session_id: &str) -> Option<AuthorFacts>;
}

/// What a run needs from the rest of cutex. The real one (cli_app) creates
/// hidden sessions and owner asks; tests use a fake.
pub trait StepBackend: Send + Sync {
    /// Create the step session and its first ask (slow: seconds).
    fn start(&self, step: &StepStart) -> Result<StepHandle, RunError>;
    /// Another ask to the same step session (the schema repair).
    fn follow_up(&self, session_id: &str, action_id: &str, title: &str, text: &str) -> Result<String, RunError>;
    fn poll(&self, ask_ids: &[String]) -> HashMap<String, AskPoll>;
    fn stop(&self, ask_id: &str);
    /// The run ended: archive its step sessions.
    fn release(&self, session_ids: &[String]);
    /// Queue the completion notice to the author agent.
    fn notify_author(&self, session_id: &str, run_id: &str, text: &str) -> Result<(), String>;
}

#[derive(Clone)]
pub enum ExecMode {
    /// `<exe> workflow exec` (production).
    Process(PathBuf),
    /// The same loop on a thread (tests).
    InProcess,
}

pub type EventSink = Arc<dyn Fn(&str, Value) + Send + Sync>;
pub type SettingsLoader = Arc<dyn Fn() -> WorkflowSettings + Send + Sync>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiError {
    pub status: u16,
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub details: Value,
}

impl ApiError {
    pub fn new(status: u16, code: &str, message: impl Into<String>) -> Self {
        Self { status, code: code.into(), message: message.into(), details: Value::Null }
    }
    fn with(mut self, details: Value) -> Self {
        self.details = details;
        self
    }
}

fn internal(error: anyhow::Error) -> ApiError {
    ApiError::new(503, "workflows_unavailable", format!("{error:#}"))
}

/// A start request (agent tool or owner route).
#[derive(Clone, Debug, Default)]
pub struct StartRequest {
    pub author: Option<Author>,
    pub script: String,
    pub draft_path: Option<String>,
    pub args: Value,
    pub name: Option<String>,
    pub limits: LimitsRequest,
    pub default_profile: Option<String>,
    pub resume_from_run_id: Option<String>,
    /// Owner runs: cwd / project (agent runs use the author's).
    pub cwd: Option<String>,
    pub project_id: Option<String>,
    /// Agent tool call id (idempotent retries).
    pub start_action_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartOutcome {
    pub run: RunRecord,
    pub warnings: Vec<String>,
    #[serde(skip)]
    pub approval: Option<crate::approvals::ApprovalRecord>,
    #[serde(skip)]
    pub approval_created: bool,
}

struct Driver {
    stop: AtomicBool,
    stop_reason: Mutex<Option<StopReason>>,
    wake: Mutex<Option<Sender<Msg>>>,
}

pub struct WorkflowService {
    store: WorkflowStore,
    backend: Arc<dyn StepBackend>,
    facts: Arc<dyn Facts>,
    exec: ExecMode,
    approvals: Option<ApprovalStore>,
    events: EventSink,
    settings: SettingsLoader,
    drivers: Mutex<HashMap<String, Arc<Driver>>>,
    host_running: Mutex<u32>,
    /// Last emitted revision per run (runs written by the Agent Bus process
    /// are announced by the tick).
    announced: Mutex<HashMap<String, u64>>,
    last_prune: Mutex<Option<Instant>>,
    poll_every: Mutex<Duration>,
}

static SERVICE: OnceLock<Arc<WorkflowService>> = OnceLock::new();

/// Install the host's service (management serve). Returns false when one exists.
pub fn install(service: Arc<WorkflowService>) -> bool {
    SERVICE.set(service).is_ok()
}

pub fn service() -> Option<Arc<WorkflowService>> {
    SERVICE.get().cloned()
}

enum Msg {
    Child(Value),
    ChildGone,
    Started { key: String, result: Result<StepHandle, RunError> },
    Wake,
}

fn new_run_id() -> String {
    format!("wr-{}", &uuid::Uuid::new_v4().simple().to_string()[..16])
}

fn validate_schema(schema: &Value) -> Result<(), String> {
    let object = schema.as_object().ok_or("schema must be a JSON Schema object")?;
    if object.get("type").and_then(Value::as_str) != Some("object") {
        return Err("schema needs {type: 'object'} at its root".into());
    }
    let properties = object.get("properties").and_then(Value::as_object).ok_or("schema needs `properties` at its root")?;
    if let Some(required) = object.get("required") {
        let required = required.as_array().ok_or("schema `required` must be an array")?;
        for name in required {
            let name = name.as_str().ok_or("schema `required` must list property names")?;
            if !properties.contains_key(name) {
                return Err(format!("schema `required` names \"{name}\", which is not in `properties`"));
            }
        }
    }
    jsonschema::validator_for(schema).map_err(|e| format!("invalid schema: {e}"))?;
    Ok(())
}

/// The first JSON object in an answer (bare or fenced).
pub fn parse_json_answer(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        if value.is_object() {
            return Some(value);
        }
    }
    let bytes = trimmed.as_bytes();
    let mut start = 0;
    while let Some(offset) = trimmed[start..].find('{') {
        let begin = start + offset;
        let mut stream = serde_json::Deserializer::from_slice(&bytes[begin..]).into_iter::<Value>();
        if let Some(Ok(value)) = stream.next() {
            if value.is_object() {
                return Some(value);
            }
        }
        start = begin + 1;
    }
    None
}

fn schema_errors(schema: &Value, value: &Value) -> Vec<String> {
    match jsonschema::validator_for(schema) {
        Ok(validator) => validator.iter_errors(value).take(8).map(|e| format!("{} at {}", e, e.instance_path)).collect(),
        Err(e) => vec![format!("invalid schema: {e}")],
    }
}

fn brief(run: &RunRecord, prompt: &str, schema: Option<&Value>) -> String {
    let mut text = format!(
        "You are one step of the cutex workflow \"{}\" (run {}). Your final message is the return value handed back to the workflow script, not a message to a person: no greeting, no follow-up questions. If you cannot do the task, say so briefly in your final message.\n",
        run.name, run.run_id
    );
    if let Some(schema) = schema {
        text.push_str("Your final message must be only a JSON object (no prose around it) that validates against this JSON Schema:\n```json\n");
        text.push_str(&serde_json::to_string_pretty(schema).unwrap_or_default());
        text.push_str("\n```\n");
    }
    text.push_str("\nTask:\n");
    text.push_str(prompt);
    text
}

fn truncate_utf8(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut cut = max;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}… [truncated: {} of {} bytes]", &text[..cut], cut, text.len())
}

impl WorkflowService {
    pub fn new(
        store: WorkflowStore,
        backend: Arc<dyn StepBackend>,
        facts: Arc<dyn Facts>,
        exec: ExecMode,
        approvals: Option<ApprovalStore>,
        events: EventSink,
        settings: SettingsLoader,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            backend,
            facts,
            exec,
            approvals,
            events,
            settings,
            drivers: Mutex::new(HashMap::new()),
            host_running: Mutex::new(0),
            announced: Mutex::new(HashMap::new()),
            last_prune: Mutex::new(None),
            poll_every: Mutex::new(POLL_EVERY),
        })
    }

    /// How often running steps are polled (tests use a short interval).
    pub fn set_poll_interval(&self, every: Duration) {
        *self.poll_every.lock().unwrap() = every;
    }

    pub fn store(&self) -> &WorkflowStore {
        &self.store
    }

    pub fn backend(&self) -> &Arc<dyn StepBackend> {
        &self.backend
    }

    pub fn settings(&self) -> WorkflowSettings {
        (self.settings)()
    }

    fn emit(&self, method: &str, params: Value) {
        (self.events)(method, params);
    }

    fn emit_run(&self, run: &RunRecord) {
        self.announced.lock().unwrap().insert(run.run_id.clone(), run.revision);
        self.emit(
            RUN_CHANGED_EVENT,
            json!({
                "runId": run.run_id, "revision": run.revision, "state": run.state, "stopReason": run.stop_reason,
                "phase": run.current_phase, "counts": run.counts, "tokens": run.tokens, "maxTokens": run.limits.max_tokens,
                "authorCutexSessionId": run.author.session(), "approvalId": run.approval_id, "name": run.name, "seq": run.seq,
            }),
        );
    }

    fn emit_step(&self, step: &StepRecord) {
        self.emit(
            STEP_CHANGED_EVENT,
            json!({"runId": step.run_id, "stepKey": step.step_key, "state": step.state, "sessionId": step.session_id, "askId": step.ask_id, "phase": step.phase, "label": step.label}),
        );
    }

    fn save_run(&self, run: &mut RunRecord) {
        if let Err(error) = self.store.save_run(run) {
            eprintln!("workflow {}: save failed: {error:#}", run.run_id);
        }
        self.emit_run(run);
    }

    fn save_step(&self, step: &StepRecord) {
        if let Err(error) = self.store.save_step(step) {
            eprintln!("workflow {}: step save failed: {error:#}", step.run_id);
        }
        self.emit_step(step);
    }

    fn log(&self, run_id: &str, level: &str, text: &str) {
        if let Ok(Some(line)) = self.store.append_log(run_id, level, text) {
            self.emit(LOG_EVENT, json!({"runId": run_id, "seq": line.seq, "level": line.level, "text": line.text, "at": line.at}));
        }
    }

    /// Check, store and either start the run or ask the owner (§3.4).
    pub fn submit(self: &Arc<Self>, request: StartRequest) -> Result<StartOutcome, ApiError> {
        let settings = self.settings();
        let outcome = submit_run(&self.store, self.approvals.as_ref(), self.facts.as_ref(), &settings, request)?;
        if outcome.approval_created {
            if let Some(approval) = &outcome.approval {
                self.emit("cutex/approvals/changed", json!({"reason": "requested", "approval": approval}));
            }
        }
        self.emit_run(&outcome.run);
        if outcome.run.state == RunState::Running {
            self.start_driver(outcome.run.clone(), false);
        }
        Ok(outcome)
    }

    /// Approval decisions, runs submitted by the Agent Bus, stop requests,
    /// events for runs other processes wrote, retention (every second or so).
    pub fn tick(self: &Arc<Self>) {
        let Ok(open) = self.store.open_runs() else { return };
        for mut run in open {
            let has_driver = self.drivers.lock().unwrap().contains_key(&run.run_id);
            let announced = self.announced.lock().unwrap().get(&run.run_id).copied();
            if !has_driver && announced != Some(run.revision) {
                if announced.is_none() && run.state == RunState::PendingApproval {
                    // Submitted by the Agent Bus: announce its approval.
                    if let (Some(approvals), Some(id)) = (&self.approvals, &run.approval_id) {
                        if let Ok(Some(record)) = approvals.get(id, chrono::Utc::now()) {
                            self.emit("cutex/approvals/changed", json!({"reason": "requested", "approval": record}));
                        }
                    }
                }
                self.emit_run(&run);
            }
            if let Ok(Some(reason)) = self.store.stop_request(&run.run_id) {
                let _ = self.store.clear_stop_request(&run.run_id);
                let _ = self.stop(&run.run_id, reason);
                continue;
            }
            match run.state {
                RunState::Running if run.started_at.is_none() && !has_driver => self.start_driver(run, false),
                RunState::PendingApproval => self.check_approval(&mut run),
                _ => {}
            }
        }
        let mut last = self.last_prune.lock().unwrap();
        if last.is_none_or(|at| at.elapsed() > Duration::from_secs(3600)) {
            *last = Some(Instant::now());
            drop(last);
            self.prune();
        }
    }

    fn check_approval(self: &Arc<Self>, run: &mut RunRecord) {
        let (Some(approvals), Some(approval_id)) = (&self.approvals, run.approval_id.clone()) else { return };
        let now = chrono::Utc::now();
        let record = approvals.get(&approval_id, now).ok().flatten();
        match record.as_ref().map(|r| r.state) {
            Some(ApprovalState::Pending) => {}
            Some(ApprovalState::Approved) => {
                let facts = self.facts.profile_facts(&run.step_profiles).unwrap_or_default();
                match approvals.gate(approval_request(run, &facts), now) {
                    Ok(GateOutcome::Approved(record)) => {
                        self.emit("cutex/approvals/changed", json!({"reason": "consumed", "approval": record}));
                        run.state = RunState::Running;
                        self.save_run(run);
                        self.start_driver(run.clone(), false);
                    }
                    Ok(GateOutcome::Denied(_) | GateOutcome::DeniedEarlier(_)) => self.end_unstarted(run.clone(), RunState::Rejected),
                    _ => {}
                }
            }
            Some(ApprovalState::Denied) => self.end_unstarted(run.clone(), RunState::Rejected),
            Some(ApprovalState::Consumed) => {
                // Consumed but the run never started (a crash in between).
                run.state = RunState::Running;
                self.save_run(run);
                self.start_driver(run.clone(), false);
            }
            Some(ApprovalState::Expired) | None => self.end_unstarted(run.clone(), RunState::Expired),
        }
    }

    fn end_unstarted(&self, mut run: RunRecord, state: RunState) {
        run.state = state;
        run.ended_at = Some(now_rfc3339());
        self.save_run(&mut run);
        self.notify(&mut run);
    }

    /// Drop finished runs older than the retention (and their step sessions).
    pub fn prune(&self) {
        self.prune_finished_older_than(FINISHED_RUN_RETENTION_DAYS, chrono::Utc::now());
    }

    /// Delete finished runs that ended more than `days` before `now`
    /// (`None`: keep every run).
    pub(crate) fn prune_finished_older_than(&self, days: Option<i64>, now: chrono::DateTime<chrono::Utc>) {
        let Some(days) = days else { return };
        let Ok(runs) = self.store.runs(&RunFilter { open: Some(false), limit: 10_000, ..Default::default() }) else { return };
        let cutoff = now - chrono::Duration::days(days);
        for run in runs {
            let ended = run.ended_at.as_deref().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok());
            if ended.is_some_and(|t| t.with_timezone(&chrono::Utc) < cutoff) {
                let _ = self.delete(&run.run_id);
            }
        }
    }

    pub fn delete(&self, run_id: &str) -> Result<(), ApiError> {
        let run = self.store.run(run_id).map_err(internal)?.ok_or_else(|| ApiError::new(404, "run_not_found", format!("no workflow run {run_id}")))?;
        if !run.state.is_terminal() {
            return Err(ApiError::new(409, "run_still_open", "stop the run before deleting it"));
        }
        let sessions: Vec<String> = self.store.steps(run_id).map_err(internal)?.into_iter().filter_map(|s| s.session_id).collect();
        self.backend.release(&sessions);
        self.store.delete_run(run_id).map_err(internal)?;
        self.emit(RUN_CHANGED_EVENT, json!({"runId": run_id, "deleted": true}));
        Ok(())
    }

    /// Stop a run (owner = User, author = Model).
    pub fn stop(self: &Arc<Self>, run_id: &str, reason: StopReason) -> Result<RunRecord, ApiError> {
        let mut run = self.store.run(run_id).map_err(internal)?.ok_or_else(|| ApiError::new(404, "run_not_found", format!("no workflow run {run_id}")))?;
        match run.state {
            RunState::PendingApproval => {
                run.state = RunState::Stopped;
                run.stop_reason = Some(reason);
                run.ended_at = Some(now_rfc3339());
                self.save_run(&mut run);
                return Ok(run);
            }
            state if state.is_terminal() => return Ok(run),
            _ => {}
        }
        let driver = self.drivers.lock().unwrap().get(run_id).cloned();
        match driver {
            Some(driver) => {
                *driver.stop_reason.lock().unwrap() = Some(reason);
                driver.stop.store(true, Ordering::SeqCst);
                if let Some(wake) = driver.wake.lock().unwrap().as_ref() {
                    let _ = wake.send(Msg::Wake);
                }
                Ok(self.store.run(run_id).map_err(internal)?.unwrap_or(run))
            }
            None => {
                // No driver (e.g. after a failed recovery): settle directly.
                for step in self.store.steps(run_id).map_err(internal)? {
                    if step.state.is_live() {
                        if let Some(ask) = &step.ask_id {
                            self.backend.stop(ask);
                        }
                    }
                }
                run.state = RunState::Stopped;
                run.stop_reason = Some(reason);
                run.ended_at = Some(now_rfc3339());
                self.save_run(&mut run);
                Ok(run)
            }
        }
    }

    /// Owner "continue": a new run seeded with this run's journal.
    pub fn resume(self: &Arc<Self>, run_id: &str, by_owner: bool) -> Result<StartOutcome, ApiError> {
        let previous = self.store.run(run_id).map_err(internal)?.ok_or_else(|| ApiError::new(404, "run_not_found", format!("no workflow run {run_id}")))?;
        let script = self.store.read_script(&previous.script_sha256).map_err(internal)?;
        let author = if by_owner && previous.author.session().is_none() { Author::Owner } else { previous.author.clone() };
        let request = StartRequest {
            author: Some(author),
            script,
            draft_path: previous.draft_path.clone(),
            args: previous.args.clone(),
            name: Some(previous.name.clone()),
            limits: LimitsRequest {
                max_agents: Some(previous.limits.max_agents),
                max_concurrency: Some(previous.limits.max_concurrency),
                max_tokens: Some(previous.limits.max_tokens),
                hard_stop: Some(previous.limits.hard_stop),
            },
            default_profile: previous.default_profile.clone(),
            resume_from_run_id: Some(previous.run_id.clone()),
            cwd: Some(previous.cwd.clone()),
            project_id: previous.project_id.clone(),
            start_action_id: None,
        };
        self.submit(request)
    }

    /// After a daemon start: continue runs whose steps are all still alive,
    /// mark the others `stopped(interrupted)` (§3.6).
    pub fn recover(self: &Arc<Self>) {
        let settings = self.settings();
        let Ok(open) = self.store.open_runs() else { return };
        for mut run in open {
            if run.state == RunState::PendingApproval {
                continue;
            }
            let steps = self.store.steps(&run.run_id).unwrap_or_default();
            let live: Vec<String> = steps.iter().filter(|s| s.state == StepState::Running).filter_map(|s| s.ask_id.clone()).collect();
            let polled = self.backend.poll(&live);
            let lost = steps.iter().any(|s| s.state == StepState::Starting)
                || polled.values().any(|p| matches!(p, AskPoll::Missing) || matches!(p, AskPoll::Failed { code, .. } if code == "runtime_restart" || code == "outcome_unknown"));
            if settings.auto_resume_after_restart && !lost && run.state == RunState::Running {
                self.log(&run.run_id, "info", "daemon restarted: continuing the run (finished steps are not repeated)");
                self.start_driver(run, true);
            } else {
                for step in &steps {
                    if step.state.is_live() {
                        if let Some(ask) = &step.ask_id {
                            self.backend.stop(ask);
                        }
                    }
                }
                run.state = RunState::Stopped;
                run.stop_reason = Some(StopReason::Interrupted);
                run.ended_at = Some(now_rfc3339());
                self.log(&run.run_id, "warn", "daemon restarted: the run cannot continue by itself (resume it)");
                self.save_run(&mut run);
                self.release_sessions(&run.run_id);
                self.notify(&mut run);
            }
        }
    }

    fn release_sessions(&self, run_id: &str) {
        let sessions: Vec<String> = self.store.steps(run_id).unwrap_or_default().into_iter().filter_map(|s| s.session_id).collect();
        if !sessions.is_empty() {
            self.backend.release(&sessions);
        }
    }

    fn notify(&self, run: &mut RunRecord) {
        let Some(session) = run.author.session().map(str::to_string) else { return };
        if run.notified {
            return;
        }
        let text = completion_text(run);
        match self.backend.notify_author(&session, &run.run_id, &text) {
            Ok(()) => {
                run.notified = true;
                self.save_run(run);
            }
            Err(error) => self.log(&run.run_id, "warn", &format!("completion notice not delivered: {error}")),
        }
    }

    fn try_host_slot(&self) -> bool {
        let limit = self.settings().max_concurrent_steps.max(1);
        let mut running = self.host_running.lock().unwrap();
        if *running >= limit {
            return false;
        }
        *running += 1;
        true
    }

    fn force_host_slot(&self) {
        *self.host_running.lock().unwrap() += 1;
    }

    fn release_host_slot(&self) {
        let mut running = self.host_running.lock().unwrap();
        *running = running.saturating_sub(1);
    }

    fn wake_all(&self) {
        for driver in self.drivers.lock().unwrap().values() {
            if let Some(wake) = driver.wake.lock().unwrap().as_ref() {
                let _ = wake.send(Msg::Wake);
            }
        }
    }

    fn start_driver(self: &Arc<Self>, run: RunRecord, recovering: bool) {
        let driver = Arc::new(Driver { stop: AtomicBool::new(false), stop_reason: Mutex::new(None), wake: Mutex::new(None) });
        self.drivers.lock().unwrap().insert(run.run_id.clone(), driver.clone());
        let service = self.clone();
        let run_id = run.run_id.clone();
        let spawned = std::thread::Builder::new().name(format!("cutex-workflow-{}", &run_id)).spawn(move || {
            let id = run.run_id.clone();
            RunDriver::run(service.clone(), driver, run, recovering);
            service.drivers.lock().unwrap().remove(&id);
            service.wake_all();
        });
        if let Err(error) = spawned {
            eprintln!("workflow {run_id}: driver thread failed: {error}");
        }
    }

    /// Wait until no driver runs (tests).
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.drivers.lock().unwrap().is_empty() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// The run snapshot shown by the API and tools.
    pub fn run_view(&self, run_id: &str, include: &[&str]) -> Result<Value, ApiError> {
        let run = self.store.run(run_id).map_err(internal)?.ok_or_else(|| ApiError::new(404, "run_not_found", format!("no workflow run {run_id}")))?;
        let steps = self.store.steps(run_id).map_err(internal)?;
        let mut view = run_summary(&run);
        let mut phases: Vec<Value> = run
            .phases
            .iter()
            .map(|p| json!({"title": p.title, "detail": p.detail, "total": 0, "done": 0, "running": 0, "failed": 0}))
            .collect();
        for step in &steps {
            let title = step.phase.clone().unwrap_or_default();
            let index = match phases.iter().position(|p| p["title"] == json!(title)) {
                Some(index) => index,
                None => {
                    phases.push(json!({"title": title, "detail": null, "total": 0, "done": 0, "running": 0, "failed": 0}));
                    phases.len() - 1
                }
            };
            let entry = &mut phases[index];
            entry["total"] = json!(entry["total"].as_u64().unwrap_or(0) + 1);
            let bucket = match step.state {
                StepState::Done | StepState::Cached => "done",
                StepState::Failed => "failed",
                StepState::Starting | StepState::Running => "running",
                StepState::Queued => "",
            };
            if !bucket.is_empty() {
                entry[bucket] = json!(entry[bucket].as_u64().unwrap_or(0) + 1);
            }
        }
        view["phaseProgress"] = json!(phases);
        view["steps"] = json!(steps.iter().map(|s| step_view(s, include.contains(&"steps"))).collect::<Vec<_>>());
        if include.contains(&"log") {
            view["log"] = json!(self.store.log_tail(run_id, 200).map_err(internal)?);
        } else {
            view["logTail"] = json!(self.store.log_tail(run_id, 5).map_err(internal)?);
        }
        if include.contains(&"script") {
            view["script"] = json!(self.store.read_script(&run.script_sha256).unwrap_or_default());
        }
        if include.contains(&"result") {
            view["result"] = run.result.clone().unwrap_or(Value::Null);
        }
        Ok(view)
    }
}

/// Check and store a run (both processes: the Agent Bus tool path and the
/// management routes). An approved / approval-free run is stored `running`
/// with no `startedAt`; the management service starts its driver.
pub fn submit_run(
    store: &WorkflowStore,
    approvals: Option<&ApprovalStore>,
    facts_source: &dyn Facts,
    settings: &WorkflowSettings,
    request: StartRequest,
) -> Result<StartOutcome, ApiError> {
    let author = request.author.clone().unwrap_or(Author::Owner);
    let author_facts = match author.session() {
        Some(session) => {
            let facts = facts_source.author_facts(session).ok_or_else(|| ApiError::new(404, "author_not_found", "the author session is unknown"))?;
            if facts.is_step_session {
                return Err(ApiError::new(403, "workflow_nesting", "workflow steps cannot start workflows"));
            }
            Some(facts)
        }
        None => None,
    };
    let checked = script::check(&request.script).map_err(|diagnostics| {
        let path = request.draft_path.clone().unwrap_or_else(|| "<script>".into());
        let lines: Vec<String> = diagnostics.iter().map(|d| d.render(&path)).collect();
        ApiError::new(400, "script_invalid", lines.join("\n")).with(json!({"diagnostics": lines, "scriptPath": request.draft_path}))
    })?;
    let limits = effective_limits(&request.limits, settings)?;
    let predecessor = match &request.resume_from_run_id {
        Some(id) => {
            let previous = store.run(id).map_err(internal)?.ok_or_else(|| ApiError::new(404, "run_not_found", format!("no workflow run {id}")))?;
            if author.session().is_some() && previous.author.session() != author.session() {
                return Err(ApiError::new(403, "not_run_author", "only the run's author may resume it"));
            }
            if !previous.state.is_terminal() {
                return Err(ApiError::new(409, "run_still_open", "stop the run before resuming it"));
            }
            Some(previous)
        }
        None => None,
    };
    let default_profile = request
        .default_profile
        .clone()
        .or_else(|| author_facts.as_ref().and_then(|f| f.profile.clone()));
    let mut step_profiles: Vec<String> = Vec::new();
    for profile in default_profile.iter().chain(checked.profiles.iter()) {
        if !step_profiles.contains(profile) {
            step_profiles.push(profile.clone());
        }
    }
    let facts = facts_source.profile_facts(&step_profiles).map_err(|e| ApiError::new(400, &e.code, e.message))?;
    let cwd = match (&author_facts, &request.cwd) {
        (Some(facts), _) => facts.cwd.clone(),
        (None, Some(cwd)) => cwd.clone(),
        (None, None) => return Err(ApiError::new(400, "invalid_request", "an owner run needs cwd")),
    };
    let project_id = author_facts.as_ref().and_then(|f| f.project_id.clone()).or(request.project_id.clone());
    let script_sha256 = store.put_script(&request.script).map_err(internal)?;
    let args_sha256 = sha256_hex(canonical(&request.args).as_bytes());
    // Resume without a new approval: same author, same script and args, no raised caps,
    // same profiles (§3.4; an edited script asks again).
    let needs_approval = author.session().is_some()
        && settings.require_approval
        && !predecessor.as_ref().is_some_and(|previous| {
            previous.approval_id.is_some()
                && previous.script_sha256 == script_sha256
                && previous.args_sha256 == args_sha256
                && previous.limits.max_agents >= limits.max_agents
                && previous.limits.max_tokens >= limits.max_tokens
                && previous.limits.max_concurrency >= limits.max_concurrency
                && previous.step_profiles.iter().all(|p| step_profiles.contains(p))
                && step_profiles.iter().all(|p| previous.step_profiles.contains(p))
        });
    let now = now_rfc3339();
    let mut run = RunRecord {
        run_id: new_run_id(),
        seq: 0,
        name: request.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or(checked.meta.name.clone()),
        description: checked.meta.description.clone(),
        phases: checked.meta.phases.clone(),
        author: match (&author, &author_facts) {
            (Author::Agent { cutex_session_id, .. }, Some(facts)) => Author::Agent { cutex_session_id: cutex_session_id.clone(), name: facts.name.clone() },
            (author, _) => author.clone(),
        },
        project_id,
        cwd,
        script_sha256,
        args: request.args.clone(),
        args_sha256,
        limits,
        default_profile,
        step_profiles,
        models: checked.models.clone(),
        approval_id: None,
        state: RunState::Running,
        stop_reason: None,
        error: None,
        current_phase: None,
        counts: StepCounts::default(),
        tokens: 0,
        result: None,
        resumed_from: predecessor.as_ref().map(|p| p.run_id.clone()),
        draft_path: request.draft_path.clone(),
        warnings: checked.warnings.iter().map(|w| w.render(request.draft_path.as_deref().unwrap_or("<script>"))).collect(),
        created_at: now.clone(),
        started_at: None,
        ended_at: None,
        start_action_id: request.start_action_id.clone(),
        notified: false,
        revision: 0,
    };
    let mut approval = None;
    let mut approval_created = false;
    if needs_approval {
        let approvals = approvals.ok_or_else(|| ApiError::new(503, "approvals_unavailable", "owner approvals are not available"))?;
        run.state = RunState::PendingApproval;
        let request = approval_request(&run, &facts);
        match approvals.gate(request, chrono::Utc::now()).map_err(internal)? {
            GateOutcome::Pending { record, created } => {
                run.approval_id = Some(record.approval_id.clone());
                approval_created = created;
                approval = Some(record);
            }
            GateOutcome::Approved(record) => {
                run.approval_id = Some(record.approval_id);
                run.state = RunState::Running;
            }
            GateOutcome::Denied(record) | GateOutcome::DeniedEarlier(record) => {
                run.approval_id = Some(record.approval_id);
                run.state = RunState::Rejected;
                run.ended_at = Some(now.clone());
            }
        }
    } else if let Some(previous) = &predecessor {
        run.approval_id = previous.approval_id.clone();
    }
    run.seq = store.insert_run(&run).map_err(internal)?;
    let warnings = run.warnings.clone();
    Ok(StartOutcome { run, warnings, approval, approval_created })
}


fn effective_limits(request: &LimitsRequest, settings: &WorkflowSettings) -> Result<Limits, ApiError> {
    let max_agents = request.max_agents.unwrap_or(settings.default_max_agents);
    let max_concurrency = request.max_concurrency.unwrap_or(settings.default_max_concurrency);
    let max_tokens = request.max_tokens.unwrap_or(settings.default_max_tokens);
    if max_agents == 0 || max_concurrency == 0 || max_tokens == 0 {
        return Err(ApiError::new(400, "invalid_limits", "limits must be positive"));
    }
    if max_tokens > settings.max_tokens_ceiling {
        return Err(ApiError::new(400, "invalid_limits", format!("maxTokens is above the host ceiling {}", settings.max_tokens_ceiling)));
    }
    Ok(Limits {
        max_agents: max_agents.min(CEILING_MAX_AGENTS),
        max_concurrency: max_concurrency.min(CEILING_MAX_CONCURRENCY).min(settings.max_concurrent_steps.max(1)),
        max_tokens,
        hard_stop: request.hard_stop.unwrap_or(false),
    })
}


/// The digest binds the approval to the exact plan (§3.4).
pub(crate) fn approval_request(run: &RunRecord, profiles: &[ProfileFact]) -> ApprovalRequest {
    let claude: Vec<&str> = profiles.iter().filter(|p| p.runtime == "claude").map(|p| p.name.as_str()).collect();
    let bound = json!({
        "runId": run.run_id, "scriptSha256": run.script_sha256, "argsSha256": run.args_sha256,
        "limits": run.limits, "stepProfiles": run.step_profiles, "models": run.models,
    });
    let phases: Vec<Value> = run.phases.iter().map(|p| json!({"title": p.title, "detail": p.detail})).collect();
    ApprovalRequest {
        kind: ApprovalKind::WorkflowRun,
        reason: "workflow_run".into(),
        requester_cutex_session_id: run.author.session().unwrap_or("owner").to_string(),
        requester_name: match &run.author {
            Author::Agent { name, .. } => name.clone(),
            Author::Owner => None,
        },
        project_id: run.project_id.clone(),
        summary: format!("Run workflow {}: {}", run.name, run.description.lines().next().unwrap_or("")),
        details: json!({
            "runId": run.run_id, "seq": run.seq, "name": run.name, "description": run.description,
            "phases": phases, "stepProfiles": run.step_profiles, "claudeProfiles": claude, "models": run.models,
            "defaultProfile": run.default_profile,
            "maxAgents": run.limits.max_agents, "maxConcurrency": run.limits.max_concurrency,
            "maxTokens": run.limits.max_tokens, "hardStop": run.limits.hard_stop,
            "cwd": run.cwd, "scriptSha256": run.script_sha256, "args": run.args, "draftPath": run.draft_path,
            "resumedFrom": run.resumed_from,
        }),
        request_sha256: crate::approvals::request_digest(&bound),
    }
}

pub fn run_summary(run: &RunRecord) -> Value {
    let mut view = serde_json::to_value(run).unwrap_or(Value::Null);
    if let Some(object) = view.as_object_mut() {
        object.remove("result");
        object.insert("hasResult".into(), json!(run.result.is_some()));
        object.insert("resultPreview".into(), json!(run.result.as_ref().map(|r| truncate_utf8(&r.to_string(), 600))));
    }
    view
}

fn step_view(step: &StepRecord, full: bool) -> Value {
    let mut view = serde_json::to_value(step).unwrap_or(Value::Null);
    if let Some(object) = view.as_object_mut() {
        object.remove("opts");
        if !full {
            object.remove("result");
            object.insert("prompt".into(), json!(truncate_utf8(&step.prompt, 300)));
            object.insert("resultPreview".into(), json!(step.result.as_ref().map(|r| truncate_utf8(&r.to_string(), 300))));
        }
    }
    view
}

fn completion_text(run: &RunRecord) -> String {
    let state = match (run.state, run.stop_reason) {
        (RunState::Stopped, Some(reason)) => format!("stopped ({})", serde_json::to_value(reason).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()),
        (state, _) => state.as_str().to_string(),
    };
    let mut text = format!(
        "[workflow completion] Workflow \"{}\" (run {}, #{}) {}.\nSteps: {} done, {} cached, {} failed · tokens {} of {}.\n",
        run.name, run.run_id, run.seq, state, run.counts.done, run.counts.cached, run.counts.failed, run.tokens, run.limits.max_tokens
    );
    if let Some(error) = &run.error {
        text.push_str(&format!("Error {}: {}\n", error.code, truncate_utf8(&error.message, 2000)));
    }
    if let Some(result) = &run.result {
        let rendered = result.to_string();
        if rendered.len() <= NOTICE_RESULT_BYTES {
            text.push_str(&format!("Result (JSON): {rendered}\n"));
        } else {
            text.push_str(&format!("Result: {} bytes; read it with workflow_read(run_id=\"{}\", include=[\"result\"]).\n", rendered.len(), run.run_id));
        }
    }
    if matches!(run.state, RunState::Stopped | RunState::Errored) {
        text.push_str(&format!("To continue from the finished steps: workflow_start with resume_from_run_id=\"{}\" (same script).\n", run.run_id));
    }
    if let Some(path) = &run.draft_path {
        text.push_str(&format!("Script: {path}\n"));
    }
    text
}

struct ChildIo {
    writer: Box<dyn Write + Send>,
    kill: Option<Box<dyn FnOnce() + Send>>,
}

impl ChildIo {
    fn send(&mut self, message: &Value) {
        let _ = writeln!(self.writer, "{message}");
        let _ = self.writer.flush();
    }
    fn kill(&mut self) {
        if let Some(kill) = self.kill.take() {
            kill();
        }
    }
}

fn spawn_child(mode: &ExecMode, tx: Sender<Msg>) -> anyhow::Result<ChildIo> {
    let forward = move |reader: Box<dyn std::io::Read + Send>, tx: Sender<Msg>| {
        std::thread::spawn(move || {
            for line in BufReader::new(reader).lines() {
                let Ok(line) = line else { break };
                if let Ok(value) = serde_json::from_str::<Value>(&line) {
                    if tx.send(Msg::Child(value)).is_err() {
                        return;
                    }
                }
            }
            let _ = tx.send(Msg::ChildGone);
        });
    };
    match mode {
        ExecMode::Process(exe) => {
            let mut child = std::process::Command::new(exe)
                .args(["workflow", "exec"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .env_clear()
                .envs(std::env::vars().filter(|(k, _)| matches!(k.as_str(), "PATH" | "SYSTEMROOT" | "TEMP" | "TMP")))
                .spawn()?;
            let stdin = child.stdin.take().expect("piped stdin");
            let stdout = child.stdout.take().expect("piped stdout");
            forward(Box::new(stdout), tx);
            let child = Arc::new(Mutex::new(child));
            let reaper = child.clone();
            std::thread::spawn(move || loop {
                if let Ok(Some(_)) = reaper.lock().unwrap().try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(500));
            });
            Ok(ChildIo {
                writer: Box::new(stdin),
                kill: Some(Box::new(move || {
                    let _ = child.lock().unwrap().kill();
                })),
            })
        }
        ExecMode::InProcess => {
            let (to_child, child_in) = pipe();
            let (child_out, from_child) = pipe();
            std::thread::spawn(move || {
                let _ = super::exec::run(BufReader::new(child_in), child_out);
            });
            forward(Box::new(from_child), tx);
            Ok(ChildIo { writer: Box::new(to_child), kill: None })
        }
    }
}

/// An in-memory pipe for the in-process child.
fn pipe() -> (PipeWriter, PipeReader) {
    let (tx, rx) = mpsc::channel();
    (PipeWriter(tx), PipeReader { rx, buffer: Vec::new(), at: 0 })
}
struct PipeWriter(Sender<Vec<u8>>);
impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.send(buf.to_vec()).map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
struct PipeReader {
    rx: Receiver<Vec<u8>>,
    buffer: Vec<u8>,
    at: usize,
}
impl std::io::Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.at >= self.buffer.len() {
            match self.rx.recv() {
                Ok(next) => {
                    self.buffer = next;
                    self.at = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.buffer.len() - self.at);
        buf[..n].copy_from_slice(&self.buffer[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

struct RunDriver {
    service: Arc<WorkflowService>,
    driver: Arc<Driver>,
    run: RunRecord,
    tx: Sender<Msg>,
    steps: HashMap<String, StepRecord>,
    order: Vec<String>,
    occurrences: HashMap<String, u32>,
    cache: HashMap<String, StepRecord>,
    /// step key → child request ids waiting for it.
    waiting: HashMap<String, Vec<u64>>,
    queue: VecDeque<String>,
    /// Steps holding a host slot.
    slots: HashSet<String>,
    agents_used: u32,
    next_call: u64,
    child: Option<ChildIo>,
    child_done: bool,
    blocked_since: Option<Instant>,
}

impl RunDriver {
    fn run(service: Arc<WorkflowService>, driver: Arc<Driver>, mut run: RunRecord, recovering: bool) {
        let (tx, rx) = mpsc::channel();
        *driver.wake.lock().unwrap() = Some(tx.clone());
        let own = service.store.steps(&run.run_id).unwrap_or_default();
        let mut cache = HashMap::new();
        if let Some(previous) = &run.resumed_from {
            for step in service.store.steps(previous).unwrap_or_default() {
                if matches!(step.state, StepState::Done | StepState::Cached) {
                    cache.insert(step.step_key.clone(), step);
                }
            }
        }
        let mut steps = HashMap::new();
        let mut order = Vec::new();
        let mut agents_used = 0;
        let mut slots = HashSet::new();
        for step in own {
            if step.state != StepState::Cached {
                agents_used += 1 + step.repairs;
            }
            if step.state.is_live() && recovering {
                service.force_host_slot();
                slots.insert(step.step_key.clone());
            }
            order.push(step.step_key.clone());
            steps.insert(step.step_key.clone(), step);
        }
        if run.started_at.is_none() {
            run.started_at = Some(now_rfc3339());
        }
        let mut this = RunDriver {
            next_call: order.len() as u64,
            service,
            driver,
            run,
            tx,
            steps,
            order,
            occurrences: HashMap::new(),
            cache,
            waiting: HashMap::new(),
            queue: VecDeque::new(),
            slots,
            agents_used,
            child: None,
            child_done: false,
            blocked_since: None,
        };
        // Steps that were queued before a restart are re-requested by the script.
        for step in this.steps.values_mut() {
            if step.state == StepState::Queued {
                step.state = StepState::Failed;
                step.error = Some(RunError { code: "superseded".into(), message: "requeued after restart".into() });
            }
        }
        this.refresh_counts();
        this.service.save_run(&mut this.run);
        this.main(rx);
    }

    fn main(&mut self, rx: Receiver<Msg>) {
        let script = match self.service.store.read_script(&self.run.script_sha256) {
            Ok(script) => script,
            Err(error) => return self.finish_error("script_missing", &format!("{error:#}")),
        };
        let checked = match script::check(&script) {
            Ok(checked) => checked,
            Err(diagnostics) => return self.finish_error("script_invalid", &diagnostics.iter().map(|d| d.render("<script>")).collect::<Vec<_>>().join("\n")),
        };
        let mut child = match spawn_child(&self.service.exec, self.tx.clone()) {
            Ok(child) => child,
            Err(error) => return self.finish_error("exec_failed", &format!("{error:#}")),
        };
        child.send(&json!({"t": "start", "body": checked.body, "args": self.run.args, "budgetTotal": self.run.limits.max_tokens, "spent": self.run.tokens}));
        self.child = Some(child);
        let poll_every = *self.service.poll_every.lock().unwrap();
        let mut last_poll = Instant::now() - poll_every;
        loop {
            match rx.recv_timeout(poll_every.min(Duration::from_millis(250))) {
                Ok(message) => self.handle(message),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            while let Ok(message) = rx.try_recv() {
                self.handle(message);
            }
            if self.driver.stop.load(Ordering::SeqCst) {
                let reason = self.driver.stop_reason.lock().unwrap().unwrap_or(StopReason::User);
                return self.finish_stopped(reason, &rx);
            }
            if last_poll.elapsed() >= poll_every {
                last_poll = Instant::now();
                self.poll();
            }
            self.admit();
            if self.run.state != RunState::Running {
                return;
            }
            if let Some(reason) = self.provider_blocked() {
                self.service.log(&self.run.run_id, "warn", &format!("every running step is blocked by its account ({reason}); stopping the run so it can be resumed after the fix"));
                return self.finish_stopped(StopReason::Provider, &rx);
            }
        }
    }

    /// Every live step has been held by its account (quota / login) for a
    /// while and nothing else can progress: stop as `provider` (§3.1).
    fn provider_blocked(&mut self) -> Option<&'static str> {
        let since = self.blocked_since?;
        (since.elapsed() >= PROVIDER_BLOCK_GRACE && self.queue.is_empty()).then_some("quota or login")
    }

    fn handle(&mut self, message: Msg) {
        match message {
            Msg::Child(value) => self.on_child(value),
            Msg::ChildGone => {
                if !self.child_done && self.run.state == RunState::Running {
                    self.finish_error("script_crashed", "the script process ended without a result");
                }
            }
            Msg::Started { key, result } => self.on_started(key, result),
            Msg::Wake => {}
        }
    }

    fn reply(&mut self, id: u64, result: Result<Value, RunError>) {
        let spent = self.run.tokens;
        let message = match result {
            Ok(value) => json!({"t": "result", "id": id, "ok": true, "value": value, "spent": spent}),
            Err(error) => json!({"t": "result", "id": id, "ok": false, "error": error, "spent": spent}),
        };
        if let Some(child) = self.child.as_mut() {
            child.send(&message);
        }
    }

    fn on_child(&mut self, value: Value) {
        if self.run.state != RunState::Running {
            return;
        }
        match value["t"].as_str() {
            Some("agent") => {
                let id = value["id"].as_u64().unwrap_or(0);
                let prompt = value["prompt"].as_str().unwrap_or_default().to_string();
                let opts = value.get("opts").cloned().unwrap_or(json!({}));
                if let Err(error) = self.on_agent(id, prompt, opts) {
                    self.service.log(&self.run.run_id, "warn", &format!("agent() refused: {} ({})", error.message, error.code));
                    self.reply(id, Err(error));
                }
            }
            Some("log") => {
                let level = if value["level"] == "warn" { "warn" } else { "info" };
                self.service.log(&self.run.run_id, level, value["text"].as_str().unwrap_or_default());
            }
            Some("phase") => {
                self.run.current_phase = value["title"].as_str().map(str::to_string);
                self.service.save_run(&mut self.run);
            }
            Some("done") => {
                self.child_done = true;
                if value["ok"] == json!(true) {
                    let result = value.get("value").cloned().unwrap_or(Value::Null);
                    if result.to_string().len() > MAX_RESULT_BYTES {
                        return self.finish_error("result_too_large", &format!("the script returned more than {} KiB", MAX_RESULT_BYTES / 1024));
                    }
                    self.run.result = Some(result);
                    self.finish(RunState::Completed, None);
                } else {
                    let code = value["error"]["code"].as_str().unwrap_or("script_error").to_string();
                    let message = value["error"]["message"].as_str().unwrap_or("the script failed").to_string();
                    self.finish_error(&code, &message);
                }
            }
            _ => {}
        }
    }

    fn err(code: &str, message: impl Into<String>) -> RunError {
        RunError { code: code.into(), message: message.into() }
    }

    fn on_agent(&mut self, id: u64, prompt: String, opts: Value) -> Result<(), RunError> {
        let object = opts.as_object().cloned().unwrap_or_default();
        for key in object.keys() {
            if key == "isolation" || key == "agentType" {
                return Err(Self::err("unsupported_option", format!("agent() option `{key}` is not available in cutex workflows")));
            }
            if !ALLOWED_OPTS.contains(&key.as_str()) {
                return Err(Self::err("invalid_option", format!("unknown agent() option `{key}` (allowed: {})", ALLOWED_OPTS.join(", "))));
            }
        }
        if prompt.trim().is_empty() {
            return Err(Self::err("invalid_prompt", "agent() needs a non-empty prompt"));
        }
        if prompt.len() > crate::asks::MAX_BRIEF_BYTES - 16 * 1024 {
            return Err(Self::err("prompt_too_large", "the prompt is larger than the step brief limit (240 KiB)"));
        }
        let text_opt = |key: &str| -> Result<Option<String>, RunError> {
            match object.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.clone())),
                Some(_) => Err(Self::err("invalid_option", format!("agent() option `{key}` must be a non-empty string"))),
            }
        };
        let label = text_opt("label")?;
        let phase = text_opt("phase")?;
        let profile = text_opt("profile")?.or_else(|| self.run.default_profile.clone());
        let _model = text_opt("model")?;
        let effort = text_opt("effort")?;
        if let Some(effort) = &effort {
            if !matches!(effort.as_str(), "minimal" | "low" | "medium" | "high" | "xhigh" | "max") {
                return Err(Self::err("invalid_option", "effort must be one of minimal, low, medium, high, xhigh, max"));
            }
        }
        if let Some(schema) = object.get("schema") {
            validate_schema(schema).map_err(|m| Self::err("invalid_schema", m))?;
        }
        if let Some(profile) = &profile {
            if !self.run.step_profiles.contains(profile) {
                return Err(Self::err("profile_not_approved", format!("profile \"{profile}\" is not in this run's approved profiles ({}); use a literal profile option so it appears on the plan", self.run.step_profiles.join(", "))));
            }
        }
        let mut canonical_opts = serde_json::Map::new();
        for (key, value) in &object {
            if key != "label" && key != "phase" {
                canonical_opts.insert(key.clone(), value.clone());
            }
        }
        if let Some(profile) = &profile {
            canonical_opts.insert("profile".into(), json!(profile));
        }
        let canonical_opts = Value::Object(canonical_opts);
        let base = sha256_hex(format!("{prompt}\u{0}{}", canonical(&canonical_opts)).as_bytes())[..16].to_string();
        let n = self.occurrences.entry(base.clone()).or_insert(0);
        *n += 1;
        let key = format!("{base}#{n}");
        // This run's own journal (recovery re-execution).
        if let Some(step) = self.steps.get(&key).cloned() {
            match step.state {
                StepState::Done | StepState::Cached => self.reply(id, Ok(step.result.clone().unwrap_or(Value::Null))),
                StepState::Failed if step.error.as_ref().is_some_and(|e| e.code == "superseded") => {
                    let mut step = step;
                    step.state = StepState::Queued;
                    step.error = None;
                    self.waiting.entry(key.clone()).or_default().push(id);
                    self.queue.push_back(key.clone());
                    self.steps.insert(key, step);
                }
                StepState::Failed => self.reply(id, Ok(Value::Null)),
                _ => self.waiting.entry(key).or_default().push(id),
            }
            return Ok(());
        }
        let now = now_rfc3339();
        let mut step = StepRecord {
            run_id: self.run.run_id.clone(),
            step_key: key.clone(),
            call_index: self.next_call,
            phase,
            label,
            prompt,
            opts: canonical_opts,
            profile,
            session_id: None,
            ask_id: None,
            ask_ids: vec![],
            state: StepState::Queued,
            result: None,
            error: None,
            tokens: 0,
            repairs: 0,
            cached_from: None,
            activity: None,
            created_at: now.clone(),
            started_at: None,
            ended_at: None,
        };
        self.next_call += 1;
        if let Some(cached) = self.cache.get(&key) {
            step.state = StepState::Cached;
            step.result = cached.result.clone();
            step.cached_from = Some(cached.run_id.clone());
            step.ended_at = Some(now);
            let result = step.result.clone().unwrap_or(Value::Null);
            self.insert_step(step);
            self.reply(id, Ok(result));
            return Ok(());
        }
        if self.agents_used >= self.run.limits.max_agents {
            return Err(Self::err("agent_limit", format!("this run may start at most {} agents", self.run.limits.max_agents)));
        }
        if self.run.tokens >= self.run.limits.max_tokens {
            return Err(Self::err("budget_exhausted", format!("the run's token budget ({}) is spent", self.run.limits.max_tokens)));
        }
        self.agents_used += 1;
        self.waiting.entry(key.clone()).or_default().push(id);
        self.queue.push_back(key.clone());
        self.insert_step(step);
        Ok(())
    }

    fn insert_step(&mut self, step: StepRecord) {
        if !self.steps.contains_key(&step.step_key) {
            self.order.push(step.step_key.clone());
        }
        self.service.save_step(&step);
        self.steps.insert(step.step_key.clone(), step);
        self.refresh_counts();
        self.service.save_run(&mut self.run);
    }

    fn update_step(&mut self, step: StepRecord) {
        self.service.save_step(&step);
        self.steps.insert(step.step_key.clone(), step);
    }

    fn refresh_counts(&mut self) {
        let steps: Vec<StepRecord> = self.order.iter().filter_map(|k| self.steps.get(k).cloned()).collect();
        self.run.counts = run_counts(&steps);
        self.run.tokens = steps.iter().map(|s| s.tokens).sum();
    }

    fn running_count(&self) -> u32 {
        self.steps.values().filter(|s| s.state.is_live()).count() as u32
    }

    fn admit(&mut self) {
        if self.run.state != RunState::Running {
            return;
        }
        while !self.queue.is_empty() && self.running_count() < self.run.limits.max_concurrency {
            if !self.service.try_host_slot() {
                break;
            }
            let key = self.queue.pop_front().expect("non-empty");
            let Some(mut step) = self.steps.get(&key).cloned() else {
                self.service.release_host_slot();
                continue;
            };
            self.slots.insert(key.clone());
            step.state = StepState::Starting;
            step.started_at = Some(now_rfc3339());
            let start = StepStart {
                run_id: self.run.run_id.clone(),
                run_name: self.run.name.clone(),
                run_seq: self.run.seq,
                step_key: key.clone(),
                label: step.label.clone(),
                profile: step.profile.clone(),
                model: step.opts.get("model").and_then(Value::as_str).map(str::to_string),
                effort: step.opts.get("effort").and_then(Value::as_str).map(str::to_string),
                cwd: self.run.cwd.clone(),
                author_session: self.run.author.session().map(str::to_string),
                title: format!("⧉ {} #{} · {}", self.run.name, self.run.seq, step.label.clone().unwrap_or_else(|| crate::asks::title_of(&step.prompt))),
                brief: brief(&self.run, &step.prompt, step.opts.get("schema")),
                action_id: format!("{}:{}", self.run.run_id, key),
            };
            self.update_step(step);
            self.refresh_counts();
            self.service.save_run(&mut self.run);
            let backend = self.service.backend.clone();
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let result = backend.start(&start);
                let _ = tx.send(Msg::Started { key: start.step_key.clone(), result });
            });
        }
    }

    fn on_started(&mut self, key: String, result: Result<StepHandle, RunError>) {
        let Some(mut step) = self.steps.get(&key).cloned() else { return };
        match result {
            Ok(handle) => {
                step.session_id = Some(handle.session_id);
                step.ask_ids.push(handle.ask_id.clone());
                step.ask_id = Some(handle.ask_id);
                if self.run.state == RunState::Running && step.state == StepState::Starting {
                    step.state = StepState::Running;
                    self.update_step(step);
                } else {
                    // Stopped meanwhile.
                    if let Some(ask) = &step.ask_id {
                        self.service.backend.stop(ask);
                    }
                    self.update_step(step);
                }
            }
            Err(error) => {
                self.service.log(&self.run.run_id, "warn", &format!("step {} could not start: {} ({})", step.label.clone().unwrap_or_else(|| key.clone()), error.message, error.code));
                self.settle(step, StepState::Failed, None, Some(error));
            }
        }
        self.refresh_counts();
        self.service.save_run(&mut self.run);
    }

    /// Terminal step: release its slot, answer the script.
    fn settle(&mut self, mut step: StepRecord, state: StepState, result: Option<Value>, error: Option<RunError>) {
        step.state = state;
        step.result = result.clone();
        step.error = error;
        step.ended_at = Some(now_rfc3339());
        let key = step.step_key.clone();
        if self.slots.remove(&key) {
            self.service.release_host_slot();
            self.service.wake_all();
        }
        self.update_step(step);
        for id in self.waiting.remove(&key).unwrap_or_default() {
            self.reply(id, Ok(if state == StepState::Done { result.clone().unwrap_or(Value::Null) } else { Value::Null }));
        }
    }

    fn poll(&mut self) {
        let live: Vec<StepRecord> = self.steps.values().filter(|s| s.state == StepState::Running).cloned().collect();
        let asks: Vec<String> = live.iter().filter_map(|s| s.ask_id.clone()).collect();
        if asks.is_empty() {
            return;
        }
        let polled = self.service.backend.poll(&asks);
        let all_blocked = asks.iter().all(|ask| matches!(polled.get(ask), Some(AskPoll::Pending { blocked: true, .. })));
        if all_blocked {
            self.blocked_since.get_or_insert_with(Instant::now);
        } else {
            self.blocked_since = None;
        }
        let before = self.run.tokens;
        for mut step in live {
            let Some(ask) = step.ask_id.clone() else { continue };
            let Some(poll) = polled.get(&ask) else { continue };
            match poll.clone() {
                AskPoll::Pending { activity, tokens, .. } => {
                    let tokens = tokens.max(step.tokens);
                    if activity != step.activity || tokens != step.tokens {
                        step.activity = activity;
                        step.tokens = tokens;
                        self.update_step(step);
                    }
                }
                AskPoll::Answered { text, tokens } => {
                    step.tokens = tokens.max(step.tokens);
                    self.on_answer(step, text);
                }
                AskPoll::Failed { code, message, tokens } => {
                    step.tokens = tokens.max(step.tokens);
                    self.service.log(&self.run.run_id, "warn", &format!("step {} failed: {message} ({code})", step.label.clone().unwrap_or_else(|| step.step_key.clone())));
                    self.settle(step, StepState::Failed, None, Some(RunError { code, message }));
                }
                AskPoll::Missing => {
                    self.settle(step, StepState::Failed, None, Some(RunError { code: "ask_missing".into(), message: "the step's ask is gone".into() }));
                }
            }
        }
        let counts_before = self.run.counts.clone();
        self.refresh_counts();
        let changed = self.run.tokens != before || self.run.counts != counts_before;
        if self.run.tokens != before {
            let spent = self.run.tokens;
            if let Some(child) = self.child.as_mut() {
                child.send(&json!({"t": "spent", "spent": spent}));
            }
            if spent >= self.run.limits.max_tokens && self.run.limits.hard_stop {
                self.service.log(&self.run.run_id, "warn", "token budget spent: stopping the running steps (hardStop)");
                for step in self.steps.values() {
                    if step.state.is_live() {
                        if let Some(ask) = &step.ask_id {
                            self.service.backend.stop(ask);
                        }
                    }
                }
            }
        }
        if changed {
            self.service.save_run(&mut self.run);
        }
    }

    fn on_answer(&mut self, mut step: StepRecord, text: String) {
        let Some(schema) = step.opts.get("schema").cloned() else {
            let result = json!(truncate_utf8(&text, MAX_STEP_RESULT_BYTES));
            return self.settle(step, StepState::Done, Some(result), None);
        };
        let problems = match parse_json_answer(&text) {
            Some(value) => {
                let errors = schema_errors(&schema, &value);
                if errors.is_empty() {
                    if value.to_string().len() > MAX_STEP_RESULT_BYTES {
                        return self.settle(step, StepState::Failed, None, Some(Self::err("result_too_large", "the step's JSON result is larger than 256 KiB")));
                    }
                    return self.settle(step, StepState::Done, Some(value), None);
                }
                errors
            }
            None => vec!["the final message contains no JSON object".to_string()],
        };
        let label = step.label.clone().unwrap_or_else(|| step.step_key.clone());
        let can_repair = step.repairs == 0 && self.agents_used < self.run.limits.max_agents && self.run.tokens < self.run.limits.max_tokens;
        if !can_repair {
            self.service.log(&self.run.run_id, "warn", &format!("step {label}: output does not match the schema (schema_mismatch): {}", problems.join("; ")));
            return self.settle(step, StepState::Failed, None, Some(Self::err("schema_mismatch", problems.join("; "))));
        }
        let text = format!(
            "Your previous final message did not match the required JSON Schema:\n- {}\nReply with only the corrected JSON object, nothing else.",
            problems.join("\n- ")
        );
        let Some(session) = step.session_id.clone() else {
            return self.settle(step, StepState::Failed, None, Some(Self::err("schema_mismatch", problems.join("; "))));
        };
        let action = format!("{}:{}:repair", self.run.run_id, step.step_key);
        let title = format!("⧉ {} #{} · {} (repair)", self.run.name, self.run.seq, label);
        match self.service.backend.follow_up(&session, &action, &title, &text) {
            Ok(ask) => {
                self.agents_used += 1;
                step.repairs += 1;
                step.ask_ids.push(ask.clone());
                step.ask_id = Some(ask);
                step.activity = Some("repairing the JSON output".into());
                self.service.log(&self.run.run_id, "info", &format!("step {label}: output did not match the schema, asked once for a repair"));
                self.update_step(step);
            }
            Err(error) => self.settle(step, StepState::Failed, None, Some(error)),
        }
    }

    fn finish_error(&mut self, code: &str, message: &str) {
        self.run.error = Some(RunError { code: code.into(), message: truncate_utf8(message, 8192) });
        self.service.log(&self.run.run_id, "warn", &format!("script error ({code}): {}", message.lines().next().unwrap_or("")));
        self.finish(RunState::Errored, None);
    }

    /// Stop the child, stop open steps and wait for them to settle.
    fn finish_stopped(&mut self, reason: StopReason, rx: &Receiver<Msg>) {
        self.run.state = RunState::Stopping;
        self.run.stop_reason = Some(reason);
        self.service.save_run(&mut self.run);
        if let Some(child) = self.child.as_mut() {
            child.kill();
        }
        self.child = None;
        self.queue.clear();
        for step in self.steps.values().cloned().collect::<Vec<_>>() {
            if step.state == StepState::Queued {
                self.settle(step, StepState::Failed, None, Some(Self::err("stopped", "the run was stopped")));
            } else if step.state.is_live() {
                if let Some(ask) = &step.ask_id {
                    self.service.backend.stop(ask);
                }
            }
        }
        let deadline = Instant::now() + STOP_SETTLE;
        let poll_every = *self.service.poll_every.lock().unwrap();
        while self.steps.values().any(|s| s.state.is_live()) && Instant::now() < deadline {
            if let Ok(message) = rx.recv_timeout(poll_every.min(Duration::from_millis(250))) {
                if let Msg::Started { key, result } = message {
                    self.on_started(key, result);
                }
            }
            self.poll();
        }
        for step in self.steps.values().cloned().collect::<Vec<_>>() {
            if step.state.is_live() {
                self.settle(step, StepState::Failed, None, Some(Self::err("stopped", "the run was stopped")));
            }
        }
        self.finish(RunState::Stopped, Some(reason));
    }

    fn finish(&mut self, state: RunState, reason: Option<StopReason>) {
        if let Some(child) = self.child.as_mut() {
            child.kill();
        }
        self.child = None;
        // Steps the script never awaited.
        for step in self.steps.values().cloned().collect::<Vec<_>>() {
            if step.state.is_live() {
                if let Some(ask) = &step.ask_id {
                    self.service.backend.stop(ask);
                }
                self.settle(step, StepState::Failed, None, Some(Self::err("abandoned", "the script ended before this step")));
            } else if step.state == StepState::Queued {
                self.settle(step, StepState::Failed, None, Some(Self::err("abandoned", "the script ended before this step")));
            }
        }
        for key in self.slots.drain() {
            let _ = key;
            self.service.release_host_slot();
        }
        self.refresh_counts();
        self.run.state = state;
        self.run.stop_reason = reason;
        self.run.ended_at = Some(now_rfc3339());
        self.service.save_run(&mut self.run);
        self.service.release_sessions(&self.run.run_id);
        let mut run = self.run.clone();
        self.service.notify(&mut run);
        self.run = run;
    }
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
