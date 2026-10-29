//! Stop idle agents (capability `stop-idle-agents`, cutex-desktop
//! docs/design/031-stop-idle-agents.md): which online agents are idle and
//! may be taken offline, and a batch stop of the ones the owner chose.
//!
//! - `GET /v2/runtimes/idle?minIdleSecs=&projectId=`: every online agent
//!   with a verdict (`stoppable` | `caution` | `blocked`) and the reasons.
//! - `POST /v2/runtimes/stop-idle {requestId, sessionIds, minIdleSecs, mode,
//!   dryRun?}`: each listed agent is checked again and stopped unless it is
//!   `blocked` now. `mode`: `wakeable` (default; the normal wake policy may
//!   start it again) or `stopped` (as the owner's own stop: delegations do
//!   not wake it). Idempotent per `requestId`.
//!
//! Only cutex decides what idle means; the desktop shows the verdicts.

use std::collections::{BTreeMap, HashMap};
use std::net::TcpStream;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::management::server::ManagementRequestContext;

pub const FEATURE: &str = "stop-idle-agents";
pub const IDLE_PATH: &str = "/v2/runtimes/idle";
pub const STOP_PATH: &str = "/v2/runtimes/stop-idle";
pub const IDEMPOTENCY_SCOPE: &str = "cutex/runtimes/stop-idle";
/// Agents per stop request.
pub const MAX_BATCH: usize = 100;

/// Background work the runtime reported (Claude tasks, Codex background
/// terminals): stopping the runtime ends it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Background {
    #[default]
    None,
    Running(usize),
    /// The runtime could not tell (older runtime, query failed).
    Unknown,
}

/// What cutex knows about one online agent.
#[derive(Clone, Debug, Default)]
pub struct IdleFacts {
    pub online: bool,
    pub active_turn: bool,
    /// The native thread is not `idle` / `notLoaded` (e.g. compacting).
    pub thread_busy: bool,
    pub owner_queue: usize,
    /// It holds the admission lease or runs a delegation attempt.
    pub handling_delegation: bool,
    /// Delegations queued for it (they would start it again at once).
    pub delegations_waiting: usize,
    /// Approvals / native questions waiting for the owner.
    pub pending_requests: usize,
    /// Open delegation questions it asked or must answer.
    pub open_questions: usize,
    pub background: Background,
    /// It waits for results of delegations it made (delegation_await or open asks).
    pub awaiting_delegations: usize,
    pub scheduled_alarms: usize,
    pub running_jobs: usize,
    pub director: bool,
    /// Only the owner can start it again (explicit launch, no reviewed relaunch).
    pub owner_start_only: bool,
    pub owned_services: usize,
    /// Latest activity: conversation, turn end or runtime start.
    pub idle_since: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Stop it (selected by default).
    Stoppable,
    /// May be stopped, not selected by default.
    Caution,
    /// Never stopped: it would end work or is not idle.
    Blocked,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stoppable => "stoppable",
            Self::Caution => "caution",
            Self::Blocked => "blocked",
        }
    }
}

/// The verdict, its reasons (blocking first) and the idle seconds.
pub fn classify(facts: &IdleFacts, now: DateTime<Utc>, min_idle_secs: u64) -> (Verdict, Vec<&'static str>, Option<i64>) {
    let idle_secs = facts.idle_since.map(|since| (now - since).num_seconds().max(0));
    let mut blocked = Vec::new();
    if !facts.online {
        blocked.push("offline");
    }
    if facts.active_turn {
        blocked.push("activeTurn");
    }
    if facts.thread_busy {
        blocked.push("threadBusy");
    }
    if facts.owner_queue > 0 {
        blocked.push("ownerQueue");
    }
    if facts.handling_delegation {
        blocked.push("handlingDelegation");
    }
    if facts.delegations_waiting > 0 {
        blocked.push("delegationsWaiting");
    }
    if facts.pending_requests > 0 {
        blocked.push("pendingRequest");
    }
    if facts.open_questions > 0 {
        blocked.push("openQuestion");
    }
    if matches!(facts.background, Background::Running(_)) {
        blocked.push("backgroundWork");
    }
    // Unknown idle time counts only for "every idle agent" (0).
    let long_enough = match idle_secs {
        Some(secs) => secs >= min_idle_secs as i64,
        None => min_idle_secs == 0,
    };
    if !long_enough {
        blocked.push("tooRecent");
    }
    let mut caution = Vec::new();
    if facts.awaiting_delegations > 0 {
        caution.push("awaitingDelegations");
    }
    if facts.scheduled_alarms > 0 {
        caution.push("scheduledAlarm");
    }
    if facts.running_jobs > 0 {
        caution.push("runningJobs");
    }
    if facts.director {
        caution.push("director");
    }
    if facts.background == Background::Unknown {
        caution.push("backgroundUnknown");
    }
    let mut notes = Vec::new();
    if facts.owner_start_only {
        notes.push("ownerStartOnly");
    }
    if facts.owned_services > 0 {
        notes.push("ownsServices");
    }
    let verdict = if !blocked.is_empty() {
        Verdict::Blocked
    } else if !caution.is_empty() {
        Verdict::Caution
    } else {
        Verdict::Stoppable
    };
    (verdict, blocked.into_iter().chain(caution).chain(notes).collect(), idle_secs)
}

