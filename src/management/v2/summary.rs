//! Host summary (capability `host-summary`, cutex-desktop
//! docs/research/cutex-mobile-api.md §4–5):
//!
//! - `GET /v2/summary` → every agent's status, what it needs from the owner,
//!   a preview, the last turn and unread turns, plus projects and the summary
//!   event cursor this snapshot reflects.
//! - `GET /v2/summary/events` (SSE, `id:` = cursor, `Last-Event-ID` /
//!   `?after=`, 25 s heartbeat): `cutex/summary/agentChanged {agent}`,
//!   `agentRemoved {id}`, `turnEnded {id, turnId, status, preview}`,
//!   `approvalsChanged {count}`, `asksChanged {count}`, `resync {reason}`;
//!   `messageToOwner {id, notificationId, preview, level, needsReply}` for
//!   owner-notify records whose channels include the phone.
//!
//! The daemon's summary thread ([`start`]) consumes the Management v2 event
//! log (turn ends, last agent message, owner input) into per-agent
//! projections and diffs the synthesised rows every few seconds; events go to
//! a journal (`<cutex home>/summary/summary.sqlite3`) kept for 7 days.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::net::TcpStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::model::{EventEnvelope, NativeMessageKind};
use super::repository::{EventRepository, ReplayError, ReplayQuery};
use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::management::server::ManagementRequestContext;

pub const SUMMARY_PATH: &str = "/v2/summary";
pub const SUMMARY_EVENTS_PATH: &str = "/v2/summary/events";
pub const SUMMARY_READ_PATH: &str = "/v2/summary/read";

const PREVIEW_CHARS: usize = 120;
const TITLE_CHARS: usize = 160;
const RETENTION_SECONDS: i64 = 7 * 24 * 3600;
const MAX_JOURNAL_ROWS: i64 = 100_000;
const HEARTBEAT: Duration = Duration::from_secs(25);
const SUBSCRIBER_CAPACITY: usize = 1024;
const TICK: Duration = Duration::from_secs(1);
/// Rows are re-synthesised at least this often (approvals, asks and the
/// session store also change without a consumed native event).
const FULL_REFRESH: Duration = Duration::from_secs(5);
const PAGE_LIMIT: usize = 500;

// ---------------------------------------------------------------- projections

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LastTurn {
    pub turn_id: String,
    pub status: String,
    pub ended_at: String,
}

/// What the event log tells about one agent beyond the session resource.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentProjection {
    #[serde(default)]
    pub last_turn: Option<LastTurn>,
    #[serde(default)]
    pub unread_turns: u64,
    #[serde(default)]
    pub preview: Option<String>,
    /// When the owner last read the agent (notification ack): turns that
    /// ended at or before it are not unread, whenever they are consumed.
    #[serde(default)]
    pub read_at: Option<String>,
}

fn timestamp(value: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(value).ok()
}

impl AgentProjection {
    /// The owner read the agent at `at` (RFC 3339).
    pub fn mark_read(&mut self, at: &str) {
        let newer = match (self.read_at.as_deref().and_then(timestamp), timestamp(at)) {
            (Some(current), Some(new)) => new > current,
            _ => true,
        };
        if newer {
            self.read_at = Some(at.to_string());
        }
        self.unread_turns = 0;
    }

    fn read_covers(&self, ended_at: &str) -> bool {
        match (self.read_at.as_deref().and_then(timestamp), timestamp(ended_at)) {
            (Some(read), Some(ended)) => ended <= read,
            _ => false,
        }
    }
}

/// A summary event before it is journaled: (name, data).
pub type PendingSummaryEvent = (String, Value);

/// Collapse whitespace and cut to `max` characters (with `…`).
pub fn preview_text(text: &str, max: usize) -> Option<String> {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    if collapsed.chars().count() <= max {
        return Some(collapsed);
    }
    let mut cut: String = collapsed.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    Some(cut)
}

/// Apply one event; returns the summary event it causes (turn ends).
pub fn reduce(
    projections: &mut HashMap<String, AgentProjection>,
    event: &EventEnvelope,
) -> Option<PendingSummaryEvent> {
    if event.cutex_session_id == super::services::HOST_EVENT_SESSION_ID {
        return None;
    }
    let id = event.cutex_session_id.clone();
    if let Some(native) = &event.native {
        if native.kind != NativeMessageKind::Notification {
            return None;
        }
        let method = native.message["method"].as_str().unwrap_or_default();
        let params = &native.message["params"];
        match method {
            "item/completed" if params.pointer("/item/type").and_then(Value::as_str) == Some("agentMessage") => {
                if let Some(text) = params.pointer("/item/text").and_then(Value::as_str) {
                    if let Some(preview) = preview_text(text, PREVIEW_CHARS) {
                        projections.entry(id).or_default().preview = Some(preview);
                    }
                }
                None
            }
            "turn/completed" => {
                let status = params
                    .pointer("/turn/status")
                    .and_then(Value::as_str)
                    .filter(|status| matches!(*status, "completed" | "failed" | "interrupted"))?
                    .to_string();
                let turn_id = params
                    .pointer("/turn/id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| event.correlation.turn_id.clone())
                    .unwrap_or_default();
                let projection = projections.entry(id.clone()).or_default();
                projection.last_turn = Some(LastTurn {
                    turn_id: turn_id.clone(),
                    status: status.clone(),
                    ended_at: crate::management::instance::normalize_utc(&event.received_at),
                });
                if !projection.read_covers(&event.received_at) {
                    projection.unread_turns = projection.unread_turns.saturating_add(1);
                }
                Some((
                    "cutex/summary/turnEnded".to_string(),
                    json!({
                        "id": id,
                        "turnId": turn_id,
                        "status": status,
                        "preview": projection.preview,
                    }),
                ))
            }
            _ => None,
        }
    } else if let Some(cutex) = &event.cutex {
        if cutex.method == crate::owner_notify::CREATED_EVENT {
            return message_to_owner(&id, &cutex.params["notification"]);
        }
        if cutex.method == "cutex/userInput/submitted" {
            projections.entry(id).or_default().mark_read(&event.received_at);
        }
        None
    } else {
        None
    }
}

/// `cutex/summary/messageToOwner` for an owner-notify record whose channels
/// include the phone (contract owner-notify §5).
pub fn message_to_owner(agent_id: &str, record: &Value) -> Option<PendingSummaryEvent> {
    let phone = record["channels"].as_array()?.iter().any(|channel| channel == "phone");
    // Cutex's own notices reach the phone only when notification levels
    // decided it (`actions` present); older ones may still list the phone.
    if !phone || (record["origin"] == "system" && record.get("actions").is_none()) {
        return None;
    }
    Some((
        "cutex/summary/messageToOwner".to_string(),
        json!({
            "id": agent_id,
            "notificationId": record["notificationId"],
            "preview": preview_text(record["text"].as_str().unwrap_or_default(), PREVIEW_CHARS),
            "level": record["level"],
            "needsReply": record["needsReply"].as_bool().unwrap_or(false),
        }),
    ))
}

// ---------------------------------------------------------------- needs

fn title(text: &str) -> String {
    preview_text(text, TITLE_CHARS).unwrap_or_default()
}

/// Characters of a command approval title.
pub const COMMAND_TITLE_CHARS: usize = 80;

/// Shell-like tokens of `text` with their byte ranges (quotes removed in the
/// value; `"…"` and `'…'` group, backslashes are literal as on Windows).
fn shell_tokens(text: &str) -> Vec<(String, usize, usize)> {
    let mut tokens = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some(&(start, c)) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let mut value = String::new();
        let mut end = start;
        while let Some(&(at, c)) = chars.peek() {
            if c.is_whitespace() {
                break;
            }
            chars.next();
            end = at + c.len_utf8();
            if c == '"' || c == '\'' {
                let quote = c;
                for (at, inner) in chars.by_ref() {
                    end = at + inner.len_utf8();
                    if inner == quote {
                        break;
                    }
                    value.push(inner);
                }
            } else {
                value.push(c);
            }
        }
        tokens.push((value, start, end));
    }
    tokens
}

