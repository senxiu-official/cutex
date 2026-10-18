//! Owner approvals for agent actions (root credential only).
//!
//! - `GET  /v2/approvals?state=pending|all` (default `pending`) →
//!   `{hostId, approvals: [ApprovalRecord]}`, newest first
//! - `POST /v2/approvals/{id}/approve` body `{note?, client?}` → `{approval}`
//! - `POST /v2/approvals/{id}/deny` body `{note?, client?}` → `{approval}`;
//!   404 `approval_not_found`, 409 `approval_not_pending` (`details.state`)
//! - `client` (`approval-decided-by`): what the deciding client says about
//!   itself (`app`, `version`, `device`, `surface`, `input`); stored with the
//!   connection kind as the record's `decidedBy` and logged as one
//!   `cutex-approval-decided` line, so a decision can be traced afterwards.
//!
//! New requests and decisions are published as `cutex/approvals/changed`
//! `{reason: requested|approved|denied|consumed|expired, approval}` on the
//! host event stream (`cutexSessionId` = [`super::services::HOST_EVENT_SESSION_ID`]).
//! The agent retries its call after a decision; see `crate::approvals`.
//!
//! Most approvals are requested in the Agent Bus process (services, agent
//! profiles), whose event appends never reach Management's live
//! subscribers. Management serve therefore watches the store
//! ([`start_watcher`], a metadata check every 2 s, capability
//! `approvals-live`) and publishes every change it did not publish itself.
//!
//! A tool call may hold for the decision (approval-wait,
//! `crate::approvals::gate_with_wait`): the watcher then also reports
//! `waiting` / `wait_ended` for a pending record, Management ends holds of a
//! turn that completed ([`on_turn_completed`]) and a decision made while a
//! hold is active gets no notice (the holding call returns it).
//!
//! After the owner approves or denies a request the agent has to retry
//! itself ([`crate::approvals::decision_needs_retry`]), the requester gets a
//! short cutex message through the alarm scheduler (`after_turn`, busy-aware;
//! capability `approval-decision-notice`).

use std::collections::BTreeMap;
use std::net::TcpStream;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{json, Value};

use crate::approvals::{ApprovalRecord, ApprovalState, ApprovalStore, DecideError};
use crate::http::server::{write_json_response, SimpleHttpRequest};

const WATCH_INTERVAL: Duration = Duration::from_secs(2);
/// Re-read the store this often even when its metadata looks unchanged.
const FULL_READ_INTERVAL: Duration = Duration::from_secs(60);

pub const PATH: &str = "/v2/approvals";
pub const APPROVALS_CHANGED_EVENT: &str = "cutex/approvals/changed";
const MAX_NOTE_CHARS: usize = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Approve,
    Deny,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    Collection,
    Decide(String, Decision),
}

pub fn parse_path(path: &str) -> Option<Route> {
    if path == PATH {
        return Some(Route::Collection);
    }
    let rest = path.strip_prefix("/v2/approvals/")?;
    let (id, action) = rest.split_once('/')?;
    let valid_id = id.starts_with("apr-")
        && id.len() <= 64
        && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
    if !valid_id {
        return None;
    }
    let decision = match action {
        "approve" => Decision::Approve,
        "deny" => Decision::Deny,
        _ => return None,
    };
    Some(Route::Decide(id.to_string(), decision))
}

pub fn is_approvals_path(path: &str) -> bool {
    path == PATH || path.starts_with("/v2/approvals/")
}

/// Publish an approval change for clients. Best effort: the store is the
/// source of truth and `GET /v2/approvals` always reflects it. In
/// Management serve the watcher then skips this state (no second event).
pub fn publish_changed(record: &ApprovalRecord, reason: &str) {
    if let Ok(mut guard) = WATCH.lock() {
        if let Some(watch) = guard.as_mut() {
            watch.known.insert(record.approval_id.clone(), Seen::of(record));
        }
    }
    append_changed(record, reason);
}