fn parse_time(value: Option<&Value>) -> Option<DateTime<Utc>> {
    value.and_then(Value::as_str).and_then(crate::asks::parse_time)
}

/// Facts from a session resource (`GET /v2/sessions` row).
pub fn session_facts(session: &Value, facts: &mut IdleFacts) {
    let runtime = session.get("runtime");
    let status = runtime.and_then(|runtime| runtime.get("status")).and_then(Value::as_str);
    facts.online = status == Some("online");
    facts.active_turn = runtime.and_then(|runtime| runtime.get("activeTurnId")).is_some_and(|id| !id.is_null());
    let thread = runtime.and_then(|runtime| runtime.get("threadStatus"));
    let thread_kind = thread.and_then(|status| status.get("type").or(Some(status))).and_then(Value::as_str);
    facts.thread_busy = !matches!(thread_kind, None | Some("idle") | Some("notLoaded"));
    facts.pending_requests = runtime
        .and_then(|runtime| runtime.get("pendingServerRequests"))
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let activity = session.get("activity");
    let latest = [
        parse_time(activity.and_then(|activity| activity.get("lastConversationAt"))),
        parse_time(activity.and_then(|activity| activity.get("lastTurnCompletedAt"))),
        facts.idle_since,
    ];
    facts.idle_since = latest.into_iter().flatten().max();
}

/// Facts from the asks store.
pub fn ask_facts(state: &crate::asks::store::AskState, session: &str, facts: &mut IdleFacts) {
    facts.handling_delegation = state.leases.contains_key(session)
        || state.next_step_leases.contains_key(session)
        || state.asks.values().any(|ask| ask.target_cutex_session_id == session && ask.execution.in_runtime());
    facts.delegations_waiting = state.waiting_for_target(session).len();
    let own_open = state
        .asks
        .values()
        .filter(|ask| ask.requester.session() == Some(session) && ask.is_open())
        .count();
    let waits = state.waits.values().filter(|wait| wait.cutex_session_id == session).count();
    facts.awaiting_delegations = own_open.max(waits);
    facts.open_questions = state
        .questions
        .values()
        .filter(|question| question.state == crate::asks::model::QuestionState::Open)
        .filter(|question| {
            state.asks.get(&question.ask_id).is_some_and(|ask| {
                ask.target_cutex_session_id == session || ask.requester.session() == Some(session)
            })
        })
        .count();
}

/// Scheduled alarms it set or that are addressed to it.
pub fn alarm_count(alarms: &[crate::agent_bus::alarms::Alarm], session: &str) -> usize {
    alarms
        .iter()
        .filter(|alarm| alarm.state == "scheduled" && (alarm.owner == session || alarm.to == session))
        .count()
}

/// Background work from a native list reply: Claude `claude/tasks/list`
/// (`{tasks: [{status}]}`) or Codex `thread/backgroundTerminals/list`
/// (`{data: [...]}`); an error reply or another shape is `Unknown`.
pub fn background_from_reply(reply: &Value) -> Background {
    let Some(result) = reply.get("result") else {
        return Background::Unknown;
    };
    if let Some(tasks) = result.get("tasks").and_then(Value::as_array) {
        let running = tasks
            .iter()
            .filter(|task| matches!(task.get("status").and_then(Value::as_str), Some("running" | "pending")))
            .count();
        return if running > 0 { Background::Running(running) } else { Background::None };
    }
    if let Some(rows) = result.get("data").and_then(Value::as_array) {
        return if rows.is_empty() { Background::None } else { Background::Running(rows.len()) };
    }
    Background::Unknown
}