/// The command a shell wrapper runs, or None when `text` is not a wrapper:
/// `pwsh|powershell [-flags] -Command|-c X`, `cmd [/flags] /c|/k X`,
/// `bash|sh|zsh|dash [-flags] -c|-lc|-ic X`.
fn unwrap_shell(text: &str) -> Option<String> {
    let tokens = shell_tokens(text);
    let (program, _, _) = tokens.first()?;
    let name = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    let is_command_flag = |flag: &str| -> bool {
        let flag = flag.to_ascii_lowercase();
        match name {
            "pwsh" | "powershell" => matches!(flag.as_str(), "-command" | "-c" | "/command" | "/c" | "-commandwithargs" | "-cwa"),
            "cmd" => matches!(flag.as_str(), "/c" | "/k" | "/s/c" | "/d/c"),
            "bash" | "sh" | "zsh" | "dash" => {
                flag.starts_with('-') && !flag.starts_with("--") && flag.ends_with('c') && flag.len() <= 4
            }
            _ => false,
        }
    };
    let index = tokens.iter().skip(1).position(|(token, _, _)| is_command_flag(token))? + 1;
    let rest = &tokens[index + 1..];
    match rest {
        [] => None,
        [(single, _, _)] => Some(single.clone()),
        [(_, start, _), ..] => Some(text[*start..].trim().to_string()),
    }
}

/// A readable approval title for a command line: shell wrappers removed
/// (also nested), whitespace collapsed, cut to [`COMMAND_TITLE_CHARS`].
pub fn command_title(command: &str) -> String {
    let mut current = command.trim().to_string();
    for _ in 0..3 {
        match unwrap_shell(&current) {
            Some(inner) if !inner.trim().is_empty() => current = inner.trim().to_string(),
            _ => break,
        }
    }
    preview_text(&current, COMMAND_TITLE_CHARS).unwrap_or_else(|| title(command))
}

/// A pending native server request as a need: (kind, title).
pub fn native_need(native: &Value) -> Option<(&'static str, String)> {
    let method = native["method"].as_str()?;
    let params = &native["params"];
    Some(match method {
        "item/commandExecution/requestApproval" => {
            let command = match &params["command"] {
                Value::String(command) => command.clone(),
                Value::Array(parts) => parts
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" "),
                _ => params["reason"].as_str().unwrap_or("Run a command").to_string(),
            };
            ("approval", command_title(&command))
        }
        "item/fileChange/requestApproval" => (
            "approval",
            title(params["reason"].as_str().unwrap_or("Apply file changes")),
        ),
        "item/permissions/requestApproval" => (
            "approval",
            title(params["reason"].as_str().unwrap_or("Grant permissions")),
        ),
        "item/tool/requestUserInput" => (
            "question",
            title(
                params
                    .pointer("/questions/0/question")
                    .and_then(Value::as_str)
                    .unwrap_or("Answer a question"),
            ),
        ),
        "mcpServer/elicitation/request" => (
            "question",
            title(params["message"].as_str().unwrap_or("Answer a question")),
        ),
        _ => ("approval", title(method)),
    })
}

/// The notification events an ask summary matches, each with the 需要你
/// kind it shows as (highest level wins, ties: the first). Closed asks match
/// none.
pub fn ask_events(ask: &Value) -> Vec<(&'static str, &'static str)> {
    if !ask["closed"].is_null() {
        return Vec::new();
    }
    let owner = ask.pointer("/requester/kind").and_then(Value::as_str) == Some("owner");
    let execution = ask["execution"].as_str().unwrap_or_default();
    if matches!(execution, "failed" | "rejected" | "cancelled") {
        let event = if owner { "delegation.failed.owner" } else { "delegation.failed.agent" };
        return vec![(event, "delegationFailed")];
    }
    let mut events = Vec::new();
    if ask["ownerQuestions"].as_u64().unwrap_or(0) > 0 {
        events.push(("delegation.ownerQuestion", "question"));
    }
    match execution {
        "pendingReview" => events.push(("delegation.pendingReview", "delegationReview")),
        "interrupted"
            if ask.pointer("/attempt/interruptReason").and_then(Value::as_str) == Some("outcomeUnknown") =>
        {
            let event = if owner { "delegation.outcomeUnknown.owner" } else { "delegation.outcomeUnknown.agent" };
            events.push((event, "delegationReview"));
        }
        "answered" if ask["review"].as_str() == Some("awaitingAcceptance") => {
            let event =
                if owner { "delegation.awaitingAcceptance.owner" } else { "delegation.awaitingAcceptance.agent" };
            events.push((event, "acceptance"));
        }
        _ => {}
    }
    events
}

/// The 需要你 kind and decision of an ask, or None when it does not wait on
/// the owner under `evaluator`.
pub fn ask_need(
    ask: &Value,
    evaluator: &crate::notification_levels::Evaluator,
    scope: crate::notification_levels::Scope<'_>,
) -> Option<(&'static str, crate::notification_levels::Decision)> {
    let events = ask_events(ask);
    if events.is_empty() {
        return None;
    }
    let ids: Vec<&str> = events.iter().map(|(event, _)| *event).collect();
    let decision = evaluator.decide(&ids, scope);
    let kind = events.iter().find(|(event, _)| *event == decision.event).map(|(_, kind)| *kind)?;
    decision.needs_you().then_some((kind, decision))
}

/// The 需要你 kind of an ask summary under the default tables (same buckets
/// as the desktop's `delegationBucket`).
pub fn ask_need_kind(ask: &Value) -> Option<&'static str> {
    ask_need(ask, &crate::notification_levels::Evaluator::default(), Default::default()).map(|(kind, _)| kind)
}