fn append_changed(record: &ApprovalRecord, reason: &str) {
    let Ok(repository) = super::repository::management_v2_repository() else {
        return;
    };
    if let Err(error) = super::server::append_cutex_event(
        repository,
        super::services::HOST_EVENT_SESSION_ID,
        super::model::EventCorrelation::default(),
        APPROVALS_CHANGED_EVENT,
        json!({ "reason": reason, "approval": record }),
    ) {
        eprintln!("approvals: could not publish event: {error:#}");
    }
}

#[derive(Default)]
struct Watch {
    primed: bool,
    stamp: Option<(SystemTime, u64)>,
    last_read: Option<Instant>,
    /// approvalId → the state (and hold) clients were last told about.
    known: BTreeMap<String, Seen>,
    /// Decided during a hold that never ended (the Agent Bus went away
    /// mid-hold): the decision notice Management skipped, queued by [`poll`].
    orphans: Vec<ApprovalRecord>,
    /// Orphaned holds already handed to [`poll`] (by hold id).
    orphans_seen: std::collections::BTreeSet<String>,
}

/// `None` outside Management serve (the Agent Bus, the CLI, tests): then
/// [`publish_changed`] only appends.
static WATCH: Mutex<Option<Watch>> = Mutex::new(None);

/// What clients know of one record: its state and its hold (approval-wait:
/// the hold's id and end, so a hold starting or ending is published too).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    state: ApprovalState,
    wait: Option<(String, Option<String>)>,
}

impl Seen {
    fn of(record: &ApprovalRecord) -> Self {
        Self {
            state: record.state,
            wait: record.wait.as_ref().map(|wait| (wait.id.clone(), wait.end.clone())),
        }
    }
}

fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    std::fs::metadata(path)
        .ok()
        .map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()))
}

fn reason_of(state: ApprovalState) -> &'static str {
    match state {
        ApprovalState::Pending => "requested",
        ApprovalState::Approved => "approved",
        ApprovalState::Denied => "denied",
        ApprovalState::Consumed => "consumed",
        ApprovalState::Expired => "expired",
    }
}

/// One watcher step: re-read the store when it changed (or every minute)
/// and return the records whose state clients have not seen, with the
/// event reason. The first read only primes. Records pruned from the store
/// are forgotten silently (they were decided or expired long ago).
/// A hold this far past its end without ending counts as orphaned.
const ORPHAN_GRACE_SECS: i64 = 30;

fn step(watch: &mut Watch, path: &Path, force: bool) -> anyhow::Result<Vec<(ApprovalRecord, &'static str)>> {
    let now_stamp = stamp(path);
    let stale = watch.last_read.is_none_or(|t| t.elapsed() >= FULL_READ_INTERVAL);
    if watch.primed && !force && !stale && now_stamp == watch.stamp {
        return Ok(Vec::new());
    }
    let rows = crate::approvals::read_all_at(path)?;
    watch.stamp = now_stamp;
    watch.last_read = Some(Instant::now());
    let mut changes = Vec::new();
    if watch.primed {
        for record in &rows {
            let seen = Seen::of(record);
            let known = watch.known.get(&record.approval_id);
            if known == Some(&seen) {
                continue;
            }
            // Same state, another hold: `waiting` / `wait_ended` (pending
            // records only; a decided record's hold ending says nothing new).
            let reason = match known {
                Some(known) if known.state == seen.state => match &record.wait {
                    _ if record.state != ApprovalState::Pending => continue,
                    Some(wait) if wait.end.is_none() => "waiting",
                    _ => "wait_ended",
                },
                _ => reason_of(record.state),
            };
            changes.push((record.clone(), reason));
        }
    }
    // A decision made while a hold was active got no notice; if the hold
    // then never ended (the Agent Bus restarted mid-hold) the requester would
    // never hear of it. Hand those to the notice once the hold is overdue.
    let overdue = chrono::Utc::now() - chrono::Duration::seconds(ORPHAN_GRACE_SECS);
    for record in &rows {
        let Some(wait) = record.wait.as_ref() else { continue };
        let decided = matches!(record.state, ApprovalState::Approved | ApprovalState::Denied);
        let past = crate::approvals::timestamp(&wait.until).is_some_and(|until| until < overdue);
        if decided && wait.end.is_none() && past && watch.orphans_seen.insert(wait.id.clone()) {
            watch.orphans.push(record.clone());
        }
    }
    watch.known = rows.iter().map(|r| (r.approval_id.clone(), Seen::of(r))).collect();
    watch.primed = true;
    Ok(changes)
}