fn probe_background(context: ManagementRequestContext, session: &Value) -> Background {
    let (Some(id), Some(thread)) = (
        session.get("cutexSessionId").and_then(Value::as_str),
        session.pointer("/native/threadId").and_then(Value::as_str),
    ) else {
        return Background::Unknown;
    };
    let claude = session.pointer("/runtime/backend").and_then(Value::as_str) == Some("claude_subscription");
    let method = if claude { "claude/tasks/list" } else { "thread/backgroundTerminals/list" };
    let message = json!({
        "id": format!("cutex-stop-idle-{}", uuid::Uuid::new_v4()),
        "method": method,
        "params": {"threadId": thread},
    });
    match (context.forward_native_request)(id, message) {
        Ok(reply) => background_from_reply(&reply),
        Err(_) => Background::Unknown,
    }
}

/// Everything shared by all agents of one listing.
struct HostFacts {
    asks: Option<crate::asks::store::AskState>,
    alarms: Vec<crate::agent_bus::alarms::Alarm>,
    agents: HashMap<String, crate::asks::children::AgentFacts>,
    services: HashMap<String, usize>,
    jobs: Option<HashMap<String, usize>>,
}

impl HostFacts {
    fn load() -> Self {
        let asks = crate::asks::store::AskStore::open_default().and_then(|store| store.snapshot()).ok();
        let alarms = crate::agent_bus::alarms::store_path()
            .and_then(|path| crate::agent_bus::alarms::read_all_at(&path))
            .unwrap_or_default();
        let agents = crate::asks::children::load_facts()
            .unwrap_or_default()
            .into_iter()
            .map(|facts| (facts.cutex_session_id.clone(), facts))
            .collect();
        let mut services = HashMap::new();
        if let Ok(registry) = crate::services::store::ServiceStore::open_default().and_then(|store| Ok(store.load()?)) {
            for record in registry.services.values() {
                if let Some(owner) = record.owner_cutex_session_id.as_deref() {
                    *services.entry(owner.to_string()).or_insert(0) += 1;
                }
            }
        }
        Self { asks, alarms, agents, services, jobs: super::human_jobs::running_jobs_by_session() }
    }

    fn facts(&self, context: ManagementRequestContext, session: &Value, record_started: Option<DateTime<Utc>>) -> IdleFacts {
        let id = session.get("cutexSessionId").and_then(Value::as_str).unwrap_or_default();
        let mut facts = IdleFacts { idle_since: record_started, ..IdleFacts::default() };
        session_facts(session, &mut facts);
        if let Some(state) = &self.asks {
            ask_facts(state, id, &mut facts);
        }
        if let Ok(queue) = super::user_input::user_input_repository().and_then(|queue| queue.list(id)) {
            facts.owner_queue = queue.1.len();
        }
        facts.scheduled_alarms = alarm_count(&self.alarms, id);
        if let Some(agent) = self.agents.get(id) {
            facts.director = agent.director;
            facts.owner_start_only = agent.explicit_launch && !agent.reviewed_relaunch;
        }
        facts.owned_services = self.services.get(id).copied().unwrap_or(0);
        facts.running_jobs = self.jobs.as_ref().and_then(|jobs| jobs.get(id)).copied().unwrap_or(0);
        // Only an otherwise idle online agent needs the (live) background query.
        let (verdict, ..) = classify(&facts, Utc::now(), 0);
        if facts.online && verdict != Verdict::Blocked {
            facts.background = probe_background(context, session);
        }
        facts
    }
}

fn runtime_started(record: &crate::session::model::CutexSessionRecord) -> Option<DateTime<Utc>> {
    record
        .app_server_runtime
        .as_ref()
        .and_then(|runtime| crate::asks::parse_time(&runtime.started_at))
}