/// The notification event of a native server request need kind.
pub fn native_event(kind: &str) -> &'static str {
    if kind == "question" {
        "native.question"
    } else {
        "native.approval"
    }
}

/// Owner-facing sources merged into needs per agent id.
#[derive(Default)]
pub struct NeedSources {
    /// (cutexSessionId, native message, createdAt) of current-generation requests.
    pub native: Vec<(String, Value, String)>,
    /// Pending owner approvals (`ApprovalRecord` JSON).
    pub approvals: Vec<Value>,
    /// Ask summaries (`/v2/asks` rows).
    pub asks: Vec<Value>,
    /// Agent → owner messages (owner-notify records).
    pub owner_notifications: Vec<crate::owner_notify::store::OwnerNotification>,
    /// The owner's notification level tables (default: the `default` presets).
    pub evaluator: crate::notification_levels::Evaluator,
    /// cutexSessionId → project id (agents in a project; project mutes).
    /// Empty = projects unknown (no project mute applies).
    pub agent_projects: HashMap<String, String>,
    /// `agent_projects` is complete: an agent missing from it is independent.
    pub projects_known: bool,
}

impl NeedSources {
    fn scope<'a>(&'a self, agent: &'a str) -> crate::notification_levels::Scope<'a> {
        let project = if self.projects_known {
            Some(self.agent_projects.get(agent).map(String::as_str))
        } else {
            self.agent_projects.get(agent).map(|project| Some(project.as_str()))
        };
        crate::notification_levels::Scope { agent: Some(agent), project }
    }

    /// Asks that count as 需要你 (target agent's scope).
    pub fn ask_need(&self, ask: &Value) -> Option<(&'static str, crate::notification_levels::Decision)> {
        let scope = match ask["target"].as_str() {
            Some(target) => self.scope(target),
            None => Default::default(),
        };
        ask_need(ask, &self.evaluator, scope)
    }
}

