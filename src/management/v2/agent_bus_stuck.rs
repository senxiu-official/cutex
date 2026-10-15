//! Owner view and discard of stuck Agent Bus messages (capability
//! `agent-bus-stuck-messages`; root credential, device / client tokens
//! included; docs/agent-bus-operations.md):
//!
//! - `GET /v2/agent-bus/stuck-messages?minAgeSecs=&agentId=&limit=` →
//!   `{messages, liveQueue: {available, error?}, minAgeSecs, generatedAt}`.
//!   Durable records still `pending` / `stale_held` at least `minAgeSecs`
//!   (default 3600) old, plus messages that sit in the bus's in-memory
//!   runtime queues that long without being ACKed (`durable: false` when
//!   the repository has no record of them). Oldest first; each with its
//!   reasons, age, recipient / sender names and live-queue placement.
//! - `POST /v2/agent-bus/stuck-messages/discard {messageId, reason,
//!   expectedSemanticSha256?, allowProtected?}` → the durable record becomes
//!   terminal (`discarded` before native submission, else `quarantined`;
//!   error code `owner_message_discarded`; event
//!   `cutex/agentBus/messageFailed` for agent-sourced messages), then the
//!   message is withdrawn from every live runtime queue, and an
//!   `owner_discarded` audit fact is appended to the Agent Bus audit log.
//!   Idempotent (`changed: false` the second time; the live withdrawal is
//!   retried every time). Task / Job / control messages need
//!   `allowProtected: true`.

use std::net::TcpStream;

use serde_json::{json, Value};

use super::agent_bus_state::{
    AgentBusMessageRepository, OwnerDiscardError, StuckAgentBusMessage,
};
use crate::agent_bus::queue::QueuedAgentBusMessage;
use crate::http::server::{write_json_response, SimpleHttpRequest};

pub const LIST_PATH: &str = "/v2/agent-bus/stuck-messages";
pub const DISCARD_PATH: &str = "/v2/agent-bus/stuck-messages/discard";
pub const DEFAULT_MIN_AGE_SECS: u64 = 3600;
const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 1000;

pub fn is_path(path: &str) -> bool {
    matches!(path.split('?').next(), Some(LIST_PATH) | Some(DISCARD_PATH))
}

/// The live bus as this module needs it (tests use a fake).
pub trait LiveBus {
    fn queued(&self) -> anyhow::Result<Vec<QueuedAgentBusMessage>>;
    fn withdraw(&self, message_ids: &[String], reason: &str) -> anyhow::Result<Vec<(String, String)>>;
}

impl LiveBus for crate::agent_bus::client::AgentBusHttpClient {
    fn queued(&self) -> anyhow::Result<Vec<QueuedAgentBusMessage>> {
        self.queued_messages()
    }

    fn withdraw(&self, message_ids: &[String], reason: &str) -> anyhow::Result<Vec<(String, String)>> {
        self.withdraw_messages(message_ids, reason)
    }
}

/// Display name of a durable session (`None` for unknown ids).
pub type NameOf<'a> = &'a dyn Fn(&str) -> Option<String>;

fn party(id: Option<&str>, names: NameOf<'_>) -> Value {
    match id {
        None => Value::Null,
        Some(id) => json!({"id": id, "name": names(id)}),
    }
}

/// `GET /v2/agent-bus/stuck-messages` body.
pub fn list_view(
    repository: &AgentBusMessageRepository,
    live: &dyn LiveBus,
    min_age_secs: u64,
    agent_id: Option<&str>,
    limit: usize,
    now_epoch_secs: u64,
    names: NameOf<'_>,
) -> anyhow::Result<Value> {
    let durable = repository.stuck_messages(min_age_secs, now_epoch_secs)?;
    let (queued, live_status) = match live.queued() {
        Ok(queued) => (queued, json!({"available": true})),
        Err(error) => (Vec::new(), json!({"available": false, "error": format!("{error:#}")})),
    };
    let live_available = live_status["available"] == true;
    let mut placements: std::collections::BTreeMap<&str, Vec<&QueuedAgentBusMessage>> =
        Default::default();
    for entry in &queued {
        placements.entry(entry.message_id.as_str()).or_default().push(entry);
    }
    let mut rows: Vec<(Option<u64>, String, Value)> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for message in &durable {
        seen.insert(message.message_id.clone());
        let placed = placements.get(message.message_id.as_str()).cloned().unwrap_or_default();
        rows.push((
            message.created_at_epoch_secs,
            message.message_id.clone(),
            durable_row(message, &placed, live_available, names),
        ));
    }
    for (message_id, placed) in &placements {
        if seen.contains(*message_id) {
            continue;
        }
        let first = placed[0];
        let age = now_epoch_secs.checked_sub(first.created_at_epoch_secs);
        if first.created_at_epoch_secs > 0 && age.unwrap_or(0) < min_age_secs {
            continue;
        }
        let record = repository.stuck_view_by_id(message_id, now_epoch_secs)?;
        rows.push((
            Some(first.created_at_epoch_secs),
            message_id.to_string(),
            live_row(first, placed, record.as_ref(), age, names),
        ));
    }
    if let Some(agent_id) = agent_id {
        rows.retain(|(_, _, row)| {
            row.pointer("/to/id").and_then(Value::as_str) == Some(agent_id)
                || row.pointer("/from/id").and_then(Value::as_str) == Some(agent_id)
        });
    }
    rows.sort_by(|a, b| (a.0.unwrap_or(0), &a.1).cmp(&(b.0.unwrap_or(0), &b.1)));
    let total = rows.len();
    let messages = rows
        .into_iter()
        .take(limit)
        .map(|(_, _, row)| row)
        .collect::<Vec<_>>();
    Ok(json!({
        "messages": messages,
        "total": total,
        "liveQueue": live_status,
        "minAgeSecs": min_age_secs,
        "generatedAt": chrono::Utc::now().to_rfc3339(),
    }))
}