/// One listed agent.
fn agent_json(session: &Value, facts: &IdleFacts, now: DateTime<Utc>, min_idle_secs: u64) -> Value {
    let (verdict, reasons, idle_secs) = classify(facts, now, min_idle_secs);
    let backend = session.pointer("/runtime/backend").and_then(Value::as_str);
    json!({
        "cutexSessionId": session.get("cutexSessionId"),
        "name": session.get("displayName"),
        "projectId": session.get("projectId"),
        "runtime": if backend == Some("claude_subscription") { "claude" } else { "codex" },
        "idleSince": facts.idle_since.map(|since| since.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        "idleSecs": idle_secs,
        "verdict": verdict.as_str(),
        "reasons": reasons,
    })
}

/// Online, active (not retired) sessions with their record's runtime start.
fn online_sessions(context: ManagementRequestContext) -> anyhow::Result<Vec<(Value, Option<DateTime<Utc>>)>> {
    let repository = super::repository::management_v2_repository()?;
    let registry = (context.load_registry)()?;
    let list = super::session::session_list_resource_with_hidden(&registry, context.load_runtime_status, repository, true)?;
    let store = crate::session::store::load_cutex_session_store()?;
    let started: HashMap<&str, Option<DateTime<Utc>>> = store
        .sessions
        .values()
        .map(|record| (record.cutex_session_id.as_str(), runtime_started(record)))
        .collect();
    Ok(list
        .get("sessions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|session| session.get("lifecycle").and_then(Value::as_str) != Some("retired"))
        .filter(|session| session.pointer("/runtime/status").and_then(Value::as_str) == Some("online"))
        .map(|session| {
            let id = session.get("cutexSessionId").and_then(Value::as_str).unwrap_or_default();
            (session.clone(), started.get(id).copied().flatten())
        })
        .collect())
}

fn query_param<'a>(params: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    params.get(key).map(String::as_str).filter(|value| !value.is_empty())
}

fn parse_min_idle(value: Option<&str>) -> Result<u64, String> {
    match value {
        None => Ok(0),
        Some(value) => value.parse::<u64>().map_err(|_| "minIdleSecs must be a non-negative integer".to_string()),
    }
}

/// `GET /v2/runtimes/idle`.
pub fn handle_idle(stream: &mut TcpStream, request: &SimpleHttpRequest, context: ManagementRequestContext) -> anyhow::Result<()> {
    let params = super::workspace::query_params(request);
    if let Some(key) = params.keys().find(|key| !matches!(key.as_str(), "minIdleSecs" | "projectId")) {
        return super::server::write_v2_error(stream, 400, "Bad Request", "invalid_request", &format!("unknown parameter: {key}"), false, json!({}));
    }
    let min_idle = match parse_min_idle(query_param(&params, "minIdleSecs")) {
        Ok(min) => min,
        Err(message) => return super::server::write_v2_error(stream, 400, "Bad Request", "invalid_request", &message, false, json!({})),
    };
    let project = query_param(&params, "projectId");
    let now = Utc::now();
    let host = HostFacts::load();
    let agents: Vec<Value> = online_sessions(context)?
        .iter()
        .filter(|(session, _)| project.is_none_or(|project| session.get("projectId").and_then(Value::as_str) == Some(project)))
        .map(|(session, started)| agent_json(session, &host.facts(context, session, *started), now, min_idle))
        .collect();
    write_json_response(stream, 200, "OK", &json!({
        "now": now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "minIdleSecs": min_idle,
        "projectId": project,
        "agents": agents,
    }))
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StopBody {
    request_id: String,
    session_ids: Vec<String>,
    #[serde(default)]
    min_idle_secs: u64,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    dry_run: bool,
}

/// Validate a stop body: `(mode stopped?, body)`.
fn parse_stop(body: &[u8]) -> Result<(bool, StopBody), String> {
    let body: StopBody = serde_json::from_slice(body).map_err(|error| format!("invalid body: {error}"))?;
    if body.request_id.is_empty() || body.request_id.len() > 128 {
        return Err("requestId must be 1..128 characters".into());
    }
    if body.session_ids.is_empty() || body.session_ids.len() > MAX_BATCH {
        return Err(format!("sessionIds must list 1..{MAX_BATCH} agents"));
    }
    let stopped = match body.mode.as_deref() {
        None | Some("wakeable") => false,
        Some("stopped") => true,
        Some(_) => return Err("mode must be wakeable or stopped".into()),
    };
    Ok((stopped, body))
}

fn stop_one(context: ManagementRequestContext, request_id: &str, session: &Value, owner_stop: bool) -> Result<(), String> {
    let id = session.get("cutexSessionId").and_then(Value::as_str).unwrap_or_default();
    let repository = super::repository::management_v2_repository().map_err(|error| format!("{error:#}"))?;
    let generation = session.pointer("/runtime/runtimeGeneration").and_then(Value::as_u64).unwrap_or(0);
    let request = super::server::ValidatedCutexRequest {
        request_id: format!("{request_id}:{id}"),
        method: "cutex/runtime/offline".to_string(),
        params: json!({"expectedRuntimeGeneration": generation, "reason": "stop idle agents"}),
    };
    if owner_stop {
        // As the owner's own stop: delegations do not wake it.
        crate::asks::wake::note_owner_runtime_action_default(id, true);
    }
    super::server::dispatch_runtime_mutation(repository, context.mutate_session, id, session, &request)
        .map(|_| ())
        .map_err(|error| format!("{}: {}", error.code, error.message))
}

/// `POST /v2/runtimes/stop-idle`.
pub fn handle_stop(stream: &mut TcpStream, request: &SimpleHttpRequest, context: ManagementRequestContext) -> anyhow::Result<()> {
    let (owner_stop, body) = match parse_stop(&request.body) {
        Ok(parsed) => parsed,
        Err(message) => return super::server::write_v2_error(stream, 400, "Bad Request", "invalid_request", &message, false, json!({})),
    };
    let canonical = json!({
        "sessionIds": body.session_ids, "minIdleSecs": body.min_idle_secs,
        "mode": if owner_stop { "stopped" } else { "wakeable" }, "dryRun": body.dry_run,
    });
    let claim = if body.dry_run {
        None
    } else {
        use super::request_idempotency::BeginRequest;
        let repository = super::request_idempotency::request_idempotency_repository()?;
        match repository.begin(IDEMPOTENCY_SCOPE, &body.request_id, &canonical)? {
            BeginRequest::Forward(claim) => Some((repository, claim)),
            BeginRequest::Completed(response) => return write_json_response(stream, response.status, &response.reason, &response.body),
            BeginRequest::InProgress => {
                return super::server::write_v2_error(stream, 202, "Accepted", "request_in_progress", "this stop is still running", true, json!({}))
            }
            BeginRequest::Conflict => {
                return super::server::write_v2_error(stream, 409, "Conflict", "idempotency_conflict", "requestId was used for another stop", false, json!({}))
            }
            other => {
                return super::server::write_v2_error(stream, 409, "Conflict", "request_outcome_unknown", &format!("{other:?}"), false, json!({}))
            }
        }
    };
    let now = Utc::now();
    let host = HostFacts::load();
    let online: BTreeMap<String, (Value, Option<DateTime<Utc>>)> = online_sessions(context)?
        .into_iter()
        .filter_map(|(session, started)| Some((session.get("cutexSessionId")?.as_str()?.to_string(), (session, started))))
        .collect();
    let mut results = Vec::new();
    for id in &body.session_ids {
        let Some((session, started)) = online.get(id) else {
            results.push(json!({"cutexSessionId": id, "result": "skipped", "reasons": ["offline"]}));
            continue;
        };
        // Checked again now: it may have started work since the listing.
        let facts = host.facts(context, session, *started);
        let (verdict, reasons, _) = classify(&facts, now, body.min_idle_secs);
        if verdict == Verdict::Blocked {
            results.push(json!({"cutexSessionId": id, "result": "skipped", "reasons": reasons}));
            continue;
        }
        if body.dry_run {
            results.push(json!({"cutexSessionId": id, "result": "wouldStop", "reasons": reasons}));
            continue;
        }
        match stop_one(context, &body.request_id, session, owner_stop) {
            Ok(()) => results.push(json!({"cutexSessionId": id, "result": "stopped", "reasons": reasons})),
            Err(error) => results.push(json!({"cutexSessionId": id, "result": "failed", "error": error})),
        }
    }
    let response = json!({
        "mode": if owner_stop { "stopped" } else { "wakeable" },
        "dryRun": body.dry_run,
        "results": results,
    });
    if let Some((repository, claim)) = claim {
        let _ = repository.complete(&claim, 200, "OK", response.clone());
    }
    write_json_response(stream, 200, "OK", &response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn now() -> DateTime<Utc> {
        crate::asks::parse_time("2026-10-22T12:00:00Z").unwrap()
    }

    /// An online agent idle for two hours, nothing else going on.
    fn idle() -> IdleFacts {
        IdleFacts { online: true, idle_since: Some(now() - Duration::hours(2)), ..IdleFacts::default() }
    }

    fn verdict(facts: &IdleFacts, min: u64) -> (Verdict, Vec<&'static str>) {
        let (verdict, reasons, _) = classify(facts, now(), min);
        (verdict, reasons)
    }

    #[test]
    fn an_idle_agent_is_stoppable_after_the_threshold() {
        assert_eq!(verdict(&idle(), 30 * 60), (Verdict::Stoppable, vec![]));
        assert_eq!(verdict(&idle(), 3 * 3600), (Verdict::Blocked, vec!["tooRecent"]));
        let (_, _, secs) = classify(&idle(), now(), 0);
        assert_eq!(secs, Some(7200));
        // Unknown idle time: only "every idle agent".
        let unknown = IdleFacts { idle_since: None, ..idle() };
        assert_eq!(verdict(&unknown, 0).0, Verdict::Stoppable);
        assert_eq!(verdict(&unknown, 60).1, vec!["tooRecent"]);
    }

    /// Design 031: anything that would end work blocks the stop.
    #[test]
    fn work_in_progress_blocks() {
        let cases: Vec<(IdleFacts, &str)> = vec![
            (IdleFacts { online: false, ..idle() }, "offline"),
            (IdleFacts { active_turn: true, ..idle() }, "activeTurn"),
            (IdleFacts { thread_busy: true, ..idle() }, "threadBusy"),
            (IdleFacts { owner_queue: 1, ..idle() }, "ownerQueue"),
            (IdleFacts { handling_delegation: true, ..idle() }, "handlingDelegation"),
            (IdleFacts { delegations_waiting: 2, ..idle() }, "delegationsWaiting"),
            (IdleFacts { pending_requests: 1, ..idle() }, "pendingRequest"),
            (IdleFacts { open_questions: 1, ..idle() }, "openQuestion"),
            (IdleFacts { background: Background::Running(1), ..idle() }, "backgroundWork"),
        ];
        for (facts, reason) in cases {
            assert_eq!(verdict(&facts, 60), (Verdict::Blocked, vec![reason]), "{reason}");
        }
    }

    #[test]
    fn cautions_are_stoppable_but_not_by_default_and_notes_do_not_change_the_verdict() {
        let cases: Vec<(IdleFacts, &str)> = vec![
            (IdleFacts { awaiting_delegations: 1, ..idle() }, "awaitingDelegations"),
            (IdleFacts { scheduled_alarms: 1, ..idle() }, "scheduledAlarm"),
            (IdleFacts { running_jobs: 1, ..idle() }, "runningJobs"),
            (IdleFacts { director: true, ..idle() }, "director"),
            (IdleFacts { background: Background::Unknown, ..idle() }, "backgroundUnknown"),
        ];
        for (facts, reason) in cases {
            assert_eq!(verdict(&facts, 60), (Verdict::Caution, vec![reason]), "{reason}");
        }
        let noted = IdleFacts { owner_start_only: true, owned_services: 2, ..idle() };
        assert_eq!(verdict(&noted, 60), (Verdict::Stoppable, vec!["ownerStartOnly", "ownsServices"]));
        let both = IdleFacts { active_turn: true, director: true, ..idle() };
        assert_eq!(verdict(&both, 60), (Verdict::Blocked, vec!["activeTurn", "director"]), "blocking reasons first");
    }

    #[test]
    fn session_resource_facts() {
        let mut facts = IdleFacts { idle_since: Some(now() - Duration::hours(5)), ..IdleFacts::default() };
        session_facts(&json!({
            "runtime": {"status": "online", "activeTurnId": null, "threadStatus": {"type": "idle"}, "pendingServerRequests": [{"id": 1}]},
            "activity": {"lastConversationAt": "2026-10-22T11:00:00Z", "lastTurnCompletedAt": "2026-10-22T10:00:00Z"}
        }), &mut facts);
        assert!(facts.online && !facts.active_turn && !facts.thread_busy);
        assert_eq!(facts.pending_requests, 1);
        assert_eq!(facts.idle_since, crate::asks::parse_time("2026-10-22T11:00:00Z"), "the latest activity counts");
        let mut busy = IdleFacts::default();
        session_facts(&json!({"runtime": {"status": "online", "activeTurnId": "t1", "threadStatus": {"type": "active"}}}), &mut busy);
        assert!(busy.active_turn && busy.thread_busy);
        let mut offline = IdleFacts::default();
        session_facts(&json!({"runtime": {"status": "offline"}}), &mut offline);
        assert!(!offline.online);
    }

    #[test]
    fn background_replies() {
        assert_eq!(background_from_reply(&json!({"result": {"tasks": [{"status": "completed"}, {"status": "running"}]}})), Background::Running(1));
        assert_eq!(background_from_reply(&json!({"result": {"tasks": [{"status": "completed"}]}})), Background::None);
        assert_eq!(background_from_reply(&json!({"result": {"data": [{"processId": "p"}]}})), Background::Running(1));
        assert_eq!(background_from_reply(&json!({"result": {"data": []}})), Background::None);
        assert_eq!(background_from_reply(&json!({"error": {"code": -32601}})), Background::Unknown);
    }

    #[test]
    fn alarms_and_asks_facts() {
        let alarm = |owner: &str, to: &str, state: &str| crate::agent_bus::alarms::Alarm {
            alarm_id: "a".into(), owner: owner.into(), to: to.into(), message: "m".into(), at: "2026-10-22T13:00:00Z".into(),
            delay_seconds: None, message_id: "x".into(), state: state.into(), attempts: 0, next_attempt: 0, receipt: None,
            last_error: None, last_attempt: None,
        };
        let alarms = vec![alarm("cutex.a", "cutex.a", "scheduled"), alarm("cutex.b", "cutex.a", "scheduled"), alarm("cutex.a", "cutex.a", "queued")];
        assert_eq!(alarm_count(&alarms, "cutex.a"), 2);
        assert_eq!(alarm_count(&alarms, "cutex.c"), 0);

        use crate::asks::store::tests::{new_ask, REQUESTER, TARGET};
        let mut state = crate::asks::store::AskState::default();
        state.create_ask(new_ask("a", "x"), now()).unwrap();
        let mut target = IdleFacts::default();
        ask_facts(&state, TARGET, &mut target);
        assert_eq!(target.delegations_waiting, 1, "a queued delegation would wake it at once");
        let mut requester = IdleFacts::default();
        ask_facts(&state, REQUESTER, &mut requester);
        assert_eq!(requester.awaiting_delegations, 1);
        assert!(!requester.handling_delegation);
    }

    #[test]
    fn stop_bodies() {
        let (stopped, body) = parse_stop(br#"{"requestId":"r1","sessionIds":["cutex.a"],"minIdleSecs":1800}"#).unwrap();
        assert!(!stopped, "wakeable by default");
        assert_eq!(body.min_idle_secs, 1800);
        assert!(parse_stop(br#"{"requestId":"r1","sessionIds":["cutex.a"],"mode":"stopped"}"#).unwrap().0);
        assert!(parse_stop(br#"{"requestId":"r1","sessionIds":[]}"#).is_err());
        assert!(parse_stop(br#"{"requestId":"","sessionIds":["a"]}"#).is_err());
        assert!(parse_stop(br#"{"requestId":"r","sessionIds":["a"],"mode":"later"}"#).is_err());
        assert!(parse_stop(br#"{"requestId":"r","sessionIds":["a"],"extra":1}"#).is_err());
        let many = serde_json::to_vec(&json!({"requestId": "r", "sessionIds": vec!["a"; MAX_BATCH + 1]})).unwrap();
        assert!(parse_stop(&many).is_err());
        assert_eq!(parse_min_idle(Some("x")), Err("minIdleSecs must be a non-negative integer".to_string()));
    }
}