pub fn needs_by_agent(sources: &NeedSources) -> BTreeMap<String, Vec<Value>> {
    let mut needs: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for (id, native, at) in &sources.native {
        if let Some((kind, title)) = native_need(native) {
            let decision = sources.evaluator.decide(&[native_event(kind)], sources.scope(id));
            if !decision.needs_you() {
                continue;
            }
            needs.entry(id.clone()).or_default().push(json!({
                "kind": kind,
                "id": format!("native:{}", native["id"]),
                "title": title,
                "at": crate::management::instance::normalize_utc(at),
                "level": decision.level,
            }));
        }
    }
    for approval in &sources.approvals {
        let Some(id) = approval["requesterCutexSessionId"].as_str() else {
            continue;
        };
        let decision = sources.evaluator.decide(&["ownerApproval.pending"], sources.scope(id));
        if !decision.needs_you() {
            continue;
        }
        needs.entry(id.to_string()).or_default().push(json!({
            "kind": "ownerApproval",
            "id": approval["approvalId"],
            "title": title(approval["summary"].as_str().unwrap_or_default()),
            "at": approval["createdAt"].as_str().map(crate::management::instance::normalize_utc),
            "level": decision.level,
        }));
    }
    let mut asks: Vec<(&Value, &'static str, crate::notification_levels::Level)> = sources
        .asks
        .iter()
        .filter_map(|ask| sources.ask_need(ask).map(|(kind, decision)| (ask, kind, decision.level)))
        .collect();
    asks.sort_by(|a, b| a.0["createdAt"].as_str().cmp(&b.0["createdAt"].as_str()));
    for (ask, kind, level) in asks {
        let Some(target) = ask["target"].as_str() else {
            continue;
        };
        needs.entry(target.to_string()).or_default().push(json!({
            "kind": kind,
            "id": ask["askId"],
            "title": title(ask["title"].as_str().unwrap_or_default()),
            "at": ask["createdAt"].as_str().map(crate::management::instance::normalize_utc),
            "level": level,
        }));
    }
    for record in sources.owner_notifications.iter().filter(|record| record.pending()) {
        needs.entry(record.agent_id.clone()).or_default().push(json!({
            "kind": if record.needs_reply { "ownerReply" } else { "ownerMessage" },
            "id": record.notification_id,
            "title": title(&record.text),
            "at": crate::management::instance::normalize_utc(&record.created_at),
            "level": record.effective_notification_level(),
        }));
    }
    for list in needs.values_mut() {
        list.sort_by(|a, b| a["at"].as_str().cmp(&b["at"].as_str()));
    }
    needs
}

// ---------------------------------------------------------------- rows

/// Per-agent facts from the session store / agent management.
#[derive(Clone, Debug, Default)]
pub struct AgentMeta {
    pub kind: &'static str,
    pub provider: &'static str,
    pub project_id: Option<String>,
}

fn later(a: Option<&str>, b: Option<&str>) -> Option<String> {
    let parse = |value: Option<&str>| {
        value.and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
    };
    match (parse(a), parse(b)) {
        (Some(x), Some(y)) => Some(if x >= y { a } else { b }.unwrap().to_string()),
        (Some(_), None) => a.map(str::to_string),
        (None, Some(_)) => b.map(str::to_string),
        (None, None) => None,
    }
}

/// One `agents[]` row from a `/v2/sessions` resource.
pub fn agent_row(
    session: &Value,
    meta: Option<&AgentMeta>,
    needs: &[Value],
    projection: Option<&AgentProjection>,
) -> Option<Value> {
    let id = session["cutexSessionId"].as_str()?;
    if session["lifecycle"].as_str() == Some("retired") {
        return None;
    }
    let runtime = &session["runtime"];
    let online = runtime["status"].as_str() == Some("online");
    let status = if !needs.is_empty() {
        "needsInput"
    } else if !online {
        "offline"
    } else if runtime["activeTurnId"].as_str().is_some() {
        "working"
    } else {
        "idle"
    };
    let preview = projection
        .and_then(|p| p.preview.clone())
        .or_else(|| {
            session
                .pointer("/activity/lastOutput/displayText")
                .and_then(Value::as_str)
                .and_then(|text| preview_text(text, PREVIEW_CHARS))
        });
    let last_activity = later(
        runtime["lastEventAt"].as_str(),
        session.pointer("/activity/lastOutputAt").and_then(Value::as_str),
    )
    .or_else(|| session["updatedAt"].as_str().map(str::to_string))
    .map(|at| crate::management::instance::normalize_utc(&at));
    Some(json!({
        "id": id,
        "name": session["displayName"].as_str().unwrap_or(id),
        "kind": meta.map(|m| m.kind).unwrap_or("agent"),
        "projectId": meta.and_then(|m| m.project_id.clone()),
        "provider": meta.map(|m| m.provider).unwrap_or("codex"),
        "status": status,
        "needs": needs,
        "preview": preview,
        "lastActivityAt": last_activity,
        "lastTurn": projection.and_then(|p| p.last_turn.clone()).map(|mut turn| {
            turn.ended_at = crate::management::instance::normalize_utc(&turn.ended_at);
            turn
        }),
        "unreadTurns": projection.map(|p| p.unread_turns).unwrap_or(0),
    }))
}

/// Rows equal apart from fields that change on every event (used for
/// diffs; compares in place instead of copying both rows).
fn same_apart_from_activity(left: &Value, right: &Value) -> bool {
    match (left.as_object(), right.as_object()) {
        // serde_json maps are ordered by key, so equal rows iterate alike.
        (Some(left), Some(right)) => {
            let kept = |(key, _): &(&String, &Value)| key.as_str() != "lastActivityAt";
            left.iter().filter(kept).eq(right.iter().filter(kept))
        }
        _ => left == right,
    }
}

/// A synthesised summary (without cursor / capabilities).
#[derive(Clone, Debug, Default)]
pub struct Synthesis {
    pub agents: Vec<Value>,
    pub projects: Vec<Value>,
    pub approvals_count: usize,
    pub asks_count: usize,
    pub needs_you_count: usize,
}

pub fn synthesize(
    sessions: &[Value],
    meta: &HashMap<String, AgentMeta>,
    projects: &BTreeMap<String, Value>,
    sources: &NeedSources,
    projections: &HashMap<String, AgentProjection>,
) -> Synthesis {
    let needs = needs_by_agent(sources);
    let mut agents = Vec::new();
    let mut listed = HashSet::new();
    for session in sessions {
        let Some(id) = session["cutexSessionId"].as_str() else {
            continue;
        };
        let agent_needs = needs.get(id).map(Vec::as_slice).unwrap_or(&[]);
        if let Some(row) = agent_row(session, meta.get(id), agent_needs, projections.get(id)) {
            listed.insert(id.to_string());
            agents.push(row);
        }
    }
    let mut used_projects = BTreeMap::new();
    for row in &agents {
        if let Some(project) = row["projectId"].as_str().and_then(|id| projects.get(id)) {
            used_projects.insert(project["id"].as_str().unwrap_or_default().to_string(), project.clone());
        }
    }
    let unlisted_needs = needs
        .iter()
        .filter(|(id, list)| !listed.contains(*id) && !list.is_empty())
        .count();
    let needs_you_count = agents
        .iter()
        .filter(|row| row["needs"].as_array().is_some_and(|n| !n.is_empty()))
        .count()
        + unlisted_needs;
    Synthesis {
        agents,
        projects: used_projects.into_values().collect(),
        approvals_count: sources.approvals.len(),
        asks_count: sources.asks.iter().filter(|ask| sources.ask_need(ask).is_some()).count(),
        needs_you_count,
    }
}

/// Summary events that turn the last emitted rows `previous` into `next`,
/// and the rows to remember as emitted. A row whose only change is
/// `lastActivityAt` is announced only when `activity_due(id)` (throttle);
/// otherwise its previously emitted row is kept so the change shows later.
pub fn diff_rows(
    previous: &BTreeMap<String, Value>,
    next: &[Value],
    activity_due: &dyn Fn(&str) -> bool,
) -> (Vec<PendingSummaryEvent>, BTreeMap<String, Value>) {
    let mut events = Vec::new();
    let mut emitted = BTreeMap::new();
    for row in next {
        let id = row["id"].as_str().unwrap_or_default().to_string();
        let changed = match previous.get(&id) {
            None => true,
            Some(old) if !same_apart_from_activity(old, row) => true,
            Some(old) if old == row => false,
            Some(_) => activity_due(&id),
        };
        if changed {
            events.push(("cutex/summary/agentChanged".to_string(), json!({ "agent": row })));
            emitted.insert(id, row.clone());
        } else {
            emitted.insert(id.clone(), previous.get(&id).cloned().unwrap_or_else(|| row.clone()));
        }
    }
    for id in previous.keys() {
        if !emitted.contains_key(id) {
            events.push(("cutex/summary/agentRemoved".to_string(), json!({ "id": id })));
        }
    }
    (events, emitted)
}

/// Activity-only changes of one agent are pushed at most this often.
pub const ACTIVITY_PUSH_INTERVAL: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------- sources (host)

fn collect_meta_and_projects() -> (HashMap<String, AgentMeta>, BTreeMap<String, Value>) {
    let mut meta = HashMap::new();
    let mut projects = BTreeMap::new();
    // Borrow the cached store: this runs on every summary tick.
    let _ = crate::session::store::with_cutex_session_store(|store| {
        for record in store.sessions.values() {
            meta.insert(
                record.cutex_session_id.clone(),
                AgentMeta {
                    kind: if record.is_owned_session() { "chat" } else { "agent" },
                    provider: if record.claude_session.is_some() { "claude" } else { "codex" },
                    project_id: None,
                },
            );
        }
    });
    if let Ok(snapshot) = crate::agent_management::AgentManagementStore::open_default()
        .and_then(|store| store.snapshot().map_err(anyhow::Error::new))
    {
        for (session_id, agent) in &snapshot.agents {
            if let Some(project_id) = crate::agent_management::current_project_id(&snapshot, agent) {
                let presentation = crate::agent_management::effective_presentation(
                    &project_id,
                    snapshot.project_presentations.get(&project_id),
                );
                projects.insert(
                    project_id.as_str().to_string(),
                    json!({
                        "id": project_id.as_str(),
                        "name": presentation.display_name,
                        "color": serde_json::to_value(presentation.color).ok(),
                    }),
                );
                if let Some(entry) = meta.get_mut(session_id.as_str()) {
                    entry.project_id = Some(project_id.as_str().to_string());
                }
            }
        }
    }
    (meta, projects)
}

/// Pending native requests the owner can still answer: the session's runtime
/// is online and the request belongs to its current runtime generation.
/// Requests of an offline / errored runtime cannot be answered (the native
/// process that asked is gone) and never count as needs.
pub fn answerable_native_requests(
    sessions: &[Value],
    pending: Vec<(String, u64, Value, String)>,
) -> Vec<(String, Value, String)> {
    let live: HashMap<&str, u64> = sessions
        .iter()
        .filter(|s| s.pointer("/runtime/status").and_then(Value::as_str) == Some("online"))
        .filter_map(|s| {
            Some((
                s["cutexSessionId"].as_str()?,
                s.pointer("/runtime/runtimeGeneration")?.as_u64()?,
            ))
        })
        .collect();
    pending
        .into_iter()
        .filter(|(id, generation, _, _)| live.get(id.as_str()) == Some(generation))
        .map(|(id, _, native, at)| (id, native, at))
        .collect()
}

fn collect_need_sources(sessions: &[Value], meta: &HashMap<String, AgentMeta>) -> NeedSources {
    let generations: HashMap<&str, u64> = sessions
        .iter()
        .filter_map(|s| {
            Some((
                s["cutexSessionId"].as_str()?,
                s.pointer("/runtime/runtimeGeneration")?.as_u64()?,
            ))
        })
        .collect();
    let _ = generations;
    let native = answerable_native_requests(sessions, super::server_requests::pending_all().unwrap_or_default());
    let approvals = crate::approvals::ApprovalStore::open_default()
        .and_then(|store| store.list(false, chrono::Utc::now()))
        .map(|records| {
            records
                .into_iter()
                .filter(|r| r.state == crate::approvals::ApprovalState::Pending)
                .filter_map(|r| serde_json::to_value(r).ok())
                .collect()
        })
        .unwrap_or_default();
    let asks = crate::asks::store::AskStore::open_default()
        .and_then(|store| store.shared())
        .map(|state| {
            state
                .asks
                .values()
                .filter(|ask| ask.closed.is_none())
                .map(|ask| crate::asks::view::summary(&state, ask))
                .collect()
        })
        .unwrap_or_default();
    NeedSources {
        native,
        approvals,
        asks,
        owner_notifications: super::owner_notify::all_records(),
        evaluator: crate::notification_levels::Evaluator::current(),
        agent_projects: meta
            .iter()
            .filter_map(|(id, meta)| Some((id.clone(), meta.project_id.clone()?)))
            .collect(),
        projects_known: !meta.is_empty(),
    }
}

fn collect(
    context: ManagementRequestContext,
    repository: &EventRepository,
    projections: &HashMap<String, AgentProjection>,
) -> anyhow::Result<Synthesis> {
    let registry = (context.load_registry)()?;
    let mut list = super::session::session_list_resource(&registry, context.load_runtime_status, repository)?;
    let sessions: Vec<Value> = match list.get_mut("sessions").map(Value::take) {
        Some(Value::Array(sessions)) => sessions,
        _ => Vec::new(),
    };
    let (meta, projects) = collect_meta_and_projects();
    let sources = collect_need_sources(&sessions, &meta);
    Ok(synthesize(&sessions, &meta, &projects, &sources, projections))
}

// ---------------------------------------------------------------- journal

#[derive(Clone, Debug)]
pub struct JournalEvent {
    pub seq: i64,
    pub name: String,
    pub data: Value,
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
             CREATE TABLE IF NOT EXISTS agents(id TEXT PRIMARY KEY, projection TEXT, row TEXT);
             CREATE TABLE IF NOT EXISTS events(seq INTEGER PRIMARY KEY AUTOINCREMENT, at INTEGER NOT NULL, name TEXT NOT NULL, data TEXT NOT NULL);",
        )?;
        let journal = Self { db };
        if journal.meta("streamId")?.is_none() {
            journal.put_meta("streamId", &uuid::Uuid::new_v4().to_string())?;
        }
        Ok(journal)
    }

    fn meta(&self, key: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .db
            .query_row("SELECT value FROM meta WHERE key=?", [key], |r| r.get(0))
            .optional()?)
    }

    fn put_meta(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.db.execute(
            "INSERT INTO meta VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn stream_id(&self) -> String {
        self.meta("streamId").ok().flatten().unwrap_or_default()
    }

    pub fn latest_seq(&self) -> anyhow::Result<Option<i64>> {
        Ok(self.db.query_row("SELECT MAX(seq) FROM events", [], |r| r.get(0))?)
    }

    pub fn earliest_seq(&self) -> anyhow::Result<Option<i64>> {
        Ok(self.db.query_row("SELECT MIN(seq) FROM events", [], |r| r.get(0))?)
    }

    /// Highest sequence ever assigned (survives pruning).
    fn assigned_seq(&self) -> anyhow::Result<i64> {
        Ok(self
            .db
            .query_row("SELECT seq FROM sqlite_sequence WHERE name='events'", [], |r| r.get(0))
            .optional()?
            .unwrap_or(0))
    }

    /// The sequence to replay after, or the 409 `cursor_expired` details.
    /// No cursor = live only. A cursor must be a sequence this journal
    /// assigned whose successors are all still retained.
    pub fn resolve_cursor(
        &self,
        after: Option<&str>,
        wanted_stream: Option<&str>,
    ) -> anyhow::Result<Result<i64, Value>> {
        let stream_id = self.stream_id();
        let latest = self.assigned_seq()?;
        let earliest = self.earliest_seq()?.unwrap_or(latest + 1);
        let Some(text) = after.map(str::trim).filter(|v| !v.is_empty()) else {
            return Ok(Ok(latest));
        };
        Ok(match text.parse::<i64>() {
            Ok(seq)
                if seq >= 0
                    && seq <= latest
                    && seq + 1 >= earliest
                    && wanted_stream.is_none_or(|s| s == stream_id) =>
            {
                Ok(seq)
            }
            _ => Err(json!({
                "streamId": stream_id,
                "earliestCursor": (earliest - 1).max(0).to_string(),
                "latestCursor": latest.to_string(),
            })),
        })
    }

    pub fn page_after(&self, after: i64, limit: usize) -> anyhow::Result<Vec<JournalEvent>> {
        let mut statement = self
            .db
            .prepare("SELECT seq, name, data FROM events WHERE seq > ? ORDER BY seq LIMIT ?")?;
        let rows = statement
            .query_map(params![after, limit as i64], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .map(|(seq, name, data)| JournalEvent {
                seq,
                name,
                data: serde_json::from_str(&data).unwrap_or(Value::Null),
            })
            .collect())
    }

    pub fn load_projections(&self) -> anyhow::Result<HashMap<String, AgentProjection>> {
        let mut statement = self.db.prepare("SELECT id, projection FROM agents WHERE projection IS NOT NULL")?;
        let rows = statement
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, text)| Some((id, serde_json::from_str(&text).ok()?)))
            .collect())
    }

    fn load_rows(&self) -> anyhow::Result<BTreeMap<String, Value>> {
        let mut statement = self.db.prepare("SELECT id, row FROM agents WHERE row IS NOT NULL")?;
        let rows = statement
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, text)| Some((id, serde_json::from_str(&text).ok()?)))
            .collect())
    }

    /// Persist the changed `projections`, the rows (when given: the
    /// persisted rows and their replacement; only the difference is
    /// written), the source checkpoint and append `events`; returns the
    /// journaled events.
    fn commit(
        &mut self,
        projections: &HashMap<String, AgentProjection>,
        rows: Option<RowsUpdate<'_>>,
        meta: &[(&str, String)],
        events: &[PendingSummaryEvent],
    ) -> anyhow::Result<Vec<JournalEvent>> {
        let now = chrono::Utc::now().timestamp();
        let tx = self.db.transaction()?;
        for (id, projection) in projections {
            tx.execute(
                "INSERT INTO agents(id, projection) VALUES(?,?) ON CONFLICT(id) DO UPDATE SET projection=excluded.projection",
                params![id, serde_json::to_string(projection)?],
            )?;
        }
        if let Some(RowsUpdate { previous, next }) = rows {
            for id in previous.keys().filter(|id| !next.contains_key(*id)) {
                tx.execute("UPDATE agents SET row=NULL WHERE id=?", [id])?;
            }
            for (id, row) in next.iter().filter(|(id, row)| previous.get(*id) != Some(*row)) {
                tx.execute(
                    "INSERT INTO agents(id, row) VALUES(?,?) ON CONFLICT(id) DO UPDATE SET row=excluded.row",
                    params![id, serde_json::to_string(row)?],
                )?;
            }
        }
        for (key, value) in meta {
            tx.execute(
                "INSERT INTO meta VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![key, value],
            )?;
        }
        let mut journaled = Vec::new();
        for (name, data) in events {
            tx.execute(
                "INSERT INTO events(at, name, data) VALUES(?,?,?)",
                params![now, name, serde_json::to_string(data)?],
            )?;
            journaled.push(JournalEvent {
                seq: tx.last_insert_rowid(),
                name: name.clone(),
                data: data.clone(),
            });
        }
        tx.execute("DELETE FROM events WHERE at < ?", [now - RETENTION_SECONDS])?;
        tx.execute(
            "DELETE FROM events WHERE seq <= (SELECT MAX(seq) FROM events) - ?",
            [MAX_JOURNAL_ROWS],
        )?;
        tx.commit()?;
        Ok(journaled)
    }
}

