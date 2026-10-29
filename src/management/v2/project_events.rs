//! Project log (capability `project-events`, cutex-desktop
//! docs/design/022-project-event-stream.md, option B):
//!
//! - `GET /v2/projects/{projectId}/events?before=&after=&limit=&kinds=&sessionId=`
//!   → the project's events, newest first (`after`: newer ones, oldest first);
//!   cursors `pe:<seq>`, 409 `cursor_expired` for a cursor this journal never
//!   assigned (another stream, or a future one). A page reads a bounded
//!   number of rows ([`SCAN_BUDGET`]); `nextCursor` (r252) says where the
//!   next page starts, so a short or empty page can still have `hasMore`.
//! - `GET /v2/projects/{projectId}/events/stream?after=` (SSE, `id:` = cursor,
//!   `Last-Event-ID`, 25 s heartbeat, `lagged` frame for slow readers).
//! - `cutex/projectEvents/appended {projectId, cursor}` on the host event
//!   stream when a project got new rows (at most every 5 s per project).
//!
//! The daemon's project-log thread ([`start`]) follows the Management v2
//! event log and turns meaningful events (turns, delegations, sub-agents,
//! approvals, artifacts, workflow runs, owner messages, jobs, runtime
//! online / offline, membership changes) into rows tagged with the project
//! the agent belonged to when the row was written. Rows live in
//! `<cutex home>/runtime/project-events/journal.sqlite3` and are never
//! deleted (owner decision 2026-10-23); pages read through the
//! `(project_id[, session_id][, kind prefix], seq)` indexes. Only new files and new `cutex/...` events:
//! older releases ignore both.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::net::TcpStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::model::{EventEnvelope, NativeMessageKind};
use super::repository::{ReplayError, ReplayQuery};
use crate::http::server::{write_json_response, SimpleHttpRequest};

pub const APPENDED_EVENT: &str = "cutex/projectEvents/appended";
/// SSE `event:` name of a row.
pub const STREAM_EVENT: &str = "cutex/projectEvents/event";
/// Claude task status change (from the runtime's `claude/task/updated`,
/// recorded only when a task's status changes): `{threadId, task}`.
pub const CLAUDE_TASK_EVENT: &str = "cutex/claudeTask/changed";
const CURSOR_PREFIX: &str = "pe:";

const TICK: Duration = Duration::from_secs(1);
const PAGE_LIMIT: usize = 500;
const PAGES_PER_TICK: usize = 20;
const PING_INTERVAL: Duration = Duration::from_secs(5);
const FACTS_REFRESH: Duration = Duration::from_secs(5);
const HEARTBEAT: Duration = Duration::from_secs(25);
const SUBSCRIBER_CAPACITY: usize = 1024;
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 500;
const BACKLOG_LIMIT: usize = 10_000;
/// Rows a page reads (after the SQL `kinds=` / `sessionId=` filters) before
/// it stops: a page for a reader who sees few of the rows answers with what
/// it found, `hasMore` and a `nextCursor` to continue from instead of walking
/// the whole (never pruned) journal. About 20 ms at 1M rows.
const SCAN_BUDGET: usize = 10_000;
/// Largest single read of a page scan.
const SCAN_BATCH: usize = 2_000;
/// SQL for a row's kind prefix (`kinds=` value; [`ProjectEvent::kind_prefix`]):
/// the text before the first `.`. Indexed (`events_project_family`,
/// `events_project_session_family`); queries must use this exact text.
const FAMILY_SQL: &str = "substr(kind, 1, instr(kind || '.', '.') - 1)";
const PREVIEW_CHARS: usize = 120;
/// Bounds of the projector's in-flight state.
const MAX_TURN_STARTS: usize = 2_000;
const MAX_ASK_PROJECTS: usize = 5_000;
const MAX_RUNNING_TASKS: usize = 2_000;
/// `/background` answers are reused this long (each one asks every online
/// runtime of the project).
const BACKGROUND_CACHE: Duration = Duration::from_secs(5);

/// Kind prefixes accepted by `kinds=`.
pub const KIND_PREFIXES: &[&str] = &[
    "turn", "ask", "subagent", "background", "approval", "artifact", "workflow", "ownerNotify", "job", "runtime", "member",
    "journal",
];

// ---------------------------------------------------------------- rows

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Parent {
    pub turn_id: Option<String>,
    pub task_id: Option<String>,
    pub thread_id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Refs {
    pub ask_id: Option<String>,
    pub job_id: Option<String>,
    pub artifact_id: Option<String>,
    pub workflow_run_id: Option<String>,
    pub approval_id: Option<String>,
    pub notification_id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    /// The event-log method the row came from (`null`: the projector's own,
    /// e.g. membership changes and gaps).
    pub method: Option<String>,
    /// Management v2 event-log cursor (`c2:…`) of that event.
    pub event_cursor: Option<String>,
}

/// One project event row (design 022 «一行事件的样子»).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectEvent {
    /// `pe:<seq>`; set when the row is journaled.
    #[serde(default)]
    pub cursor: String,
    /// RFC 3339 UTC (the source event's receipt time).
    pub at: String,
    pub project_id: String,
    pub cutex_session_id: Option<String>,
    pub agent_name: Option<String>,
    pub kind: String,
    pub status: Option<String>,
    pub title: Option<String>,
    pub detail: Option<String>,
    pub parent: Parent,
    pub refs: Refs,
    pub source: Source,
    /// Turn rows: run time when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Turn rows: tool items in the turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<u64>,
    /// Written by the first-start replay of the retained event log; the
    /// project is the membership at replay time.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub backfilled: bool,
}

impl ProjectEvent {
    fn kind_prefix(&self) -> &str {
        self.kind.split('.').next().unwrap_or_default()
    }
}

/// A row before it is journaled; `dedupe` is unique in the journal (a
/// source event seen twice, e.g. an ask notice on both of its sessions,
/// becomes one row).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Normalized {
    pub dedupe: String,
    pub row: ProjectEvent,
}

pub fn format_cursor(seq: i64) -> String {
    format!("{CURSOR_PREFIX}{seq}")
}

pub fn parse_cursor(text: &str) -> Option<i64> {
    text.trim().strip_prefix(CURSOR_PREFIX)?.parse::<i64>().ok().filter(|seq| *seq >= 0)
}

/// Collapse whitespace and cut to [`PREVIEW_CHARS`].
fn preview(text: &str) -> Option<String> {
    super::summary::preview_text(text, PREVIEW_CHARS)
}

fn text(value: &Value) -> Option<String> {
    value.as_str().filter(|v| !v.is_empty()).map(str::to_string)
}

// ---------------------------------------------------------------- facts

/// What the projector needs to know besides the event itself; abstracted
/// for tests.
pub trait Facts {
    /// The project the session belongs to now (member or Director seat).
    fn project_of_session(&self, session: &str) -> Option<String>;
    /// The session's own native thread (sub-agent threads differ).
    fn own_thread(&self, session: &str) -> Option<String>;
    fn agent_name(&self, session: &str) -> Option<String>;
    /// The project recorded on a delegation (fallback for asks created
    /// before the projector saw them).
    fn ask_project(&self, ask_id: &str) -> Option<String>;
    /// The delegation's title (same fallback).
    fn ask_title(&self, ask_id: &str) -> Option<String>;
}

/// The projector's in-flight state (persisted with the checkpoint).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pending {
    /// `<session>|<turnId>` → `turn/started` time (Codex turns carry no
    /// duration of their own).
    #[serde(default)]
    pub turn_starts: BTreeMap<String, String>,
    /// askId → project, from `cutex/ask/created`, until the ask closes.
    #[serde(default)]
    pub ask_projects: BTreeMap<String, String>,
    /// askId → title, from `cutex/ask/created`, until the ask closes: the
    /// later ask rows carry it too.
    #[serde(default)]
    pub ask_titles: BTreeMap<String, String>,
    /// `<session>|<taskId>` → the last change of a Claude task still
    /// running (shown as state unknown while its runtime is offline).
    #[serde(default)]
    pub running_tasks: BTreeMap<String, Value>,
}

impl Pending {
    fn bound(&mut self) {
        fn trim(map: &mut BTreeMap<String, String>, max: usize) {
            while map.len() > max {
                let Some(key) = map.keys().next().cloned() else { break };
                map.remove(&key);
            }
        }
        trim(&mut self.turn_starts, MAX_TURN_STARTS);
        trim(&mut self.ask_projects, MAX_ASK_PROJECTS);
        trim(&mut self.ask_titles, MAX_ASK_PROJECTS);
        while self.running_tasks.len() > MAX_RUNNING_TASKS {
            let Some(key) = self.running_tasks.keys().next().cloned() else { break };
            self.running_tasks.remove(&key);
        }
    }
}