fn poll(force: bool) -> anyhow::Result<()> {
    let path = crate::approvals::store_path()?;
    let (changes, orphans) = {
        let mut guard = WATCH.lock().unwrap_or_else(|e| e.into_inner());
        let watch = guard.get_or_insert_with(Watch::default);
        let changes = step(watch, &path, force)?;
        (changes, std::mem::take(&mut watch.orphans))
    };
    for (record, reason) in changes {
        append_changed(&record, reason);
    }
    for record in orphans {
        queue_decision_notice(&record);
    }
    Ok(())
}

/// Management serve: watch the approval store for `cutex/approvals/changed`
/// (requests of the Agent Bus process, consumption by a retried call).
pub fn start_watcher() -> anyhow::Result<()> {
    // Prime now, so a change right after start is reported.
    if let Err(error) = poll(false) {
        eprintln!("approvals watcher: {error:#}");
    }
    std::thread::Builder::new()
        .name("cutex-approvals-watch".into())
        .spawn(|| {
            let mut failing = false;
            loop {
                std::thread::sleep(WATCH_INTERVAL);
                match poll(false) {
                    Ok(()) => failing = false,
                    Err(error) => {
                        if !failing {
                            eprintln!("approvals watcher: {error:#}");
                        }
                        failing = true;
                    }
                }
            }
        })?;
    Ok(())
}

/// Management, `turn/completed` of a session's thread: holds of that turn
/// end (approval-wait), so a later approval is not used by a call whose
/// turn the agent no longer sees (Codex does not cancel MCP calls on an
/// interrupt). The watcher publishes the change. Best effort.
pub fn on_turn_completed(cutex_session_id: &str, params: &Value) {
    let thread = params.get("threadId").and_then(Value::as_str);
    let turn = params.pointer("/turn/id").and_then(Value::as_str);
    let result = ApprovalStore::open_default().and_then(|store| {
        store.end_waits_for_turn(cutex_session_id, thread, turn, chrono::Utc::now())
    });
    if let Err(error) = result {
        eprintln!("approvals: could not end holds of {cutex_session_id}: {error:#}");
    }
}

/// The message the requester gets after the owner's decision (None when
/// the agent does not retry itself or the requester is not a local agent).
pub fn decision_notice(record: &ApprovalRecord) -> Option<String> {
    if !crate::approvals::decision_needs_retry(record.kind)
        || !crate::agent_bus::routing::is_full_durable_cutex_session_id(&record.requester_cutex_session_id)
    {
        return None;
    }
    let summary: String = record.summary.chars().take(300).collect();
    match record.state {
        ApprovalState::Approved => Some(format!(
            "[cutex approvals] The owner approved {} ({summary}). Retry the same tool call now with the same arguments{}; it proceeds once. The approval lapses after 24 h.",
            record.approval_id,
            if record.kind == crate::approvals::ApprovalKind::AgentProfile { " and the same action_id" } else { "" },
        )),
        ApprovalState::Denied => Some(format!(
            "[cutex approvals] The owner denied {} ({summary}).{} Do not retry the same call; choose another way or ask the owner.",
            record.approval_id,
            record
                .note
                .as_deref()
                .map(|note| format!(" Owner's note: {}", note.chars().take(500).collect::<String>()))
                .unwrap_or_default(),
        )),
        _ => None,
    }
}