/// New summary rows against the rows the journal holds (`previous`).
#[derive(Clone, Copy)]
pub struct RowsUpdate<'a> {
    pub previous: &'a BTreeMap<String, Value>,
    pub next: &'a BTreeMap<String, Value>,
}

// ---------------------------------------------------------------- hub

type Subscriber = (mpsc::SyncSender<JournalEvent>, Arc<AtomicBool>);

/// Journal + live subscribers of this daemon.
pub struct Hub {
    journal: Mutex<Journal>,
    subscribers: Mutex<Vec<Subscriber>>,
    dirty: AtomicBool,
    /// Owner reads waiting for the summary thread: (cutexSessionId, at).
    reads: Mutex<Vec<(String, String)>>,
}

impl Hub {
    pub fn new(journal: Journal) -> Self {
        Self {
            journal: Mutex::new(journal),
            subscribers: Mutex::new(Vec::new()),
            dirty: AtomicBool::new(true),
            reads: Mutex::new(Vec::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Journal> {
        self.journal.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn publish(&self, events: &[JournalEvent]) {
        let mut subscribers = self
            .subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for event in events {
            subscribers.retain(|(sender, lagged)| match sender.try_send(event.clone()) {
                Ok(()) => true,
                Err(mpsc::TrySendError::Full(_)) => {
                    lagged.store(true, Ordering::SeqCst);
                    false
                }
                Err(mpsc::TrySendError::Disconnected(_)) => false,
            });
        }
    }

    /// Queue an owner read; applied by the summary thread on its next tick.
    pub fn queue_read(&self, cutex_session_id: String, at: String) {
        self.reads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((cutex_session_id, at));
        self.dirty.store(true, Ordering::SeqCst);
    }

    fn take_reads(&self) -> Vec<(String, String)> {
        std::mem::take(&mut *self.reads.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    /// Journal events and publish them (single writer: the caller holds no lock).
    pub fn append(
        &self,
        projections: &HashMap<String, AgentProjection>,
        rows: Option<RowsUpdate<'_>>,
        meta: &[(&str, String)],
        events: &[PendingSummaryEvent],
    ) -> anyhow::Result<Vec<JournalEvent>> {
        let mut journal = self.lock();
        let journaled = journal.commit(projections, rows, meta, events)?;
        // Publish while holding the journal lock so a subscriber registered
        // under the same lock sees each event exactly once.
        self.publish(&journaled);
        Ok(journaled)
    }
}

static HUB: OnceLock<Arc<Hub>> = OnceLock::new();

fn hub() -> Option<&'static Arc<Hub>> {
    HUB.get()
}

/// Owner read the session (`/v2/notifications/ack` on its thread). Only
/// queued: the summary thread is the single writer of projections, so a
/// read cannot be overwritten by a tick that loaded them earlier.
pub fn mark_read_thread(thread_id: &str) {
    let Some(hub) = hub() else { return };
    let Ok(store) = crate::session::store::load_cutex_session_store_shared() else {
        return;
    };
    let Some(id) = store
        .sessions
        .values()
        .find(|r| r.app_server_thread_id() == Some(thread_id))
        .map(|r| r.cutex_session_id.clone())
    else {
        return;
    };
    hub.queue_read(
        id,
        crate::management::instance::now_utc(),
    );
}

/// `POST /v2/summary/read {agentId}` (capability `summary-read`): the owner
/// read this agent. Clears `unreadTurns` (applied by the summary thread
/// within a tick, then announced as `agentChanged`) and acknowledges the
/// session's notification reminder whatever its id.
pub fn handle_read(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let bad = |stream: &mut TcpStream, message: &str| {
        super::server::write_v2_error(stream, 400, "Bad Request", "invalid_request", message, false, json!({}))
    };
    let body: Value = match serde_json::from_slice(&request.body) {
        Ok(body) => body,
        Err(_) => return bad(stream, "JSON body {agentId} required"),
    };
    let Some(agent_id) = body["agentId"].as_str().filter(|id| !id.is_empty() && id.len() <= 200) else {
        return bad(stream, "agentId (a cutexSessionId) required");
    };
    if let Some(key) = body.as_object().and_then(|o| o.keys().find(|k| *k != "agentId")) {
        return bad(stream, &format!("unsupported field: {key}"));
    }
    let store = crate::session::store::load_cutex_session_store_shared()?;
    let Some(record) = store.sessions.get(agent_id) else {
        return super::server::write_v2_error(
            stream,
            404,
            "Not Found",
            "session_not_found",
            "no cutex session has this id",
            false,
            json!({ "agentId": agent_id }),
        );
    };
    let at = crate::management::instance::now_utc();
    let summary = match hub() {
        Some(hub) => {
            hub.queue_read(agent_id.to_string(), at.clone());
            true
        }
        None => false,
    };
    let notification = match record.app_server_thread_id() {
        Some(thread) => crate::notify::state::mark_thread_read(thread).unwrap_or_else(|error| {
            eprintln!("summary read: notification state: {error:#}");
            false
        }),
        None => false,
    };
    // Owner-notify messages of this agent count as seen (alerts close).
    let owner_messages = super::owner_notify::mark_agent_read(agent_id);
    write_json_response(
        stream,
        200,
        "OK",
        &json!({
            "agentId": agent_id,
            "readAt": at,
            "summaryQueued": summary,
            "notificationAcknowledged": notification,
            "ownerMessagesAcknowledged": owner_messages,
        }),
    )
}

/// Start the summary thread (daemon only; once).
pub fn start(context: ManagementRequestContext) -> anyhow::Result<()> {
    let path = crate::config::paths::config_dir()?.join("summary").join("summary.sqlite3");
    let journal = Journal::open(&path)?;
    let hub = Arc::new(Hub::new(journal));
    if HUB.set(hub.clone()).is_err() {
        return Ok(());
    }
    std::thread::Builder::new()
        .name("cutex-summary".into())
        .spawn(move || {
            let mut state = TickState::default();
            let mut last_error: Option<String> = None;
            loop {
                match tick(&hub, context, &mut state) {
                    Ok(()) => last_error = None,
                    Err(error) => {
                        let text = format!("{error:#}");
                        if last_error.as_deref() != Some(text.as_str()) {
                            eprintln!("host summary warning: {text}");
                            last_error = Some(text);
                        }
                    }
                }
                std::thread::sleep(TICK);
            }
        })?;
    Ok(())
}

/// In-memory state of the summary thread between ticks.
#[derive(Default)]
struct TickState {
    last_full: Option<Instant>,
    /// Last `agentChanged` per agent (activity-only throttle).
    last_pushed: HashMap<String, Instant>,
    /// What the journal holds after the last successful commit (the summary
    /// thread is its only writer). `None` = read it from the journal: at
    /// start and after a failed tick.
    persisted: Option<Persisted>,
}

/// The summary thread's view of the journal's agents table and meta.
struct Persisted {
    projections: HashMap<String, AgentProjection>,
    rows: BTreeMap<String, Value>,
    checkpoint: Option<(String, String)>,
    initialized: bool,
    approvals_count: Option<usize>,
    asks_count: Option<usize>,
}

impl Persisted {
    fn load(journal: &Journal) -> anyhow::Result<Self> {
        let count = |key: &str| -> anyhow::Result<Option<usize>> {
            Ok(journal.meta(key)?.and_then(|v| v.parse::<usize>().ok()))
        };
        Ok(Self {
            projections: journal.load_projections()?,
            rows: journal.load_rows()?,
            checkpoint: journal
                .meta("sourceCheckpoint")?
                .and_then(|text| serde_json::from_str::<(String, String)>(&text).ok()),
            initialized: journal.meta("initialized")?.is_some(),
            approvals_count: count("approvalsCount")?,
            asks_count: count("asksCount")?,
        })
    }
}

#[cfg(test)]
static JOURNAL_STATE_LOADS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn tick(hub: &Hub, context: ManagementRequestContext, state: &mut TickState) -> anyhow::Result<()> {
    let last_full = &mut state.last_full;
    let repository = super::repository::management_v2_repository()?;
    // Reuse the state of the last commit instead of re-reading (and
    // re-parsing) every projection and row each second. Taken out, so an
    // error below leaves it unset and the next tick reloads.
    let persisted = match state.persisted.take() {
        Some(persisted) => persisted,
        None => {
            #[cfg(test)]
            JOURNAL_STATE_LOADS.fetch_add(1, Ordering::Relaxed);
            Persisted::load(&hub.lock())?
        }
    };
    let Persisted {
        projections: persisted_projections,
        rows: previous_rows,
        checkpoint,
        initialized,
        approvals_count: counts_approvals,
        asks_count: counts_asks,
    } = persisted;
    let mut projections = persisted_projections.clone();
    let reads = hub.take_reads();
    for (id, at) in &reads {
        projections.entry(id.clone()).or_default().mark_read(at);
    }
    let mut events: Vec<PendingSummaryEvent> = Vec::new();
    // First run: replay the native log and establish rows without announcing.
    let silent = !initialized;
    let mut position = checkpoint.clone();
    let mut consumed = 0usize;
    let mut resync = false;
    for _ in 0..20 {
        let query = ReplayQuery {
            stream_id: position.as_ref().map(|(stream, _)| stream.clone()),
            after: position.as_ref().map(|(_, cursor)| cursor.clone()),
            limit: PAGE_LIMIT,
            cutex_session_id: None,
        };
        let page = match repository.page(query) {
            Ok(page) => page,
            Err(ReplayError::CursorExpired { .. } | ReplayError::StreamChanged { .. }) => {
                // The native log moved on: restart from its beginning, silently.
                position = None;
                resync = true;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        for event in &page.events {
            if let Some(summary_event) = reduce(&mut projections, event) {
                if !silent && !resync {
                    events.push(summary_event);
                }
            }
            position = Some((event.stream_id.clone(), event.cursor.clone()));
        }
        consumed += page.events.len();
        if position.is_none() {
            position = page.checkpoint.cursor.clone().map(|cursor| (page.stream_id.clone(), cursor));
        }
        if !page.has_more {
            break;
        }
    }
    if silent || resync {
        // A replayed log says nothing about what the owner has read.
        projections.values_mut().for_each(|p| p.unread_turns = 0);
    }
    let full_due = last_full.is_none_or(|at| at.elapsed() >= FULL_REFRESH)
        || consumed > 0
        || hub.dirty.swap(false, Ordering::SeqCst);
    let mut rows_to_store = None;
    let mut counts = (counts_approvals, counts_asks);
    let mut meta: Vec<(&str, String)> = Vec::new();
    if let Some(position) = &position {
        meta.push(("sourceCheckpoint", serde_json::to_string(position)?));
    }
    if full_due {
        *last_full = Some(Instant::now());
        let synthesis = collect(context, repository, &projections)?;
        let now = Instant::now();
        let last_pushed = &state.last_pushed;
        let activity_due = |id: &str| {
            last_pushed
                .get(id)
                .is_none_or(|at| now.duration_since(*at) >= ACTIVITY_PUSH_INTERVAL)
        };
        let (changes, emitted) = diff_rows(&previous_rows, &synthesis.agents, &activity_due);
        if resync {
            events.push(("cutex/summary/resync".to_string(), json!({ "reason": "sourceReset" })));
        } else {
            for (_, data) in &changes {
                if let Some(id) = data.pointer("/agent/id").and_then(Value::as_str) {
                    state.last_pushed.insert(id.to_string(), now);
                }
            }
            events.extend(changes);
        }
        state.last_pushed.retain(|id, _| emitted.contains_key(id));
        if counts_approvals != Some(synthesis.approvals_count) {
            events.push((
                "cutex/summary/approvalsChanged".to_string(),
                json!({ "count": synthesis.approvals_count }),
            ));
        }
        if counts_asks != Some(synthesis.asks_count) {
            events.push((
                "cutex/summary/asksChanged".to_string(),
                json!({ "count": synthesis.asks_count }),
            ));
        }
        meta.push(("approvalsCount", synthesis.approvals_count.to_string()));
        meta.push(("asksCount", synthesis.asks_count.to_string()));
        counts = (Some(synthesis.approvals_count), Some(synthesis.asks_count));
        rows_to_store = Some(emitted);
    }
    if silent {
        events.clear();
        meta.push(("initialized", chrono::Utc::now().to_rfc3339()));
    }
    let rows_changed = rows_to_store.as_ref().is_some_and(|rows| *rows != previous_rows);
    // The journal keeps its checkpoint when no position was found.
    let next_checkpoint = position.clone().or(checkpoint.clone());
    if !silent && consumed == 0 && reads.is_empty() && !rows_changed && events.is_empty() && position == checkpoint {
        // Nothing is written: keep exactly what the journal holds.
        state.persisted = Some(Persisted {
            projections: persisted_projections,
            rows: previous_rows,
            checkpoint: next_checkpoint,
            initialized,
            approvals_count: counts.0,
            asks_count: counts.1,
        });
        return Ok(());
    }
    if !rows_changed {
        rows_to_store = None;
    }
    // Only projections that differ from the journal's are written.
    let changed_projections: HashMap<String, AgentProjection> = projections
        .iter()
        .filter(|(id, projection)| persisted_projections.get(*id) != Some(*projection))
        .map(|(id, projection)| (id.clone(), projection.clone()))
        .collect();
    hub.append(
        &changed_projections,
        rows_to_store.as_ref().map(|next| RowsUpdate {
            previous: &previous_rows,
            next,
        }),
        &meta,
        &events,
    )?;
    state.persisted = Some(Persisted {
        projections,
        rows: rows_to_store.unwrap_or(previous_rows),
        checkpoint: next_checkpoint,
        initialized: initialized || silent,
        approvals_count: counts.0,
        asks_count: counts.1,
    });
    Ok(())
}

// ---------------------------------------------------------------- HTTP

/// `GET /v2/summary`.
pub fn handle_summary(
    stream: &mut TcpStream,
    context: ManagementRequestContext,
    repository: &EventRepository,
) -> anyhow::Result<()> {
    let value = summary_resource(context, repository)?;
    write_json_response(stream, 200, "OK", &value)
}

/// The `GET /v2/summary` body.
pub(super) fn summary_resource(
    context: ManagementRequestContext,
    repository: &EventRepository,
) -> anyhow::Result<Value> {
    let (projections, stream_id, latest) = match hub() {
        Some(hub) => {
            let journal = hub.lock();
            (
                journal.load_projections().unwrap_or_default(),
                journal.stream_id(),
                journal.assigned_seq().ok().filter(|seq| *seq > 0),
            )
        }
        None => (HashMap::new(), String::new(), None),
    };
    let synthesis = collect(context, repository, &projections)?;
    let host = crate::platform::host::current_host_name();
    let capabilities = super::capabilities::capabilities_resource()["featureList"].clone();
    Ok(json!({
            "hostId": host,
            "hostName": host,
            "instanceId": crate::management::instance::instance_id(),
            "capabilities": capabilities,
            "cursor": { "streamId": stream_id, "after": latest.unwrap_or(0).to_string() },
            "generatedAt": crate::management::instance::now_utc(),
            "projects": synthesis.projects,
            "agents": synthesis.agents,
            "needsYouCount": synthesis.needs_you_count,
        }))
}

fn write_summary_event(stream: &mut impl Write, event: &JournalEvent) -> anyhow::Result<()> {
    write!(
        stream,
        "id: {}\nevent: {}\ndata: {}\n\n",
        event.seq,
        event.name,
        serde_json::to_string(&event.data)?
    )?;
    stream.flush()?;
    Ok(())
}

/// `GET /v2/summary/events`.
pub fn handle_events(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    handle_events_filtered(stream, request, &mut |_| true)
}

/// `handle_events` writing only the journal events `visible` accepts (a
/// non-owner user's scope, P1 users).
pub(super) fn handle_events_filtered(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    visible: &mut dyn FnMut(&JournalEvent) -> bool,
) -> anyhow::Result<()> {
    let Some(hub) = hub() else {
        return super::server::write_v2_error(
            stream,
            503,
            "Service Unavailable",
            "summary_unavailable",
            "the host summary is not running in this process",
            true,
            json!({}),
        );
    };
    let after_text = crate::http::query::query_value(&request.path, "after")
        .or_else(|| request.headers.get("last-event-id").cloned());
    let wanted_stream = crate::http::query::query_value(&request.path, "streamId");
    let (receiver, lagged, backlog) = {
        let journal = hub.lock();
        let after = match journal.resolve_cursor(after_text.as_deref(), wanted_stream.as_deref())? {
            Ok(after) => after,
            Err(details) => {
                return super::server::write_v2_error(
                    stream,
                    409,
                    "Conflict",
                    "cursor_expired",
                    "the summary cursor is outside the retained journal; re-read /v2/summary",
                    false,
                    details,
                )
            }
        };
        let (sender, receiver) = mpsc::sync_channel(SUBSCRIBER_CAPACITY);
        let lagged = Arc::new(AtomicBool::new(false));
        hub.subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((sender, lagged.clone()));
        let backlog = journal.page_after(after, 10_000)?;
        (receiver, lagged, backlog)
    };
    stream.write_all(crate::http::server::sse_response_head().as_bytes())?;
    stream.flush()?;
    stream_summary_filtered(
        stream,
        backlog,
        receiver,
        lagged,
        crate::management::listeners::RevocationWatch::for_current_request(),
        visible,
    )
}

/// Body of the summary stream after the headers (testable without a socket).
pub fn stream_summary(
    stream: &mut impl Write,
    backlog: Vec<JournalEvent>,
    receiver: mpsc::Receiver<JournalEvent>,
    lagged: Arc<AtomicBool>,
    watch: crate::management::listeners::RevocationWatch,
) -> anyhow::Result<()> {
    stream_summary_filtered(stream, backlog, receiver, lagged, watch, &mut |_| true)
}

pub fn stream_summary_filtered(
    stream: &mut impl Write,
    backlog: Vec<JournalEvent>,
    receiver: mpsc::Receiver<JournalEvent>,
    lagged: Arc<AtomicBool>,
    mut watch: crate::management::listeners::RevocationWatch,
    visible: &mut dyn FnMut(&JournalEvent) -> bool,
) -> anyhow::Result<()> {
    let mut last = None;
    for event in &backlog {
        if visible(event) {
            write_summary_event(stream, event)?;
        }
        last = Some(event.seq);
    }
    let mut last_write = Instant::now();
    loop {
        if let Some(token_id) = watch.revoked() {
            crate::management::listeners::write_revoked_frame(stream, &token_id)?;
            return Ok(());
        }
        match receiver.recv_timeout(crate::management::listeners::STREAM_TICK) {
            Ok(event) => {
                if last.is_some_and(|seq| event.seq <= seq) {
                    continue;
                }
                if !visible(&event) {
                    last = Some(event.seq);
                    continue;
                }
                write_summary_event(stream, &event)?;
                last = Some(event.seq);
                last_write = Instant::now();
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
                    super::server::write_sse_lagged(stream, last.map(|seq| seq.to_string()).as_deref())?;
                }
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
#[path = "summary_tests.rs"]
mod tests;