// ---------------------------------------------------------------- normalize

struct Base<'a> {
    event: &'a EventEnvelope,
    method: &'a str,
}

impl Base<'_> {
    fn row(&self, kind: &str, project: String, session: Option<String>, facts: &dyn Facts) -> ProjectEvent {
        ProjectEvent {
            at: crate::management::instance::normalize_utc(&self.event.received_at),
            project_id: project,
            agent_name: session.as_deref().and_then(|s| facts.agent_name(s)),
            cutex_session_id: session,
            kind: kind.to_string(),
            source: Source { method: Some(self.method.to_string()), event_cursor: Some(self.event.cursor.clone()) },
            ..ProjectEvent::default()
        }
    }
}

fn is_host(session: &str) -> bool {
    session.is_empty() || session == super::services::HOST_EVENT_SESSION_ID
}

/// Turn one event-log entry into project rows (none for most events).
pub fn normalize(event: &EventEnvelope, facts: &dyn Facts, pending: &mut Pending) -> Vec<Normalized> {
    let rows = if let Some(native) = &event.native {
        if native.kind != NativeMessageKind::Notification || is_host(&event.cutex_session_id) {
            return Vec::new();
        }
        let method = native.message["method"].as_str().unwrap_or_default();
        normalize_native(&Base { event, method }, &native.message["params"], facts, pending)
    } else if let Some(cutex) = &event.cutex {
        normalize_cutex(&Base { event, method: &cutex.method }, &cutex.params, facts, pending)
    } else {
        Vec::new()
    };
    pending.bound();
    rows
}