/// Queue [`decision_notice`] for the requester. Best effort: the decision
/// stands and `GET /v2/approvals` shows it even if the notice fails. Also
/// called by the Agent Bus when a holding call could not deliver a decision
/// made during its hold (approval-wait). Notice ids are per approval and
/// decision, so a second queue of the same notice is a no-op.
pub fn queue_decision_notice(record: &ApprovalRecord) {
    let Some(message) = decision_notice(record) else {
        return;
    };
    let notice_id = format!("{}:{}", record.approval_id, reason_of(record.state));
    if let Err(error) = crate::agent_bus::alarms::set_system_notice(
        crate::agent_bus::alarms::APPROVAL_NOTICE_OWNER,
        &notice_id,
        &record.requester_cutex_session_id,
        &message,
    ) {
        eprintln!("approvals: decision notice for {} not queued: {error:#}", record.approval_id);
    }
}

/// Client facts a decision may carry (`approval-decided-by`): `{app, version,
/// device, surface, input}`, each a short single-line string.
const CLIENT_FIELDS: &[&str] = &["app", "version", "device", "surface", "input"];
const MAX_CLIENT_FIELD_CHARS: usize = 80;

/// Decision body `{note?, client?}`.
#[derive(Debug, Default, PartialEq, Eq)]
struct DecisionBody {
    note: Option<String>,
    client: BTreeMap<String, String>,
}

fn parse_decision_body(body: &[u8]) -> Result<DecisionBody, String> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(DecisionBody::default());
    }
    let value: Value =
        serde_json::from_slice(body).map_err(|error| format!("JSON object body required: {error}"))?;
    let object = value.as_object().ok_or("JSON object body required")?;
    if let Some(key) = object.keys().find(|key| *key != "note" && *key != "client") {
        return Err(format!("unsupported field: {key}"));
    }
    let note = match object.get("note") {
        None | Some(Value::Null) => None,
        Some(Value::String(note)) => {
            let note = note.trim();
            if note.chars().count() > MAX_NOTE_CHARS
                || note.chars().any(|c| c.is_control() && c != '\n')
            {
                return Err(format!("note: at most {MAX_NOTE_CHARS} characters"));
            }
            (!note.is_empty()).then(|| note.to_string())
        }
        Some(_) => return Err("note: must be a string or null".into()),
    };
    let mut client = BTreeMap::new();
    match object.get("client") {
        None | Some(Value::Null) => {}
        Some(Value::Object(fields)) => {
            for (key, value) in fields {
                if !CLIENT_FIELDS.contains(&key.as_str()) {
                    return Err(format!("client: unsupported field {key}"));
                }
                let Value::String(text) = value else {
                    return Err(format!("client.{key}: must be a string"));
                };
                let text = text.trim();
                if text.chars().count() > MAX_CLIENT_FIELD_CHARS || text.chars().any(char::is_control) {
                    return Err(format!("client.{key}: one line, at most {MAX_CLIENT_FIELD_CHARS} characters"));
                }
                if !text.is_empty() {
                    client.insert(key.clone(), text.to_string());
                }
            }
        }
        Some(_) => return Err("client: must be an object or null".into()),
    }
    Ok(DecisionBody { note, client })
}