/// Same rule as the repository: Task / Job messages and any control message.
fn live_protected(entry: &QueuedAgentBusMessage) -> bool {
    entry.control_type.is_some() || matches!(entry.sender_kind.as_str(), "task_service" | "job_service")
}

fn live_placement(placed: &[&QueuedAgentBusMessage]) -> Value {
    json!(placed
        .iter()
        .map(|entry| json!({"agentId": entry.agent_id, "agentName": entry.agent_name}))
        .collect::<Vec<_>>())
}

fn durable_row(
    message: &StuckAgentBusMessage,
    placed: &[&QueuedAgentBusMessage],
    live_available: bool,
    names: NameOf<'_>,
) -> Value {
    let mut row = serde_json::to_value(message).unwrap_or_else(|_| json!({}));
    let mut reasons = message.reasons.clone();
    if live_available {
        if placed.is_empty() {
            reasons.push(
                "not in any live runtime queue: the recipient runtime is not registered; it is redriven when the recipient registers again"
                    .into(),
            );
        } else {
            reasons.push(
                "queued for a registered runtime that keeps polling it without ACK".into(),
            );
        }
    }
    if let Some(object) = row.as_object_mut() {
        object.insert("durable".into(), json!(true));
        object.insert("to".into(), party(Some(&message.to_cutex_session_id), names));
        object.insert("from".into(), party(message.from_cutex_session_id.as_deref(), names));
        object.insert("liveQueues".into(), live_placement(placed));
        object.insert("reasons".into(), json!(reasons));
    }
    row
}

fn live_row(
    first: &QueuedAgentBusMessage,
    placed: &[&QueuedAgentBusMessage],
    record: Option<&StuckAgentBusMessage>,
    age: Option<u64>,
    names: NameOf<'_>,
) -> Value {
    let mut reasons = vec!["queued for a registered runtime that keeps polling it without ACK".to_string()];
    match record {
        None => reasons.push(
            "no durable record (in-memory only): discard withdraws it from the queue; its producer may queue it again"
                .into(),
        ),
        Some(record) => reasons.push(format!(
            "durable record is already {}: the runtime's bridge should ACK it on its next poll",
            record.state
        )),
    }
    let protected = live_protected(first);
    if protected {
        reasons.push("Task / Job / control message: check its assignment or job before discarding".into());
    }
    json!({
        "messageId": first.message_id,
        "durable": record.is_some(),
        "state": record.map(|r| r.state.as_str()).unwrap_or("live_only"),
        "to": party(first.to_cutex_session_id.as_deref(), names),
        "from": record
            .and_then(|r| r.from_cutex_session_id.as_deref())
            .map(|id| party(Some(id), names))
            .unwrap_or_else(|| json!({"id": first.sender, "name": Value::Null})),
        "toCutexSessionId": first.to_cutex_session_id,
        "createdAtEpochSecs": first.created_at_epoch_secs,
        "ageSecs": age,
        "senderKind": first.sender_kind,
        "controlType": first.control_type,
        "protectedControl": protected,
        "semanticSha256": record.and_then(|r| r.semantic_sha256.clone()),
        "error": record.and_then(|r| r.error.clone()),
        "liveQueues": live_placement(placed),
        "reasons": reasons,
        "contentPreview": first.content_preview,
    })
}