fn normalize_native(base: &Base<'_>, params: &Value, facts: &dyn Facts, pending: &mut Pending) -> Vec<Normalized> {
    let event = base.event;
    let session = event.cutex_session_id.as_str();
    let thread = text(&params["threadId"]).or_else(|| event.correlation.thread_id.clone());
    let own_thread = || match (facts.own_thread(session), &thread) {
        (Some(own), Some(thread)) => own == *thread,
        _ => true,
    };
    match base.method {
        "turn/started" => {
            if let Some(turn) = text(&params["turn"]["id"]).or_else(|| event.correlation.turn_id.clone()) {
                if own_thread() {
                    pending.turn_starts.insert(format!("{session}|{turn}"), event.received_at.clone());
                }
            }
            Vec::new()
        }
        "turn/completed" => {
            let turn = &params["turn"];
            let Some(status) = turn["status"].as_str().filter(|s| matches!(*s, "completed" | "failed" | "interrupted"))
            else {
                return Vec::new();
            };
            let turn_id = text(&turn["id"]).or_else(|| event.correlation.turn_id.clone());
            let started = turn_id.as_ref().and_then(|t| pending.turn_starts.remove(&format!("{session}|{t}")));
            // Sub-agent threads report under the parent's session: their
            // turns are the sub-agent's rows, not the agent's.
            if !own_thread() {
                return Vec::new();
            }
            let Some(project) = facts.project_of_session(session) else { return Vec::new() };
            let mut row = base.row(&format!("turn.{status}"), project, Some(session.to_string()), facts);
            row.status = Some(status.to_string());
            let items = turn["items"].as_array().map(Vec::as_slice).unwrap_or_default();
            row.title = items.iter().find(|item| item["type"] == "userMessage").and_then(user_message_text).and_then(|t| preview(&t));
            row.detail = match turn["error"]["message"].as_str() {
                Some(error) if status != "completed" => preview(error),
                _ => items
                    .iter()
                    .rev()
                    .find(|item| item["type"] == "agentMessage")
                    .and_then(|item| item["text"].as_str())
                    .and_then(preview),
            };
            row.tool_calls = Some(items.iter().filter(|item| is_tool_item(item)).count() as u64);
            row.duration_ms = turn["durationMs"].as_u64().or_else(|| {
                let start = chrono::DateTime::parse_from_rfc3339(started.as_deref()?).ok()?;
                let end = chrono::DateTime::parse_from_rfc3339(&event.received_at).ok()?;
                u64::try_from((end - start).num_milliseconds()).ok()
            });
            row.parent = Parent { turn_id: turn_id.clone(), task_id: None, thread_id: thread };
            let dedupe = format!("turn:{session}:{}", turn_id.unwrap_or_else(|| event.event_id.clone()));
            vec![Normalized { dedupe, row }]
        }
        "item/completed" => {
            let item = &params["item"];
            let turn_id = text(&params["turnId"]).or_else(|| event.correlation.turn_id.clone());
            // (child thread, row kind, status)
            let children: Vec<(String, &str, Option<&str>)> = match item["type"].as_str() {
                Some("subAgentActivity") => {
                    let Some(child) = text(&item["agentThreadId"]) else { return Vec::new() };
                    match item["kind"].as_str() {
                        Some("started") => vec![(child, "subagent.started", None)],
                        Some(kind @ ("completed" | "interrupted")) => vec![(child, "subagent.finished", Some(kind))],
                        _ => return Vec::new(),
                    }
                }
                Some("collabAgentToolCall") if item["tool"] == "spawnAgent" => item["receiverThreadIds"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|child| text(child).map(|child| (child, "subagent.started", None)))
                    .collect(),
                _ => return Vec::new(),
            };
            let Some(project) = facts.project_of_session(session) else { return Vec::new() };
            children
                .into_iter()
                .map(|(child, kind, status)| {
                    let mut row = base.row(kind, project.clone(), Some(session.to_string()), facts);
                    row.status = status.map(str::to_string);
                    row.title = text(&item["agentPath"]).or_else(|| Some(child.clone()));
                    row.parent = Parent {
                        turn_id: turn_id.clone(),
                        task_id: child.rsplit_once(":task:").map(|(_, task)| task.to_string()),
                        thread_id: Some(child.clone()),
                    };
                    let phase = if kind == "subagent.started" { "started" } else { "finished" };
                    Normalized { dedupe: format!("subagent:{session}:{child}:{phase}"), row }
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

/// Text of a native `userMessage` item (Codex `content[]`, or `text`).
fn user_message_text(item: &Value) -> Option<String> {
    if let Some(text) = item["text"].as_str() {
        return Some(text.to_string());
    }
    let parts: Vec<&str> = item["content"]
        .as_array()?
        .iter()
        .filter_map(|part| part["text"].as_str())
        .collect();
    (!parts.is_empty()).then(|| parts.join(" "))
}

fn is_tool_item(item: &Value) -> bool {
    !matches!(
        item["type"].as_str(),
        None | Some(
            "userMessage" | "agentMessage" | "reasoning" | "plan" | "contextCompaction" | "hookPrompt" | "enteredReviewMode"
                | "exitedReviewMode"
        )
    )
}

fn normalize_cutex(base: &Base<'_>, params: &Value, facts: &dyn Facts, pending: &mut Pending) -> Vec<Normalized> {
    let event = base.event;
    let session = (!is_host(&event.cutex_session_id)).then(|| event.cutex_session_id.clone());
    let member_project = |session: Option<&str>| session.and_then(|s| facts.project_of_session(s));
    let one = |dedupe: String, row: ProjectEvent| vec![Normalized { dedupe, row }];
    match base.method {
        "cutex/ask/created" => {
            let ask = &params["ask"];
            let Some(ask_id) = text(&ask["askId"]) else { return Vec::new() };
            let requester = text(&ask["requester"]["cutexSessionId"]);
            let target = text(&ask["targetCutexSessionId"]);
            let Some(project) = text(&ask["projectId"])
                .or_else(|| member_project(requester.as_deref()))
                .or_else(|| member_project(target.as_deref()))
            else {
                return Vec::new();
            };
            pending.ask_projects.insert(ask_id.clone(), project.clone());
            let mut row = base.row("ask.created", project, requester.or(target), facts);
            row.status = text(&ask["execution"]);
            row.title = ask["title"].as_str().and_then(preview);
            if let Some(title) = &row.title {
                pending.ask_titles.insert(ask_id.clone(), title.clone());
            }
            row.detail = text(&ask["targetName"]).map(|name| format!("→ {name}"));
            row.refs.ask_id = Some(ask_id.clone());
            one(format!("ask:{ask_id}:created"), row)
        }
        "cutex/ask/answered" | "cutex/ask/stateChanged" | "cutex/ask/closed" => {
            let Some(ask_id) = text(&params["askId"]) else { return Vec::new() };
            let (kind, status, dedupe) = match base.method {
                "cutex/ask/answered" => (
                    "ask.answered",
                    text(&params["status"]),
                    format!("ask:{ask_id}:answered:{}", params["answerId"].as_str().unwrap_or_default()),
                ),
                "cutex/ask/closed" => ("ask.closed", text(&params["outcome"]), format!("ask:{ask_id}:closed")),
                _ => match params["execution"].as_str() {
                    Some(state @ ("failed" | "interrupted" | "rejected")) => (
                        "ask.failed",
                        Some(state.to_string()),
                        format!("ask:{ask_id}:failed:{}", params["revision"]),
                    ),
                    _ => return Vec::new(),
                },
            };
            let project = pending
                .ask_projects
                .get(&ask_id)
                .cloned()
                .or_else(|| facts.ask_project(&ask_id))
                .or_else(|| member_project(session.as_deref()));
            let title = pending
                .ask_titles
                .get(&ask_id)
                .cloned()
                .or_else(|| facts.ask_title(&ask_id).as_deref().and_then(preview));
            if kind == "ask.closed" {
                pending.ask_projects.remove(&ask_id);
                pending.ask_titles.remove(&ask_id);
            }
            let Some(project) = project else { return Vec::new() };
            let mut row = base.row(kind, project, session, facts);
            row.status = status;
            row.title = title;
            row.detail = params["preview"].as_str().and_then(preview);
            row.refs.ask_id = Some(ask_id);
            one(dedupe, row)
        }
        "cutex/approvals/changed" => {
            let approval = &params["approval"];
            let reason = params["reason"].as_str().unwrap_or_default();
            let kind = match reason {
                "requested" => "approval.requested",
                "approved" | "denied" => "approval.decided",
                _ => return Vec::new(),
            };
            let Some(id) = text(&approval["approvalId"]) else { return Vec::new() };
            let requester = text(&approval["requesterCutexSessionId"]);
            let Some(project) = text(&approval["projectId"]).or_else(|| member_project(requester.as_deref())) else {
                return Vec::new();
            };
            let mut row = base.row(kind, project, requester, facts);
            row.agent_name = row.agent_name.or_else(|| text(&approval["requesterName"]));
            row.status = Some(reason.to_string());
            row.title = approval["summary"].as_str().and_then(preview);
            row.detail = text(&approval["kind"]);
            row.refs.approval_id = Some(id.clone());
            one(format!("approval:{id}:{reason}"), row)
        }
        "cutex/artifact/published" => {
            if params["newVersion"] == Value::Bool(false) {
                return Vec::new();
            }
            let Some(id) = text(&params["artifactId"]) else { return Vec::new() };
            let producer = &params["producer"];
            let producer_session = text(&producer["sessionId"]);
            let scoped = (params["scope"]["kind"] == "project").then(|| text(&params["scope"]["id"])).flatten();
            let Some(project) = scoped.or_else(|| member_project(producer_session.as_deref())) else {
                return Vec::new();
            };
            let mut row = base.row("artifact.published", project, producer_session, facts);
            row.agent_name = row.agent_name.or_else(|| text(&producer["displayName"]));
            row.title = params["name"].as_str().and_then(preview);
            row.detail = text(&params["kind"]);
            row.parent.turn_id = text(&producer["turnId"]);
            row.refs.artifact_id = Some(id.clone());
            row.refs.ask_id = text(&producer["askId"]);
            one(format!("artifact:{id}:{}", params["version"]), row)
        }
        "cutex/workflow/run/changed" => {
            let Some(state) = params["state"]
                .as_str()
                .filter(|s| matches!(*s, "completed" | "errored" | "stopped" | "rejected" | "expired"))
            else {
                return Vec::new();
            };
            let Some(run) = text(&params["runId"]) else { return Vec::new() };
            let author = text(&params["authorCutexSessionId"]);
            let Some(project) = text(&params["projectId"]).or_else(|| member_project(author.as_deref())) else {
                return Vec::new();
            };
            let mut row = base.row("workflow.run", project, author, facts);
            row.status = Some(state.to_string());
            row.title = params["name"].as_str().and_then(preview);
            row.detail = params["stopReason"].as_str().and_then(preview);
            row.refs.workflow_run_id = Some(run.clone());
            row.refs.approval_id = text(&params["approvalId"]);
            one(format!("workflow:{run}:{state}"), row)
        }
        crate::owner_notify::CREATED_EVENT => {
            let record = &params["notification"];
            let Some(id) = text(&record["notificationId"]) else { return Vec::new() };
            let agent = text(&record["agentId"]).or(session);
            let Some(project) = member_project(agent.as_deref()) else { return Vec::new() };
            let mut row = base.row("ownerNotify.created", project, agent, facts);
            row.agent_name = row.agent_name.or_else(|| text(&record["agentName"]));
            row.status = text(&record["level"]);
            row.title = record["text"].as_str().and_then(preview);
            row.parent.turn_id = text(&record["turnId"]);
            row.refs.notification_id = Some(id.clone());
            one(format!("ownerNotify:{id}"), row)
        }
        "cutex/jobService/terminalObserved" => {
            let target = text(&params["targetCutexSessionId"]).or(session);
            let Some(project) = member_project(target.as_deref()) else { return Vec::new() };
            let job = text(&params["jobId"]);
            let mut row = base.row("job.finished", project, target, facts);
            row.status = text(&params["terminalStatus"]);
            row.title = job.clone();
            row.detail = params["facts"]["exitCode"].as_i64().map(|code| format!("exit {code}"));
            row.duration_ms = params["facts"]["execution"]["observedRunDurationMillis"].as_u64();
            row.refs.job_id = job;
            let id = text(&params["eventId"]).unwrap_or_else(|| event.event_id.clone());
            one(format!("job:{id}"), row)
        }
        CLAUDE_TASK_EVENT => {
            let task = &params["task"];
            let (Some(session), Some(task_id)) = (session, text(&task["taskId"])) else { return Vec::new() };
            let key = format!("{session}|{task_id}");
            let status = task["status"].as_str().unwrap_or_default();
            let terminal = is_terminal_task_status(status);
            let first = !pending.running_tasks.contains_key(&key);
            if terminal {
                pending.running_tasks.remove(&key);
            } else {
                let mut summary = task.clone();
                summary["threadId"] = params["threadId"].clone();
                summary["at"] = json!(crate::management::instance::normalize_utc(&event.received_at));
                if let Some(seen) = pending.running_tasks.get(&key).and_then(|seen| seen.get("startedAt")).cloned() {
                    summary["startedAt"] = seen;
                } else {
                    summary["startedAt"] = summary["at"].clone();
                }
                pending.running_tasks.insert(key, summary);
            }
            // History: background commands only (sub-agents already have
            // their subagent.* rows from the turn's items).
            if task["taskType"] != "local_bash" || (!terminal && !first) {
                return Vec::new();
            }
            let Some(project) = member_project(Some(&session)) else { return Vec::new() };
            let (kind, phase) = if terminal { ("background.finished", "finished") } else { ("background.started", "started") };
            let mut row = base.row(kind, project, Some(session.clone()), facts);
            row.status = Some(status.to_string());
            let command = task["command"].as_str().and_then(preview);
            row.title = task["description"].as_str().and_then(preview).or_else(|| command.clone());
            row.detail = if row.title == command { None } else { command };
            row.duration_ms = task["usage"]["duration_ms"].as_u64();
            row.parent = Parent { turn_id: text(&task["turnId"]), task_id: Some(task_id.clone()), thread_id: text(&params["threadId"]) };
            one(format!("background:{session}:{task_id}:{phase}"), row)
        }
        "cutex/runtime/online" | "cutex/runtime/offline" | "cutex/runtime/failed" => {
            if base.method == "cutex/runtime/online" {
                // A new runtime generation: the old one's tasks are gone.
                if let Some(session) = &session {
                    let prefix = format!("{session}|");
                    pending.running_tasks.retain(|key, _| !key.starts_with(&prefix));
                }
            }
            let Some(project) = member_project(session.as_deref()) else { return Vec::new() };
            let kind = if base.method == "cutex/runtime/online" { "runtime.online" } else { "runtime.offline" };
            let mut row = base.row(kind, project, session, facts);
            row.status = text(&params["status"]).or_else(|| base.method.rsplit('/').next().map(str::to_string));
            row.detail = params["reason"].as_str().and_then(preview);
            one(format!("runtime:{}", event.event_id), row)
        }
        _ => Vec::new(),
    }
}

/// Terminal Claude task statuses (adapter `runtime_tasks.TERMINAL`).
fn is_terminal_task_status(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "stopped" | "killed" | "cancelled" | "lost")
}

/// Last recorded status per `<session>|<taskId>`.
static CLAUDE_TASK_STATUS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

/// Remember `status` for the task; false when it is the one recorded last.
fn claude_task_status_changed(session: &str, task_id: &str, status: &str) -> bool {
    let key = format!("{session}|{task_id}");
    let mut seen = CLAUDE_TASK_STATUS.get_or_init(Default::default).lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if seen.get(&key).map(String::as_str) == Some(status) {
        return false;
    }
    if seen.len() >= MAX_RUNNING_TASKS {
        // Bound only; a forgotten task re-announces its unchanged status
        // once, which adds no row.
        seen.clear();
    }
    if is_terminal_task_status(status) {
        seen.remove(&key);
    } else {
        seen.insert(key, status.to_string());
    }
    true
}

/// A Claude runtime's `claude/task/updated {threadId, task}` (sent on every
/// progress tick): append [`CLAUDE_TASK_EVENT`] when the task's status
/// changed, with the task's identity, kind, status and (cut) description /
/// command only.
pub fn record_claude_task(cutex_session_id: &str, params: &Value) -> anyhow::Result<()> {
    let task = &params["task"];
    let (Some(task_id), Some(status)) = (task["taskId"].as_str(), task["status"].as_str()) else {
        return Ok(());
    };
    if !claude_task_status_changed(cutex_session_id, task_id, status) {
        return Ok(());
    }
    let cut = |value: &Value| value.as_str().map(|text| json!(text.chars().take(500).collect::<String>()));
    let mut summary = json!({});
    for field in ["taskId", "turnId", "taskType", "status", "background", "toolUseId"] {
        if let Some(value) = task.get(field).filter(|v| !v.is_null()) {
            summary[field] = value.clone();
        }
    }
    for field in ["description", "command"] {
        if let Some(value) = cut(&task[field]) {
            summary[field] = value;
        }
    }
    if let Some(ms) = task["usage"]["duration_ms"].as_u64() {
        summary["usage"] = json!({ "duration_ms": ms });
    }
    let correlation = super::model::EventCorrelation {
        thread_id: text(&params["threadId"]),
        turn_id: text(&task["turnId"]),
        ..Default::default()
    };
    super::server::append_cutex_event(
        super::repository::management_v2_repository()?,
        cutex_session_id,
        correlation,
        CLAUDE_TASK_EVENT,
        json!({ "threadId": params["threadId"], "task": summary }),
    )
}

/// `member.joined` / `member.left` rows for the change from `previous` to
/// `next` (session → project), at `at`.
pub fn membership_rows(
    previous: &BTreeMap<String, String>,
    next: &BTreeMap<String, String>,
    at: &str,
    facts: &dyn Facts,
) -> Vec<Normalized> {
    let mut rows = Vec::new();
    let mut row = |kind: &str, session: &str, project: &str| {
        let mut event = ProjectEvent {
            at: at.to_string(),
            project_id: project.to_string(),
            cutex_session_id: Some(session.to_string()),
            agent_name: facts.agent_name(session),
            kind: kind.to_string(),
            ..ProjectEvent::default()
        };
        event.source.method = None;
        rows.push(Normalized { dedupe: format!("member:{session}:{project}:{kind}:{at}"), row: event });
    };
    for (session, project) in previous {
        if next.get(session) != Some(project) {
            row("member.left", session, project);
        }
    }
    for (session, project) in next {
        if previous.get(session) != Some(project) {
            row("member.joined", session, project);
        }
    }
    rows
}

fn gap_rows(projects: &BTreeSet<String>, at: &str) -> Vec<Normalized> {
    projects
        .iter()
        .map(|project| Normalized {
            dedupe: format!("gap:{project}:{at}"),
            row: ProjectEvent {
                at: at.to_string(),
                project_id: project.clone(),
                kind: "journal.gap".into(),
                detail: Some("the event log moved on before these events were read; some activity is missing".into()),
                ..ProjectEvent::default()
            },
        })
        .collect()
}

// ---------------------------------------------------------------- journal

/// A page request against one project.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PageQuery {
    pub project_id: String,
    /// Older than this sequence (newest first). Neither = from the newest.
    pub before: Option<i64>,
    /// Newer than this sequence (oldest first).
    pub after: Option<i64>,
    pub limit: usize,
    pub kinds: Vec<String>,
    pub session_id: Option<String>,
}

pub struct Page {
    pub events: Vec<ProjectEvent>,
    pub has_more: bool,
    /// With `has_more`: the sequence to pass as `before` (`after` for
    /// oldest-first pages) for the next page. Past the last event, or past
    /// the scanned rows when the scan budget ran out (then the page can be
    /// short, even empty).
    pub next: Option<i64>,
}

pub struct Journal {
    db: Connection,
}

impl Journal {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            crate::management::client_tokens::ensure_private_dir(parent)?;
        }
        let db = Connection::open(path)?;
        db.busy_timeout(Duration::from_secs(2))?;
        db.execute_batch(
            "PRAGMA journal_mode=DELETE;
             CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS events(seq INTEGER PRIMARY KEY AUTOINCREMENT, project_id TEXT NOT NULL,
                 session_id TEXT, at INTEGER NOT NULL, kind TEXT NOT NULL, dedupe TEXT NOT NULL UNIQUE, row TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS events_project ON events(project_id, seq);
             CREATE INDEX IF NOT EXISTS events_project_session ON events(project_id, session_id, seq);
             CREATE TABLE IF NOT EXISTS floors(project_id TEXT PRIMARY KEY, seq INTEGER NOT NULL);",
        )?;
        // r252: `kinds=` reads through these (a rare kind no longer scans the
        // project). Built once on the first open of an older journal.
        db.execute_batch(&format!(
            "CREATE INDEX IF NOT EXISTS events_project_family ON events(project_id, {FAMILY_SQL}, seq);
             CREATE INDEX IF NOT EXISTS events_project_session_family ON events(project_id, session_id, {FAMILY_SQL}, seq);"
        ))?;
        let journal = Self { db };
        if journal.meta("streamId")?.is_none() {
            journal.db.execute(
                "INSERT INTO meta VALUES('streamId', ?)",
                [uuid::Uuid::new_v4().to_string()],
            )?;
        }
        Ok(journal)
    }

    fn meta(&self, key: &str) -> anyhow::Result<Option<String>> {
        Ok(self.db.query_row("SELECT value FROM meta WHERE key=?", [key], |r| r.get(0)).optional()?)
    }

    pub fn stream_id(&self) -> String {
        self.meta("streamId").ok().flatten().unwrap_or_default()
    }

    /// Highest sequence ever assigned (survives pruning).
    pub fn assigned_seq(&self) -> anyhow::Result<i64> {
        Ok(self
            .db
            .query_row("SELECT seq FROM sqlite_sequence WHERE name='events'", [], |r| r.get(0))
            .optional()?
            .unwrap_or(0))
    }

    /// Highest pruned sequence of the project (0: nothing pruned).
    pub fn floor(&self, project_id: &str) -> anyhow::Result<i64> {
        Ok(self
            .db
            .query_row("SELECT seq FROM floors WHERE project_id=?", [project_id], |r| r.get(0))
            .optional()?
            .unwrap_or(0))
    }

    /// Validate a cursor of `project_id`: `Ok(seq)` or the 409
    /// `cursor_expired` details. `after` cursors must not reach into pruned
    /// rows; any cursor must be one this journal (stream) assigned.
    pub fn check_cursor(
        &self,
        project_id: &str,
        seq: i64,
        after: bool,
        wanted_stream: Option<&str>,
    ) -> anyhow::Result<Result<i64, Value>> {
        let stream_id = self.stream_id();
        let latest = self.assigned_seq()?;
        let floor = self.floor(project_id)?;
        let valid = wanted_stream.is_none_or(|s| s == stream_id)
            && if after { seq <= latest && seq >= floor } else { seq <= latest + 1 };
        Ok(if valid {
            Ok(seq)
        } else {
            Err(json!({
                "streamId": stream_id,
                "earliestCursor": format_cursor(floor),
                "latestCursor": format_cursor(latest),
            }))
        })
    }

    /// Up to `limit` matching rows past `bound` (exclusive), nearest first;
    /// `None` for a row that no longer parses (it still counts as read).
    fn select(&self, query: &PageQuery, bound: i64, newer: bool, limit: usize) -> anyhow::Result<Vec<(i64, Option<ProjectEvent>)>> {
        let mut kinds: Vec<Option<&str>> = query.kinds.iter().map(|kind| Some(kind.as_str())).collect();
        kinds.sort();
        kinds.dedup();
        if kinds.is_empty() {
            kinds.push(None);
        }
        // One indexed read per kind, merged: `family IN (...)` would sort.
        let mut rows = Vec::new();
        for kind in &kinds {
            rows.extend(self.select_kind(query, *kind, bound, newer, limit)?);
        }
        if kinds.len() > 1 {
            rows.sort_by_key(|(seq, _)| if newer { *seq } else { -*seq });
            rows.truncate(limit);
        }
        Ok(rows
            .into_iter()
            .map(|(seq, text)| {
                let row = serde_json::from_str::<ProjectEvent>(&text).ok().map(|mut row| {
                    row.cursor = format_cursor(seq);
                    row
                });
                (seq, row)
            })
            .collect())
    }

    fn select_kind(&self, query: &PageQuery, kind: Option<&str>, bound: i64, newer: bool, limit: usize) -> anyhow::Result<Vec<(i64, String)>> {
        let mut sql = String::from("SELECT seq, row FROM events WHERE project_id=?1");
        let mut values: Vec<rusqlite::types::Value> = vec![query.project_id.clone().into(), bound.into()];
        sql.push_str(if newer { " AND seq > ?2" } else { " AND seq < ?2" });
        if let Some(session) = &query.session_id {
            values.push(session.clone().into());
            sql.push_str(&format!(" AND session_id=?{}", values.len()));
        }
        if let Some(kind) = kind {
            values.push(kind.to_string().into());
            sql.push_str(&format!(" AND {FAMILY_SQL}=?{}", values.len()));
        }
        values.push((limit as i64).into());
        sql.push_str(&format!(" ORDER BY seq {} LIMIT ?{}", if newer { "ASC" } else { "DESC" }, values.len()));
        let mut statement = self.db.prepare_cached(&sql)?;
        let rows = statement
            .query_map(params_from_iter(values), |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Up to `query.limit` rows `visible` accepts (filtered rows do not
    /// count), reading at most [`SCAN_BUDGET`] matching rows.
    pub fn page(&self, query: &PageQuery, visible: &mut dyn FnMut(&ProjectEvent) -> bool) -> anyhow::Result<Page> {
        self.page_with_budget(query, SCAN_BUDGET, visible)
    }

    fn page_with_budget(
        &self,
        query: &PageQuery,
        budget: usize,
        visible: &mut dyn FnMut(&ProjectEvent) -> bool,
    ) -> anyhow::Result<Page> {
        let newer = query.after.is_some();
        let mut bound = query.after.or(query.before).unwrap_or(i64::MAX);
        let limit = query.limit.clamp(1, MAX_LIMIT);
        // Read at least a full page plus the row that proves `has_more`.
        let mut budget = budget.max(limit + 1);
        let mut batch = limit + 1;
        let mut events = Vec::new();
        let mut last_event = None;
        loop {
            let want = batch.min(budget);
            let rows = self.select(query, bound, newer, want)?;
            let exhausted = rows.len() < want;
            budget -= rows.len();
            for (seq, row) in rows {
                bound = seq;
                let Some(row) = row.filter(|row| visible(row)) else { continue };
                if events.len() == limit {
                    return Ok(Page { events, has_more: true, next: last_event });
                }
                last_event = Some(seq);
                events.push(row);
            }
            if exhausted {
                return Ok(Page { events, has_more: false, next: None });
            }
            if budget == 0 {
                // Unknown beyond here: continue after the rows read.
                return Ok(Page { events, has_more: true, next: Some(bound) });
            }
            batch = (batch * 2).min(SCAN_BATCH);
        }
    }

    /// Journal `rows` (duplicates ignored) and the meta values; returns the
    /// rows written, with their cursors. Nothing is ever deleted.
    fn commit(&mut self, rows: &[Normalized], meta: &[(&str, String)]) -> anyhow::Result<Vec<(i64, ProjectEvent)>> {
        let now = chrono::Utc::now().timestamp();
        let tx = self.db.transaction()?;
        let mut written = Vec::new();
        for Normalized { dedupe, row } in rows {
            let at = chrono::DateTime::parse_from_rfc3339(&row.at).map(|t| t.timestamp()).unwrap_or(now);
            let inserted = tx.execute(
                "INSERT OR IGNORE INTO events(project_id, session_id, at, kind, dedupe, row) VALUES(?,?,?,?,?,?)",
                params![row.project_id, row.cutex_session_id, at, row.kind, dedupe, serde_json::to_string(row)?],
            )?;
            if inserted == 1 {
                let seq = tx.last_insert_rowid();
                let mut row = row.clone();
                row.cursor = format_cursor(seq);
                written.push((seq, row));
            }
        }
        for (key, value) in meta {
            tx.execute(
                "INSERT INTO meta VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![key, value],
            )?;
        }
        tx.commit()?;
        Ok(written)
    }
}

// ---------------------------------------------------------------- hub

type Subscriber = (String, mpsc::SyncSender<(i64, ProjectEvent)>, Arc<AtomicBool>);

/// Journal + live subscribers of this daemon.
pub struct Hub {
    journal: Mutex<Journal>,
    subscribers: Mutex<Vec<Subscriber>>,
}

impl Hub {
    pub fn new(journal: Journal) -> Self {
        Self { journal: Mutex::new(journal), subscribers: Mutex::new(Vec::new()) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Journal> {
        self.journal.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn publish(&self, rows: &[(i64, ProjectEvent)]) {
        let mut subscribers = self.subscribers.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for (seq, row) in rows {
            subscribers.retain(|(project, sender, lagged)| {
                if *project != row.project_id {
                    return true;
                }
                match sender.try_send((*seq, row.clone())) {
                    Ok(()) => true,
                    Err(mpsc::TrySendError::Full(_)) => {
                        lagged.store(true, Ordering::SeqCst);
                        false
                    }
                    Err(mpsc::TrySendError::Disconnected(_)) => false,
                }
            });
        }
    }

    /// Journal and publish (the projector thread is the only writer).
    pub fn append(&self, rows: &[Normalized], meta: &[(&str, String)]) -> anyhow::Result<Vec<(i64, ProjectEvent)>> {
        let mut journal = self.lock();
        let written = journal.commit(rows, meta)?;
        // Under the journal lock: a stream registered under the same lock
        // sees each row exactly once.
        self.publish(&written);
        Ok(written)
    }
}

static HUB: OnceLock<Arc<Hub>> = OnceLock::new();

fn hub() -> Option<&'static Arc<Hub>> {
    HUB.get()
}

// ---------------------------------------------------------------- projector

/// Agent Management + session store facts, read once per tick.
struct LiveFacts {
    projects: BTreeMap<String, String>,
    threads: HashMap<String, String>,
    names: HashMap<String, String>,
    asks: std::cell::RefCell<Option<Option<crate::asks::store::AskState>>>,
}

impl LiveFacts {
    fn load() -> anyhow::Result<Self> {
        let snapshot = crate::memory::load_agent_management_snapshot()?;
        let mut ids: BTreeSet<String> =
            snapshot.agents.values().map(|agent| agent.cutex_session_id.as_str().to_string()).collect();
        ids.extend(snapshot.projects.values().map(|p| p.authorized_director_session.as_str().to_string()));
        let projects = ids
            .into_iter()
            .filter_map(|id| {
                let project = crate::memory::project_for_session(&snapshot, &id)?;
                Some((id, project.as_str().to_string()))
            })
            .collect();
        let store = crate::session::store::load_cutex_session_store_shared()?;
        let mut threads = HashMap::new();
        let mut names = HashMap::new();
        for (id, record) in &store.sessions {
            if let Some(thread) = record.app_server_thread_id() {
                threads.insert(id.clone(), thread.to_string());
            }
            names.insert(id.clone(), crate::session::metadata::cutex_session_display_name(record));
        }
        Ok(Self { projects, threads, names, asks: std::cell::RefCell::new(None) })
    }

    /// Read one delegation from the ask store (loaded once per tick).
    fn ask<T>(&self, ask_id: &str, read: impl FnOnce(&crate::asks::model::AskRecord) -> Option<T>) -> Option<T> {
        let mut cell = self.asks.borrow_mut();
        let state = cell.get_or_insert_with(|| {
            crate::asks::store::AskStore::open_default().and_then(|store| store.snapshot()).ok()
        });
        if !state.as_ref()?.asks.contains_key(ask_id) {
            let ask=crate::asks::store::AskStore::open_default().and_then(|store|store.lookup_ask(ask_id)).ok()??;
            state.as_mut()?.asks.insert(ask_id.to_owned(),ask);
        }
        read(state.as_ref()?.asks.get(ask_id)?)
    }
}

impl Facts for LiveFacts {
    fn project_of_session(&self, session: &str) -> Option<String> {
        self.projects.get(session).cloned()
    }
    fn own_thread(&self, session: &str) -> Option<String> {
        self.threads.get(session).cloned()
    }
    fn agent_name(&self, session: &str) -> Option<String> {
        self.names.get(session).cloned()
    }
    fn ask_project(&self, ask_id: &str) -> Option<String> {
        self.ask(ask_id, |ask| ask.project_id.clone())
    }
    fn ask_title(&self, ask_id: &str) -> Option<String> {
        self.ask(ask_id, |ask| Some(ask.title.clone()).filter(|title| !title.is_empty()))
    }
}

/// What the journal holds (the projector thread is its only writer).
#[derive(Clone, Default)]
pub(super) struct Persisted {
    pub(super) checkpoint: Option<(String, String)>,
    pub(super) initialized: bool,
    pub(super) memberships: Option<BTreeMap<String, String>>,
    pub(super) pending: Pending,
}

impl Persisted {
    fn load(journal: &Journal) -> anyhow::Result<Self> {
        let json = |key: &str| -> anyhow::Result<Option<Value>> {
            Ok(journal.meta(key)?.and_then(|text| serde_json::from_str(&text).ok()))
        };
        Ok(Self {
            checkpoint: json("sourceCheckpoint")?.and_then(|v| serde_json::from_value(v).ok()),
            initialized: journal.meta("initialized")?.is_some(),
            memberships: json("memberships")?.and_then(|v| serde_json::from_value(v).ok()),
            pending: json("pending")?.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default(),
        })
    }
}

#[derive(Default)]
struct TickState {
    persisted: Option<Persisted>,
    /// project → newest cursor not yet announced.
    owed: BTreeMap<String, i64>,
    last_ping: HashMap<String, Instant>,
    /// Membership and names, reloaded every [`FACTS_REFRESH`] (an idle
    /// daemon reads no store each second).
    facts: Option<(Instant, LiveFacts)>,
}

/// Start the project-log thread (daemon only; once).
pub fn start() -> anyhow::Result<()> {
    let path = crate::config::paths::runtime_dir()?.join("project-events").join("journal.sqlite3");
    let hub = Arc::new(Hub::new(Journal::open(&path)?));
    if HUB.set(hub.clone()).is_err() {
        return Ok(());
    }
    std::thread::Builder::new().name("cutex-project-events".into()).spawn(move || {
        let mut state = TickState::default();
        let mut last_error: Option<String> = None;
        loop {
            match tick(&hub, &mut state) {
                Ok(()) => last_error = None,
                Err(error) => {
                    state.persisted = None;
                    let text = format!("{error:#}");
                    if last_error.as_deref() != Some(text.as_str()) {
                        eprintln!("project events warning: {text}");
                        last_error = Some(text);
                    }
                }
            }
            std::thread::sleep(TICK);
        }
    })?;
    Ok(())
}

/// One projector pass over `pages` (testable without the repository):
/// rows, the new position and whether the log was caught up. `source`
/// returns a page or `None` when the position expired.
pub(super) struct Pass {
    pub rows: Vec<Normalized>,
    pub position: Option<(String, String)>,
    pub caught_up: bool,
    pub gap: bool,
}

pub(super) fn project_pass(
    mut source: impl FnMut(Option<&(String, String)>) -> anyhow::Result<Option<super::model::EventPage>>,
    persisted: &mut Persisted,
    facts: &dyn Facts,
) -> anyhow::Result<Pass> {
    let backfill = !persisted.initialized;
    let mut position = persisted.checkpoint.clone();
    let mut rows = Vec::new();
    let mut gap = false;
    let mut caught_up = false;
    for _ in 0..PAGES_PER_TICK {
        let Some(page) = source(position.as_ref())? else {
            // The event log moved past the checkpoint: go on from its start.
            gap |= position.is_some() && persisted.initialized;
            position = None;
            continue;
        };
        for event in &page.events {
            let own = event.cutex.as_ref().is_some_and(|m| m.method.starts_with("cutex/projectEvents/"));
            if !own {
                for mut normalized in normalize(event, facts, &mut persisted.pending) {
                    normalized.row.backfilled = backfill;
                    rows.push(normalized);
                }
            }
            position = Some((event.stream_id.clone(), event.cursor.clone()));
        }
        if position.is_none() {
            position = page.checkpoint.cursor.clone().map(|cursor| (page.stream_id.clone(), cursor));
        }
        if !page.has_more {
            caught_up = true;
            break;
        }
    }
    Ok(Pass { rows, position, caught_up, gap })
}

fn tick(hub: &Hub, state: &mut TickState) -> anyhow::Result<()> {
    let repository = super::repository::management_v2_repository()?;
    let (loaded_at, facts) = match state.facts.take() {
        Some((at, facts)) if at.elapsed() < FACTS_REFRESH => (at, facts),
        _ => (Instant::now(), LiveFacts::load()?),
    };
    // Ask lookups are per tick.
    facts.asks.replace(None);
    let previous = match state.persisted.take() {
        Some(persisted) => persisted,
        None => Persisted::load(&hub.lock())?,
    };
    let mut next = previous.clone();
    let pass = project_pass(
        |position| {
            let query = ReplayQuery {
                stream_id: position.map(|(stream, _)| stream.clone()),
                after: position.map(|(_, cursor)| cursor.clone()),
                limit: PAGE_LIMIT,
                cutex_session_id: None,
            };
            match repository.page(query) {
                Ok(page) => Ok(Some(page)),
                Err(ReplayError::CursorExpired { .. } | ReplayError::StreamChanged { .. }) => Ok(None),
                Err(error) => Err(error.into()),
            }
        },
        &mut next,
        &facts,
    )?;
    let now = crate::management::instance::now_utc();
    let mut rows = pass.rows;
    if pass.gap {
        rows.extend(gap_rows(&facts.projects.values().cloned().collect(), &now));
    }
    if let Some(memberships) = &previous.memberships {
        rows.extend(membership_rows(memberships, &facts.projects, &now, &facts));
    }
    next.memberships = Some(facts.projects.clone());
    next.checkpoint = pass.position.or(previous.checkpoint.clone());
    next.initialized = previous.initialized || pass.caught_up;
    let unchanged = rows.is_empty()
        && next.checkpoint == previous.checkpoint
        && next.initialized == previous.initialized
        && next.memberships == previous.memberships
        && next.pending == previous.pending;
    if !unchanged {
        let mut meta = vec![
            ("memberships", serde_json::to_string(&next.memberships)?),
            ("pending", serde_json::to_string(&next.pending)?),
        ];
        if let Some(checkpoint) = &next.checkpoint {
            meta.push(("sourceCheckpoint", serde_json::to_string(checkpoint)?));
        }
        if next.initialized && !previous.initialized {
            meta.push(("initialized", now.clone()));
        }
        let written = hub.append(&rows, &meta)?;
        if previous.initialized {
            for (seq, row) in &written {
                state.owed.insert(row.project_id.clone(), *seq);
            }
        }
    }
    state.persisted = Some(next);
    state.facts = Some((loaded_at, facts));
    announce(repository, state);
    Ok(())
}

/// `cutex/projectEvents/appended` for projects with unannounced rows, at
/// most every [`PING_INTERVAL`] per project.
fn announce(repository: &super::repository::EventRepository, state: &mut TickState) {
    let due: Vec<(String, i64)> = state
        .owed
        .iter()
        .filter(|(project, _)| state.last_ping.get(*project).is_none_or(|at| at.elapsed() >= PING_INTERVAL))
        .map(|(project, seq)| (project.clone(), *seq))
        .collect();
    for (project, seq) in due {
        let sent = super::server::append_cutex_event(
            repository,
            super::services::HOST_EVENT_SESSION_ID,
            super::model::EventCorrelation::default(),
            APPENDED_EVENT,
            json!({ "projectId": project, "cursor": format_cursor(seq) }),
        );
        if let Err(error) = sent {
            eprintln!("project events: announce {project}: {error:#}");
            return;
        }
        state.owed.remove(&project);
        state.last_ping.insert(project, Instant::now());
    }
    state.last_ping.retain(|_, at| at.elapsed() < PING_INTERVAL * 4);
}

// ---------------------------------------------------------------- HTTP

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    /// `GET /v2/projects/{projectId}/events`
    Events(String),
    /// `GET /v2/projects/{projectId}/events/stream`
    Stream(String),
    /// `GET /v2/projects/{projectId}/background` (what runs now)
    Background(String),
}

impl Route {
    pub fn project_id(&self) -> &str {
        match self {
            Self::Events(id) | Self::Stream(id) | Self::Background(id) => id,
        }
    }
}

pub fn parse_route(path: &str) -> Option<Route> {
    let rest = path.strip_prefix("/v2/projects/")?;
    let (project, tail) = rest.split_once('/')?;
    let project = super::attachments::percent_decode(project)?;
    crate::agent_management::ProjectId::new(project.clone()).ok()?;
    match tail {
        "events" => Some(Route::Events(project)),
        "events/stream" => Some(Route::Stream(project)),
        "background" => Some(Route::Background(project)),
        _ => None,
    }
}

fn unavailable(stream: &mut TcpStream) -> anyhow::Result<()> {
    super::server::write_v2_error(
        stream,
        503,
        "Service Unavailable",
        "project_events_unavailable",
        "the project log is not running in this process",
        true,
        json!({}),
    )
}

fn bad_request(stream: &mut TcpStream, message: &str) -> anyhow::Result<()> {
    super::server::write_v2_error(stream, 400, "Bad Request", "invalid_request", message, false, json!({}))
}

fn cursor_expired(stream: &mut TcpStream, details: Value) -> anyhow::Result<()> {
    super::server::write_v2_error(
        stream,
        409,
        "Conflict",
        "cursor_expired",
        "the project event cursor is outside the retained rows; read the newest page again",
        false,
        details,
    )
}

/// A query value, percent-decoded: clients built on `URLSearchParams`
/// send `after=pe%3A12` and `kinds=turn%2Cask`.
fn query_param(path: &str, key: &str) -> Option<String> {
    let query = path.split_once('?')?.1;
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.into_owned())
}

/// `kinds=` and `sessionId=` of a request (shared by both routes).
fn filters(request: &SimpleHttpRequest) -> Result<(Vec<String>, Option<String>), String> {
    let query = |key: &str| query_param(&request.path, key);
    let mut kinds = Vec::new();
    for kind in query("kinds").unwrap_or_default().split(',').map(str::trim).filter(|k| !k.is_empty()) {
        if !KIND_PREFIXES.contains(&kind) {
            return Err(format!("unknown kind: {kind} (expected {})", KIND_PREFIXES.join(", ")));
        }
        kinds.push(kind.to_string());
    }
    Ok((kinds, query("sessionId").filter(|s| !s.is_empty())))
}

fn row_matches(row: &ProjectEvent, kinds: &[String], session: Option<&str>) -> bool {
    (kinds.is_empty() || kinds.iter().any(|kind| kind == row.kind_prefix()))
        && session.is_none_or(|session| row.cutex_session_id.as_deref() == Some(session))
}

/// Serve a project-log route; `visible` filters rows for non-owners.
pub fn handle(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    route: &Route,
    visible: &mut dyn FnMut(&ProjectEvent) -> bool,
) -> anyhow::Result<()> {
    match route {
        Route::Events(project) => handle_page(stream, request, project, visible),
        Route::Stream(project) => handle_stream(stream, request, project, visible),
        Route::Background(_) => anyhow::bail!("the background route is served by handle_background"),
    }
}

fn handle_page(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    project: &str,
    visible: &mut dyn FnMut(&ProjectEvent) -> bool,
) -> anyhow::Result<()> {
    let Some(hub) = hub() else { return unavailable(stream) };
    let query = |key: &str| query_param(&request.path, key).filter(|v| !v.is_empty());
    let (kinds, session_id) = match filters(request) {
        Ok(filters) => filters,
        Err(message) => return bad_request(stream, &message),
    };
    let limit = match query("limit").map(|v| v.parse::<usize>()) {
        None => DEFAULT_LIMIT,
        Some(Ok(limit)) if (1..=MAX_LIMIT).contains(&limit) => limit,
        Some(_) => return bad_request(stream, &format!("limit must be 1..{MAX_LIMIT}")),
    };
    let (before, after) = (query("before"), query("after"));
    if before.is_some() && after.is_some() {
        return bad_request(stream, "use either before or after");
    }
    let wanted_stream = query("streamId");
    let journal = hub.lock();
    let checked = |text: Option<String>, after: bool| -> anyhow::Result<Result<Option<i64>, Option<Value>>> {
        let Some(text) = text else { return Ok(Ok(None)) };
        let Some(seq) = parse_cursor(&text) else { return Ok(Err(None)) };
        Ok(journal.check_cursor(project, seq, after, wanted_stream.as_deref())?.map(Some).map_err(Some))
    };
    let (before, after) = match (checked(before, false)?, checked(after, true)?) {
        (Ok(before), Ok(after)) => (before, after),
        (Err(None), _) | (_, Err(None)) => {
            drop(journal);
            return bad_request(stream, "cursors look like pe:<n>");
        }
        (Err(Some(details)), _) | (_, Err(Some(details))) => {
            drop(journal);
            return cursor_expired(stream, details);
        }
    };
    let page_query = PageQuery { project_id: project.to_string(), before, after, limit, kinds, session_id };
    let page = journal.page(&page_query, visible)?;
    let body = json!({
        "projectId": project,
        "streamId": journal.stream_id(),
        "cursor": format_cursor(journal.assigned_seq()?),
        "earliestCursor": format_cursor(journal.floor(project)?),
        "order": if page_query.after.is_some() { "oldestFirst" } else { "newestFirst" },
        "hasMore": page.has_more,
        "nextCursor": page.next.map(format_cursor),
        "events": page.events,
    });
    drop(journal);
    write_json_response(stream, 200, "OK", &body)
}

fn write_row(stream: &mut impl Write, seq: i64, row: &ProjectEvent) -> anyhow::Result<()> {
    write!(stream, "id: {}\nevent: {STREAM_EVENT}\ndata: {}\n\n", format_cursor(seq), serde_json::to_string(row)?)?;
    stream.flush()?;
    Ok(())
}

fn handle_stream(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    project: &str,
    visible: &mut dyn FnMut(&ProjectEvent) -> bool,
) -> anyhow::Result<()> {
    let Some(hub) = hub() else { return unavailable(stream) };
    let (kinds, session_id) = match filters(request) {
        Ok(filters) => filters,
        Err(message) => return bad_request(stream, &message),
    };
    let after_text = query_param(&request.path, "after")
        .filter(|v| !v.is_empty())
        .or_else(|| request.headers.get("last-event-id").cloned());
    let wanted_stream = query_param(&request.path, "streamId");
    let (receiver, lagged, backlog) = {
        let journal = hub.lock();
        let after = match after_text {
            None => journal.assigned_seq()?,
            Some(text) => {
                let Some(seq) = parse_cursor(&text) else {
                    drop(journal);
                    return bad_request(stream, "cursors look like pe:<n>");
                };
                match journal.check_cursor(project, seq, true, wanted_stream.as_deref())? {
                    Ok(seq) => seq,
                    Err(details) => {
                        drop(journal);
                        return cursor_expired(stream, details);
                    }
                }
            }
        };
        let (sender, receiver) = mpsc::sync_channel(SUBSCRIBER_CAPACITY);
        let lagged = Arc::new(AtomicBool::new(false));
        hub.subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((project.to_string(), sender, lagged.clone()));
        let query = PageQuery { project_id: project.to_string(), ..PageQuery::default() };
        let mut backlog = Vec::new();
        let mut bound = after;
        while backlog.len() < BACKLOG_LIMIT {
            let rows = journal.select(&query, bound, true, MAX_LIMIT)?;
            let done = rows.len() < MAX_LIMIT;
            if let Some((seq, _)) = rows.last() {
                bound = *seq;
            }
            backlog.extend(rows.into_iter().filter_map(|(seq, row)| Some((seq, row?))));
            if done {
                break;
            }
        }
        (receiver, lagged, backlog)
    };
    stream.write_all(crate::http::server::sse_response_head().as_bytes())?;
    stream.flush()?;
    let mut matching = |row: &ProjectEvent| row_matches(row, &kinds, session_id.as_deref()) && visible(row);
    let watch = crate::management::listeners::RevocationWatch::for_stream(stream)?;
    stream_rows(
        stream,
        backlog,
        receiver,
        lagged,
        watch,
        &mut matching,
    )
}

/// Body of the stream after the headers (testable without a socket).
pub fn stream_rows(
    stream: &mut impl Write,
    backlog: Vec<(i64, ProjectEvent)>,
    receiver: mpsc::Receiver<(i64, ProjectEvent)>,
    lagged: Arc<AtomicBool>,
    mut watch: crate::management::listeners::RevocationWatch,
    visible: &mut dyn FnMut(&ProjectEvent) -> bool,
) -> anyhow::Result<()> {
    let mut last = None;
    for (seq, row) in &backlog {
        if visible(row) {
            write_row(stream, *seq, row)?;
        }
        last = Some(*seq);
    }
    let mut last_write = Instant::now();
    loop {
        if watch.disconnected()? { return Ok(()); }
        if let Some(token_id) = watch.revoked() {
            crate::management::listeners::write_revoked_frame(stream, &token_id)?;
            return Ok(());
        }
        match receiver.recv_timeout(crate::management::listeners::STREAM_TICK) {
            Ok((seq, row)) => {
                if last.is_some_and(|last| seq <= last) {
                    continue;
                }
                last = Some(seq);
                if visible(&row) {
                    write_row(stream, seq, &row)?;
                    last_write = Instant::now();
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if last_write.elapsed() >= HEARTBEAT {
                    stream.write_all(b": keepalive\n\n")?;
                    stream.flush()?;
                    last_write = Instant::now();
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if lagged.load(Ordering::SeqCst) {
                    super::server::write_sse_lagged(stream, last.map(format_cursor).as_deref())?;
                }
                return Ok(());
            }
        }
    }
}

// ---------------------------------------------------------------- background

/// Last `/background` answers per project (unfiltered).
static BACKGROUND: OnceLock<Mutex<HashMap<String, (Instant, Value)>>> = OnceLock::new();

/// `GET /v2/projects/{projectId}/background`: per agent of the project,
/// what runs now: Claude sub-agents and background commands
/// (`claude/tasks/list`), Codex background terminals of the agent's thread
/// (`thread/backgroundTerminals/list`) and unfinished Jobs. An offline Claude
/// runtime's last known tasks are listed with state `unknown`. `stop` names
/// the existing request that stops an item (owner only; `null` for others).
/// `sees_session` filters agents for non-owners.
pub fn handle_background(
    stream: &mut TcpStream,
    context: crate::management::server::ManagementRequestContext,
    project: &str,
    sees_session: &mut dyn FnMut(&str) -> bool,
    owner: bool,
) -> anyhow::Result<()> {
    let cache = BACKGROUND.get_or_init(Default::default);
    let cached = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(project)
        .filter(|(at, _)| at.elapsed() < BACKGROUND_CACHE)
        .map(|(_, value)| value.clone());
    let mut body = match cached {
        Some(body) => body,
        None => {
            let Some(body) = background_resource(context, project)? else {
                return super::server::write_v2_error(
                    stream,
                    404,
                    "Not Found",
                    "project_not_found",
                    "no project has this id",
                    false,
                    json!({ "projectId": project }),
                );
            };
            let mut cache = cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            cache.retain(|_, (at, _)| at.elapsed() < BACKGROUND_CACHE);
            cache.insert(project.to_string(), (Instant::now(), body.clone()));
            body
        }
    };
    filter_background(&mut body, sees_session, owner);
    write_json_response(stream, 200, "OK", &body)
}

/// Keep the agents `sees_session` accepts; non-owners get no stop requests.
pub fn filter_background(body: &mut Value, sees_session: &mut dyn FnMut(&str) -> bool, owner: bool) {
    if let Some(agents) = body["agents"].as_array_mut() {
        agents.retain(|agent| agent["cutexSessionId"].as_str().is_some_and(&mut *sees_session));
        if !owner {
            for item in agents.iter_mut().flat_map(|agent| agent["items"].as_array_mut().into_iter().flatten()) {
                item["stop"] = Value::Null;
            }
        }
    }
}

fn background_resource(
    context: crate::management::server::ManagementRequestContext,
    project: &str,
) -> anyhow::Result<Option<Value>> {
    let snapshot = crate::memory::load_agent_management_snapshot()?;
    let Ok(project_id) = crate::agent_management::ProjectId::new(project.to_string()) else { return Ok(None) };
    if !crate::memory::project_is_known(&snapshot, &project_id) {
        return Ok(None);
    }
    let store = crate::session::store::load_cutex_session_store_shared()?;
    let pending = match hub() {
        Some(hub) => Persisted::load(&hub.lock())?.pending,
        None => Pending::default(),
    };
    let jobs = super::human_jobs::running_jobs();
    let agents: Vec<Value> = crate::memory::project_member_session_ids(&snapshot, &project_id)
        .into_iter()
        .map(|id| {
            let record = store.sessions.get(&id);
            let claude = record.is_some_and(|r| r.claude_session.is_some());
            let thread = record.and_then(|r| r.app_server_thread_id()).map(str::to_string);
            let online = match ((context.load_runtime_status)(&id), record, &thread) {
                (Ok(Some(status)), Some(record), Some(thread)) => {
                    status.connected && status.runtime_generation == record.runtime_generation && status.thread_id == *thread
                }
                _ => false,
            };
            let (mut items, complete) = match (&thread, online) {
                (Some(thread), true) => runtime_items(context, &id, thread, claude),
                _ if claude => (offline_claude_items(&pending, &id), false),
                _ => (Vec::new(), false),
            };
            // Claude lists carry no start time: the projector saw the start.
            for item in items.iter_mut().filter(|item| item["startedAt"].is_null() && item["source"] == "claude") {
                let key = format!("{id}|{}", item["id"].as_str().unwrap_or_default());
                if let Some(at) = pending.running_tasks.get(&key).and_then(|task| task.get("startedAt")) {
                    item["startedAt"] = at.clone();
                }
            }
            items.extend(jobs.iter().flatten().filter(|job| job["sessionId"] == id.as_str()).map(job_item));
            json!({
                "cutexSessionId": id,
                "agentName": record.map(crate::session::metadata::cutex_session_display_name),
                "backend": if claude { "claude" } else { "codex" },
                "runtime": if online { "online" } else { "offline" },
                "complete": complete,
                "items": items,
            })
        })
        .collect();
    Ok(Some(json!({
        "projectId": project,
        "generatedAt": crate::management::instance::now_utc(),
        "jobsAvailable": jobs.is_some(),
        "agents": agents,
    })))
}

/// Items an online runtime reports; `complete` = the list call answered.
fn runtime_items(
    context: crate::management::server::ManagementRequestContext,
    session: &str,
    thread: &str,
    claude: bool,
) -> (Vec<Value>, bool) {
    let method = if claude { "claude/tasks/list" } else { "thread/backgroundTerminals/list" };
    let message = json!({
        "id": format!("cutex-project-background-{}", uuid::Uuid::new_v4()),
        "method": method,
        "params": { "threadId": thread },
    });
    match (context.forward_native_request)(session, message) {
        Ok(reply) => items_from_reply(session, thread, claude, &reply).map_or((Vec::new(), false), |items| (items, true)),
        Err(_) => (Vec::new(), false),
    }
}

/// The running items of a `claude/tasks/list` / `thread/backgroundTerminals/list`
/// reply; None for an error reply.
pub(super) fn items_from_reply(session: &str, thread: &str, claude: bool, reply: &Value) -> Option<Vec<Value>> {
    let result = reply.get("result")?;
    let items = if claude {
        result["tasks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|task| matches!(task["status"].as_str(), Some("running" | "pending")))
            .filter_map(|task| claude_item(session, thread, task, task["status"].as_str().unwrap_or("running")))
            .collect()
    } else {
        result["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|terminal| {
                let process = text(&terminal["processId"])?;
                Some(json!({
                    "kind": "background",
                    "source": "codex",
                    "id": process,
                    "title": terminal["command"].as_str().and_then(preview),
                    "detail": text(&terminal["cwd"]),
                    "turnId": null,
                    "threadId": thread,
                    "startedAt": null,
                    "state": "running",
                    "stop": {
                        "kind": "native",
                        "cutexSessionId": session,
                        "method": "thread/backgroundTerminals/terminate",
                        "params": { "threadId": thread, "processId": process },
                    },
                }))
            })
            .collect()
    };
    Some(items)
}

fn claude_item(session: &str, thread: &str, task: &Value, state: &str) -> Option<Value> {
    let task_id = text(&task["taskId"])?;
    let kind = match task["taskType"].as_str() {
        Some("local_agent") => "subagent",
        Some("local_bash") => "background",
        _ => "task",
    };
    let command = task["command"].as_str().and_then(preview);
    let title = task["description"].as_str().and_then(preview).or_else(|| command.clone());
    let stop = (state != "unknown").then(|| {
        json!({
            "kind": "native",
            "cutexSessionId": session,
            "method": "claude/tasks/stop",
            "params": { "threadId": thread, "taskId": task_id },
        })
    });
    Some(json!({
        "kind": kind,
        "source": "claude",
        "id": task_id,
        "title": title,
        "detail": if title == command { None } else { command },
        "turnId": text(&task["turnId"]),
        "threadId": thread,
        "startedAt": task.get("startedAt").cloned().unwrap_or(Value::Null),
        "state": state,
        "stop": stop,
    }))
}

/// The last known running tasks of an offline Claude runtime (state unknown).
fn offline_claude_items(pending: &Pending, session: &str) -> Vec<Value> {
    let prefix = format!("{session}|");
    pending
        .running_tasks
        .iter()
        .filter(|(key, _)| key.starts_with(&prefix))
        .filter_map(|(_, task)| claude_item(session, task["threadId"].as_str().unwrap_or_default(), task, "unknown"))
        .collect()
}

fn job_item(job: &Value) -> Value {
    let started = job["createdAt"]
        .as_i64()
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
        .map(|at| crate::management::instance::normalize_utc(&at.to_rfc3339()));
    json!({
        "kind": "job",
        "source": "job",
        "id": job["jobId"],
        "title": job["command"].as_str().and_then(preview).or_else(|| text(&job["jobId"])),
        "detail": text(&job["state"]),
        "turnId": null,
        "threadId": null,
        "startedAt": started,
        "state": "running",
        "stop": {
            "kind": "job",
            "jobId": job["jobId"],
            "expectedRevision": job["revision"],
        },
    })
}

#[cfg(test)]
#[path = "project_events_tests.rs"]
mod tests;