/// The decider of a request on this connection: the owner (root credential,
/// checked by the server before [`handle`]) plus how it connected.
fn decider(client: BTreeMap<String, String>) -> crate::approvals::ApprovalDecider {
    let connection = crate::management::listeners::current_connection();
    let remote = connection.as_ref().is_some_and(|info| info.remote);
    crate::approvals::ApprovalDecider {
        by: "owner".into(),
        connection: connection.as_ref().map(|_| if remote { "remote" } else { "loopback" }.to_string()),
        peer: connection.filter(|info| info.remote).map(|info| info.peer.ip().to_string()),
        client,
    }
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let store = match ApprovalStore::open_default() {
        Ok(store) => store,
        Err(error) => return unavailable(stream, &error),
    };
    let now = chrono::Utc::now();
    match (request.method.as_str(), parse_path(path)) {
        ("GET", Some(Route::Collection)) => {
            let include_decided = match crate::http::query::query_value(&request.path, "state")
                .as_deref()
            {
                None | Some("pending") => false,
                Some("all") => true,
                Some(_) => {
                    return super::server::write_v2_error(
                        stream,
                        400,
                        "Bad Request",
                        "invalid_request",
                        "state must be pending or all",
                        false,
                        json!({}),
                    )
                }
            };
            match store.list(include_decided, now) {
                Ok(approvals) => write_json_response(
                    stream,
                    200,
                    "OK",
                    &json!({
                        "hostId": crate::platform::host::current_host_name(),
                        "approvals": approvals,
                    }),
                ),
                Err(error) => unavailable(stream, &error),
            }
        }
        ("POST", Some(Route::Decide(id, decision))) => {
            let body = match parse_decision_body(&request.body) {
                Ok(body) => body,
                Err(message) => {
                    return super::server::write_v2_error(
                        stream,
                        400,
                        "Bad Request",
                        "invalid_request",
                        &message,
                        false,
                        json!({}),
                    )
                }
            };
            let decided_by = decider(body.client);
            eprintln!(
                "cutex-approval-decided {}",
                json!({ "approvalId": id, "decision": if decision == Decision::Approve { "approve" } else { "deny" }, "decidedBy": decided_by })
            );
            match store.decide_as(&id, decision == Decision::Approve, body.note, Some(decided_by), now) {
                Ok(record) => {
                    publish_changed(
                        &record,
                        if decision == Decision::Approve { "approved" } else { "denied" },
                    );
                    // A call holding for this decision gets it as its tool
                    // result (approval-wait): no notice. If the hold ends
                    // without delivering it, the Agent Bus queues the notice.
                    if !record.wait.as_ref().is_some_and(|wait| wait.active(now)) {
                        queue_decision_notice(&record);
                    }
                    write_json_response(stream, 200, "OK", &json!({ "approval": record }))
                }
                Err(DecideError::NotFound) => super::server::write_v2_error(
                    stream,
                    404,
                    "Not Found",
                    "approval_not_found",
                    "no approval has this id",
                    false,
                    json!({ "approvalId": id }),
                ),
                Err(DecideError::NotPending(state)) => super::server::write_v2_error(
                    stream,
                    409,
                    "Conflict",
                    "approval_not_pending",
                    "the approval was already decided or expired",
                    false,
                    json!({ "approvalId": id, "state": state }),
                ),
                Err(DecideError::Store(error)) => super::server::write_v2_error(
                    stream,
                    503,
                    "Service Unavailable",
                    "approvals_unavailable",
                    "the approval store could not be updated",
                    true,
                    json!({ "diagnostic": error }),
                ),
            }
        }
        _ => super::server::write_v2_error(
            stream,
            405,
            "Method Not Allowed",
            "method_not_allowed",
            "GET /v2/approvals or POST /v2/approvals/{id}/approve|deny",
            false,
            json!({}),
        ),
    }
}