/// A refused discard: HTTP status, code, message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscardRefusal {
    pub status: u16,
    pub code: String,
    pub message: String,
}

impl DiscardRefusal {
    fn new(status: u16, code: &str, message: impl Into<String>) -> Self {
        Self { status, code: code.into(), message: message.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscardRequest {
    pub message_id: String,
    pub reason: String,
    pub expected_semantic_sha256: Option<String>,
    pub allow_protected: bool,
}

impl DiscardRequest {
    pub fn parse(body: &[u8]) -> Result<Self, DiscardRefusal> {
        let bad = |message: &str| DiscardRefusal::new(400, "invalid_request", message);
        let value: Value = serde_json::from_slice(body).map_err(|_| bad("body must be a JSON object"))?;
        let object = value.as_object().ok_or_else(|| bad("body must be a JSON object"))?;
        const ALLOWED: &[&str] = &["messageId", "reason", "expectedSemanticSha256", "allowProtected"];
        if let Some(key) = object.keys().find(|key| !ALLOWED.contains(&key.as_str())) {
            return Err(bad(&format!("unsupported field: {key}")));
        }
        let message_id = object
            .get("messageId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.len() <= 512)
            .ok_or_else(|| bad("messageId is required"))?
            .to_string();
        let reason = object
            .get("reason")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|reason| !reason.is_empty() && reason.len() <= 1024)
            .ok_or_else(|| bad("reason is required (1-1024 bytes)"))?
            .to_string();
        let expected_semantic_sha256 = match object.get("expectedSemanticSha256") {
            None | Some(Value::Null) => None,
            Some(Value::String(sha))
                if sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()) =>
            {
                Some(sha.to_ascii_lowercase())
            }
            Some(_) => return Err(bad("expectedSemanticSha256 must be 64 hex characters")),
        };
        let allow_protected = match object.get("allowProtected") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(value)) => *value,
            Some(_) => return Err(bad("allowProtected must be a boolean")),
        };
        Ok(Self { message_id, reason, expected_semantic_sha256, allow_protected })
    }
}

fn refusal_of(error: &OwnerDiscardError) -> DiscardRefusal {
    let status = match error {
        OwnerDiscardError::NotFound => 404,
        OwnerDiscardError::InvalidReason => 400,
        OwnerDiscardError::DigestMismatch
        | OwnerDiscardError::ProtectedControl
        | OwnerDiscardError::NotDiscardable { .. } => 409,
    };
    DiscardRefusal::new(status, error.code(), error.message())
}

/// `POST /v2/agent-bus/stuck-messages/discard` body. `audit` receives the
/// `owner_discarded` fact.
pub fn discard(
    repository: &AgentBusMessageRepository,
    live: &dyn LiveBus,
    request: &DiscardRequest,
    now: chrono::DateTime<chrono::Utc>,
    audit: &dyn Fn(Value),
) -> anyhow::Result<Result<Value, DiscardRefusal>> {
    let durable = repository.owner_discard(
        &request.message_id,
        request.expected_semantic_sha256.as_deref(),
        &request.reason,
        request.allow_protected,
        now,
    )?;
    let (durable_view, changed_durable) = match durable {
        Ok(outcome) => (
            Some(json!({
                "changed": outcome.changed,
                "previousState": outcome.previous_state,
                "state": outcome.snapshot.state,
                "toCutexSessionId": outcome.owner_cutex_session_id,
            })),
            outcome.changed,
        ),
        Err(OwnerDiscardError::NotFound) => {
            // In-memory only: allowed when it really is queued (and, for
            // protected messages, explicitly allowed).
            let queued = match live.queued() {
                Ok(queued) => queued,
                Err(error) => {
                    return Ok(Err(DiscardRefusal::new(
                        503,
                        "agent_bus_unavailable",
                        format!("no durable record and the live bus cannot be read: {error:#}"),
                    )))
                }
            };
            let Some(entry) = queued.iter().find(|entry| entry.message_id == request.message_id) else {
                return Ok(Err(refusal_of(&OwnerDiscardError::NotFound)));
            };
            let protected = live_protected(entry);
            if protected && !request.allow_protected {
                return Ok(Err(refusal_of(&OwnerDiscardError::ProtectedControl)));
            }
            (None, false)
        }
        Err(error) => return Ok(Err(refusal_of(&error))),
    };
    let ids = std::slice::from_ref(&request.message_id);
    let (withdrawn, live_error) = match live.withdraw(ids, &request.reason) {
        Ok(withdrawn) => (withdrawn, None),
        Err(error) => (Vec::new(), Some(format!("{error:#}"))),
    };
    if durable_view.is_none() && withdrawn.is_empty() && live_error.is_none() {
        // Raced with an ACK: nothing is left anywhere.
        return Ok(Err(refusal_of(&OwnerDiscardError::NotFound)));
    }
    let withdrawn_view = withdrawn
        .iter()
        .map(|(agent_id, _)| json!({"agentId": agent_id}))
        .collect::<Vec<_>>();
    let changed = changed_durable || !withdrawn.is_empty();
    audit(json!({
        "event": "owner_discarded",
        "timestamp": now.to_rfc3339(),
        "message_id": request.message_id,
        "reason": crate::agent_bus::audit::content_preview(&request.reason, 500),
        "durable": durable_view,
        "changed": changed,
        "withdrawn_from": withdrawn.iter().map(|(agent_id, _)| agent_id).collect::<Vec<_>>(),
        "live_error": live_error,
        "allow_protected": request.allow_protected,
    }));
    let mut result = json!({
        "messageId": request.message_id,
        "discarded": true,
        "changed": changed,
        "durable": durable_view,
        "liveWithdrawn": withdrawn_view,
    });
    if let Some(error) = live_error {
        // The record is terminal: the bridge ACKs it on its next poll and
        // no redrive brings it back; retrying the discard retries this.
        result["liveWithdrawError"] = json!(error);
    }
    Ok(Ok(result))
}