fn unavailable(stream: &mut TcpStream, error: &anyhow::Error) -> anyhow::Result<()> {
    super::server::write_v2_error(
        stream,
        503,
        "Service Unavailable",
        "approvals_unavailable",
        "the approval store could not be read",
        true,
        json!({ "diagnostic": format!("{error:#}") }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_parse_exact_ids_and_actions() {
        assert_eq!(parse_path("/v2/approvals"), Some(Route::Collection));
        assert_eq!(
            parse_path("/v2/approvals/apr-0123abcd/approve"),
            Some(Route::Decide("apr-0123abcd".into(), Decision::Approve))
        );
        assert_eq!(
            parse_path("/v2/approvals/apr-0123abcd/deny"),
            Some(Route::Decide("apr-0123abcd".into(), Decision::Deny))
        );
        for bad in [
            "/v2/approvals/",
            "/v2/approvals/apr-1",
            "/v2/approvals/x-1/approve",
            "/v2/approvals/apr-1/cancel",
            "/v2/approvals/apr-1/approve/extra",
            "/v2/approvals/apr-../approve",
        ] {
            assert_eq!(parse_path(bad), None, "{bad}");
        }
        assert!(is_approvals_path("/v2/approvals/apr-1/approve"));
    }

    fn record(id: &str, kind: crate::approvals::ApprovalKind, state: ApprovalState) -> ApprovalRecord {
        ApprovalRecord {
            approval_id: id.into(),
            kind,
            reason: "outside_caller_envelope".into(),
            requester_cutex_session_id: "cutex.00000000-0000-0000-0000-000000000001".into(),
            requester_name: None,
            project_id: None,
            summary: "Keep service web persistently".into(),
            details: json!({}),
            request_sha256: "d".into(),
            state,
            created_at: chrono::Utc::now().to_rfc3339(),
            decided_at: None,
            note: None,
            decided_by: None,
            consumed_at: None,
            wait: None,
        }
    }

    fn write(path: &Path, records: &[ApprovalRecord]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec(&json!({ "approvals": records })).unwrap()).unwrap();
    }

    #[test]
    fn watcher_primes_then_reports_unseen_states_once() {
        use crate::approvals::ApprovalKind::*;
        let dir = std::env::temp_dir().join(format!("approvals-watch-{}", uuid::Uuid::new_v4()));
        let path = dir.join("approvals.json");
        let mut watch = Watch::default();
        write(&path, &[record("apr-old", ManagedService, ApprovalState::Pending)]);
        assert!(step(&mut watch, &path, false).unwrap().is_empty(), "first read primes");
        write(&path, &[record("apr-old", ManagedService, ApprovalState::Pending), record("apr-new", ServiceLifetime, ApprovalState::Pending)]);
        let changes = step(&mut watch, &path, true).unwrap();
        assert_eq!(changes.iter().map(|(r, why)| (r.approval_id.as_str(), *why)).collect::<Vec<_>>(), [("apr-new", "requested")]);
        assert!(step(&mut watch, &path, true).unwrap().is_empty(), "reported once");
        // A decision published in this process is not reported again.
        watch.known.insert("apr-new".into(), Seen { state: ApprovalState::Approved, wait: None });
        write(&path, &[record("apr-old", ManagedService, ApprovalState::Consumed), record("apr-new", ServiceLifetime, ApprovalState::Approved)]);
        let changes = step(&mut watch, &path, true).unwrap();
        assert_eq!(changes.iter().map(|(r, why)| (r.approval_id.as_str(), *why)).collect::<Vec<_>>(), [("apr-old", "consumed")]);
        write(&path, &[]);
        assert!(step(&mut watch, &path, true).unwrap().is_empty(), "pruned records are forgotten");
        assert!(watch.known.is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn watcher_reports_holds_starting_and_ending() {
        use crate::approvals::ApprovalKind::*;
        let dir = std::env::temp_dir().join(format!("approvals-watch-wait-{}", uuid::Uuid::new_v4()));
        let path = dir.join("approvals.json");
        let mut watch = Watch::default();
        let mut row = record("apr-w", ManagedService, ApprovalState::Pending);
        write(&path, std::slice::from_ref(&row));
        step(&mut watch, &path, false).unwrap();
        let wait = crate::approvals::ApprovalWait {
            id: "w1".into(),
            started_at: chrono::Utc::now().to_rfc3339(),
            until: (chrono::Utc::now() + chrono::Duration::seconds(120)).to_rfc3339(),
            thread_id: Some("t".into()),
            turn_id: None,
            ended_at: None,
            end: None,
        };
        row.wait = Some(wait.clone());
        write(&path, std::slice::from_ref(&row));
        let reasons = |changes: Vec<(ApprovalRecord, &'static str)>| changes.into_iter().map(|(_, why)| why).collect::<Vec<_>>();
        assert_eq!(reasons(step(&mut watch, &path, true).unwrap()), ["waiting"]);
        row.wait = Some(crate::approvals::ApprovalWait { end: Some("timeout".into()), ..wait });
        write(&path, std::slice::from_ref(&row));
        assert_eq!(reasons(step(&mut watch, &path, true).unwrap()), ["wait_ended"]);
        // Consumed with its hold ending in the same write: one consumed event.
        row.state = ApprovalState::Consumed;
        row.wait.as_mut().unwrap().end = Some("approved".into());
        write(&path, std::slice::from_ref(&row));
        assert_eq!(reasons(step(&mut watch, &path, true).unwrap()), ["consumed"]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn watcher_hands_over_decisions_of_orphaned_holds_once() {
        use crate::approvals::ApprovalKind::*;
        let dir = std::env::temp_dir().join(format!("approvals-watch-orphan-{}", uuid::Uuid::new_v4()));
        let path = dir.join("approvals.json");
        let mut watch = Watch::default();
        let hold = |until: chrono::DateTime<chrono::Utc>| crate::approvals::ApprovalWait {
            id: "w1".into(),
            started_at: (until - chrono::Duration::seconds(120)).to_rfc3339(),
            until: until.to_rfc3339(),
            thread_id: None,
            turn_id: None,
            ended_at: None,
            end: None,
        };
        // Approved during a hold still running: the holding call delivers it.
        let mut row = record("apr-o", ManagedService, ApprovalState::Approved);
        row.wait = Some(hold(chrono::Utc::now() + chrono::Duration::seconds(60)));
        write(&path, std::slice::from_ref(&row));
        step(&mut watch, &path, false).unwrap();
        assert!(watch.orphans.is_empty());
        // The hold is long overdue and never ended: the Bus went away.
        row.wait = Some(hold(chrono::Utc::now() - chrono::Duration::seconds(90)));
        write(&path, std::slice::from_ref(&row));
        step(&mut watch, &path, true).unwrap();
        assert_eq!(std::mem::take(&mut watch.orphans).len(), 1);
        step(&mut watch, &path, true).unwrap();
        assert!(watch.orphans.is_empty(), "handed over once");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn decision_notice_only_for_retrying_local_requesters() {
        use crate::approvals::ApprovalKind::*;
        let approved = decision_notice(&record("apr-1", ServiceLifetime, ApprovalState::Approved)).unwrap();
        assert!(approved.contains("apr-1") && approved.contains("Retry the same tool call now") && approved.contains("Keep service web"), "{approved}");
        assert!(!approved.contains("action_id"));
        assert!(decision_notice(&record("apr-2", AgentProfile, ApprovalState::Approved)).unwrap().contains("same action_id"));
        let mut denied = record("apr-3", ManagedService, ApprovalState::Denied);
        denied.note = Some("use port 3000".into());
        let text = decision_notice(&denied).unwrap();
        assert!(text.contains("denied apr-3") && text.contains("use port 3000") && text.contains("Do not retry"), "{text}");
        assert!(decision_notice(&record("apr-9", StaticRoot, ApprovalState::Approved)).unwrap().contains("Retry the same tool call now"));
        assert!(decision_notice(&record("apr-4", AskDelivery, ApprovalState::Approved)).is_none(), "the scheduler acts itself");
        assert!(decision_notice(&record("apr-5", WorkflowRun, ApprovalState::Approved)).is_none());
        assert!(decision_notice(&record("apr-6", BrowserClaim, ApprovalState::Approved)).is_none());
        assert!(decision_notice(&record("apr-7", BrowserOpen, ApprovalState::Pending)).is_none());
        let mut peer = record("apr-8", ManagedService, ApprovalState::Approved);
        peer.requester_cutex_session_id = "peer:ci-0123:cutex.x".into();
        assert!(decision_notice(&peer).is_none());
    }

    #[test]
    fn notes_are_optional_and_bounded() {
        let note = |body: &[u8]| parse_decision_body(body).map(|body| body.note);
        assert_eq!(note(b""), Ok(None));
        assert_eq!(note(br#"{"note":" ok "}"#), Ok(Some("ok".into())));
        assert_eq!(note(br#"{"note":null}"#), Ok(None));
        assert!(note(br#"{"reason":"x"}"#).is_err());
        assert!(note(br#"{"note":1}"#).is_err());
        let long = format!(r#"{{"note":"{}"}}"#, "x".repeat(MAX_NOTE_CHARS + 1));
        assert!(note(long.as_bytes()).is_err());
    }

    #[test]
    fn decision_body_client_facts_are_bounded_and_recorded() {
        let body = parse_decision_body(
            br#"{"client":{"app":"cutex-desktop","device":"laptop-01","surface":"inline","input":"keyboard","version":" 0.1.3 "}}"#,
        )
        .unwrap();
        assert_eq!(body.note, None);
        assert_eq!(body.client.get("version").map(String::as_str), Some("0.1.3"));
        assert_eq!(body.client.len(), 5);
        assert!(parse_decision_body(br#"{"client":{"token":"x"}}"#).is_err());
        assert!(parse_decision_body(br#"{"client":{"app":1}}"#).is_err());
        assert!(parse_decision_body(br#"{"client":"desktop"}"#).is_err());
        assert!(parse_decision_body(br#"{"client":{"app":"a\nb"}}"#).is_err());
        let long = format!(r#"{{"client":{{"device":"{}"}}}}"#, "x".repeat(MAX_CLIENT_FIELD_CHARS + 1));
        assert!(parse_decision_body(long.as_bytes()).is_err());
        assert_eq!(parse_decision_body(br#"{"client":null}"#).unwrap(), DecisionBody::default());

        // Outside a listener (no connection): owner, connection unknown.
        let local = decider(body.client.clone());
        assert_eq!(local.by, "owner");
        assert_eq!(local.connection, None);
        crate::management::listeners::set_current_connection(Some(crate::management::listeners::ConnectionInfo {
            remote: true,
            peer: "100.64.0.9:5000".parse().unwrap(),
        }));
        let remote = decider(BTreeMap::new());
        crate::management::listeners::set_current_connection(None);
        assert_eq!((remote.connection.as_deref(), remote.peer.as_deref()), (Some("remote"), Some("100.64.0.9")));
        let json = serde_json::to_value(&remote).unwrap();
        assert_eq!(json, json!({ "by": "owner", "connection": "remote", "peer": "100.64.0.9" }));
    }

    #[test]
    fn decide_as_records_the_decider_and_old_records_still_read() {
        let dir = std::env::temp_dir().join(format!("approvals-decider-{}", uuid::Uuid::new_v4()));
        let path = dir.join("approvals.json");
        // A record written before approval-decided-by (no decidedBy) reads and decides.
        let old = record("apr-d", crate::approvals::ApprovalKind::ServiceLifetime, ApprovalState::Pending);
        let mut raw = serde_json::to_value(&old).unwrap();
        assert!(raw.get("decidedBy").is_none());
        raw.as_object_mut().unwrap().remove("decidedBy");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, serde_json::to_vec(&json!({ "approvals": [raw] })).unwrap()).unwrap();
        let store = ApprovalStore::at(&path);
        let mut client = BTreeMap::new();
        client.insert("surface".to_string(), "inline".to_string());
        let decided = store
            .decide_as("apr-d", false, None, Some(decider(client)), chrono::Utc::now())
            .unwrap();
        assert_eq!(decided.state, ApprovalState::Denied);
        let stored = crate::approvals::read_all_at(&path).unwrap();
        let by = stored[0].decided_by.as_ref().unwrap();
        assert_eq!((by.by.as_str(), by.client.get("surface").map(String::as_str)), ("owner", Some("inline")));
        std::fs::remove_dir_all(dir).ok();
    }
}