fn session_names() -> impl Fn(&str) -> Option<String> {
    let sessions = crate::session::store::load_cutex_session_store_shared().ok();
    move |id: &str| {
        super::agent_bus_state::AgentParty::of_session(sessions.as_deref(), id).name
    }
}

/// HTTP entry (root already checked by the caller).
pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let error = |stream: &mut TcpStream, status: u16, code: &str, message: &str| {
        let reason = match status {
            400 => "Bad Request",
            404 => "Not Found",
            405 => "Method Not Allowed",
            409 => "Conflict",
            _ => "Service Unavailable",
        };
        super::server::write_v2_error(stream, status, reason, code, message, status >= 500, json!({}))
    };
    let repository = match super::agent_bus_state::agent_bus_message_repository() {
        Ok(repository) => repository,
        Err(e) => return error(stream, 503, "agent_bus_state_unavailable", &format!("{e:#}")),
    };
    let config = crate::config::store::load_codez_config();
    let bus = crate::agent_bus::client::AgentBusHttpClient::from_config(&config);
    match (request.method.as_str(), path) {
        ("GET", LIST_PATH) => {
            let query = |name: &str| crate::http::query::query_value(&request.path, name);
            let min_age = match query("minAgeSecs") {
                None => DEFAULT_MIN_AGE_SECS,
                Some(text) => match text.parse::<u64>() {
                    Ok(secs) => secs,
                    Err(_) => return error(stream, 400, "invalid_request", "minAgeSecs: seconds (0 or more)"),
                },
            };
            let limit = match query("limit") {
                None => DEFAULT_LIMIT,
                Some(text) => match text.parse::<usize>() {
                    Ok(limit) if (1..=MAX_LIMIT).contains(&limit) => limit,
                    _ => return error(stream, 400, "invalid_request", "limit: 1-1000"),
                },
            };
            let agent = query("agentId").filter(|id| !id.is_empty());
            let names = session_names();
            match list_view(
                repository,
                &bus,
                min_age,
                agent.as_deref(),
                limit,
                crate::platform::now_epoch_secs(),
                &names,
            ) {
                Ok(view) => write_json_response(stream, 200, "OK", &view),
                Err(e) => error(stream, 503, "agent_bus_state_unavailable", &format!("{e:#}")),
            }
        }
        ("POST", DISCARD_PATH) => {
            let parsed = match DiscardRequest::parse(&request.body) {
                Ok(parsed) => parsed,
                Err(refusal) => return error(stream, refusal.status, &refusal.code, &refusal.message),
            };
            let audit = |fact: Value| {
                if let Err(e) = crate::agent_bus::audit::append_agent_bus_audit_record(fact) {
                    eprintln!("warning: could not write the owner discard audit fact: {e:#}");
                }
            };
            match discard(repository, &bus, &parsed, chrono::Utc::now(), &audit) {
                Ok(Ok(result)) => write_json_response(stream, 200, "OK", &result),
                Ok(Err(refusal)) => error(stream, refusal.status, &refusal.code, &refusal.message),
                Err(e) => error(stream, 503, "agent_bus_state_unavailable", &format!("{e:#}")),
            }
        }
        (_, LIST_PATH) => error(stream, 405, "method_not_allowed", "use GET"),
        _ => error(stream, 405, "method_not_allowed", "use POST"),
    }
}
