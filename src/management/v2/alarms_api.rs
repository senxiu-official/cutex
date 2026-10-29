//! Agent alarms (`alarm_set` reminders) for the owner (capability
//! `alarms-api`, root incl. client / device tokens).
//!
//! - `GET /v2/alarms?sessionId=&state=pending|all&limit=` →
//!   `{hostId, alarms: [AlarmView], nextAt}`. `sessionId` keeps the alarms
//!   that agent set or that will wake it (each row then carries `relation`:
//!   `outgoing` | `incoming` | `self`). `state=pending` (default) = only
//!   `scheduled` alarms; `all` adds `queued` (handed to the Agent Bus) and
//!   `cancelled` ones. Order: scheduled by `at` ascending, then the rest by
//!   `at` descending; `limit` 1..1000 (default 200) after ordering. `nextAt`
//!   = the earliest scheduled `at` of the filtered set (before `limit`).
//! - `GET /v2/alarms/{id}` → `{alarm}` with the full `message`.
//! - `DELETE /v2/alarms/{id}` → `{alarm}` (the owner cancels any agent's
//!   alarm; cancelling a cancelled one returns it again); 404
//!   `alarm_not_found`, 409 `alarm_already_queued` (the message was already
//!   handed to the Agent Bus and cannot be retracted).
//!
//! `{id}` is the alarm's unique message id (`alarm-<uuid>`); `alarmId` is the
//! creator's own name for it and only unique per creator.
//!
//! Alarms are set by agents (MCP `alarm_set`, Agent Bus / facade processes),
//! the CLI, and sent by the Agent Bus scheduler, so Management watches the
//! store (`runtime/alarms/alarms.json`, a metadata check every 2 s) and is
//! the only source of `cutex/alarms/changed` `{reason, id, alarm,
//! ownerCutexSessionId, targetCutexSessionId, hostId}` on the host event
//! stream (`cutexSessionId` = [`super::services::HOST_EVENT_SESSION_ID`]).
//! `reason`: `created` | `queued` | `cancelled` | `retrying` (a failed send
//! attempt, still scheduled) | `scheduled` | `removed` (`alarm` null).

use std::collections::BTreeMap;
use std::net::TcpStream;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{json, Value};

use crate::agent_bus::alarms::{self, Alarm, OwnerCancelError};
use crate::http::server::{write_json_response, SimpleHttpRequest};

pub const PATH: &str = "/v2/alarms";
pub const ALARMS_CHANGED_EVENT: &str = "cutex/alarms/changed";
const PREVIEW_CHARS: usize = 200;
const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 1000;
const WATCH_INTERVAL: Duration = Duration::from_secs(2);
/// Re-read the store this often even when its metadata looks unchanged.
const FULL_READ_INTERVAL: Duration = Duration::from_secs(60);

pub const EXPORTED_SCHEMA: super::schema_export::ExportedSchema = super::schema_export::ExportedSchema {
    name: "alarms",
    title: "Agent alarms",
    routes: &["GET /v2/alarms", "GET /v2/alarms/{id}", "DELETE /v2/alarms/{id}", "host event cutex/alarms/changed"],
    text: include_str!("schema/export/alarms.json"),
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    Collection,
    Item(String),
}

fn valid_id(id: &str) -> bool {
    id.starts_with("alarm-") && id.len() <= 100 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

pub fn parse_path(path: &str) -> Option<Route> {
    if path == PATH {
        return Some(Route::Collection);
    }
    let id = path.strip_prefix("/v2/alarms/")?;
    valid_id(id).then(|| Route::Item(id.to_string()))
}

pub fn is_path(path: &str) -> bool {
    path == PATH || path.starts_with("/v2/alarms/")
}

// ---------------------------------------------------------------------------
// Views

fn human_owner(owner: &str) -> bool {
    !crate::agent_bus::routing::is_full_durable_cutex_session_id(owner)
}

/// Agent display names from the session store (empty when unreadable).
fn session_names() -> BTreeMap<String, String> {
    crate::session::store::load_cutex_session_store_shared()
        .map(|store| {
            store
                .sessions
                .values()
                .map(|r| (r.cutex_session_id.clone(), crate::session::metadata::cutex_session_display_name(r)))
                .collect()
        })
        .unwrap_or_default()
}

pub fn alarm_view(alarm: &Alarm, names: &BTreeMap<String, String>, session: Option<&str>) -> Value {
    let chars = alarm.message.chars().count();
    let preview: String = alarm.message.chars().take(PREVIEW_CHARS).collect();
    let human = human_owner(&alarm.owner);
    let mut view = json!({
        "id": alarm.message_id,
        "alarmId": alarm.alarm_id,
        "ownerKind": if human { "human" } else { "agent" },
        "ownerCutexSessionId": if human { Value::Null } else { json!(alarm.owner) },
        "ownerName": if human { Value::Null } else { json!(names.get(&alarm.owner)) },
        "targetCutexSessionId": alarm.to,
        "targetName": if crate::agent_bus::alarms::is_director_alarm(alarm) {json!("Director")}else{json!(names.get(&alarm.to))},
        "at": alarm.at,
        "delaySeconds": alarm.delay_seconds,
        "state": alarm.state,
        "attempts": alarm.attempts,
        "lastAttemptAt": alarm.last_attempt,
        "lastError": alarm.last_error,
        "messagePreview": preview,
        "messageChars": chars,
        "messageTruncated": chars > PREVIEW_CHARS,
    });
    if let Some(role)=crate::agent_bus::alarms::director_alarm_route(alarm){
        view["targetRef"]=json!({"kind":"projectDirector","projectId":role.project_id});
        if let Some(target)=role.target.or_else(||crate::director_address::authority_for_project(&role.project_id).ok().map(|(_,target,_)|target)){
            view["targetCutexSessionId"]=json!(target);
        }
    }
    if let Some(session) = session {
        let relation = match (alarm.owner == session, alarm.to == session) {
            (true, true) => "self",
            (true, false) => "outgoing",
            _ => "incoming",
        };
        view["relation"] = json!(relation);
    }
    view
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateFilter {
    Pending,
    All,
}

pub struct ListQuery {
    pub session: Option<String>,
    pub state: StateFilter,
    pub limit: usize,
}

pub fn parse_query(path: &str) -> Result<ListQuery, String> {
    let q = |key| crate::http::query::query_value(path, key).filter(|v| !v.is_empty());
    let session = match q("sessionId") {
        None => None,
        Some(id) if id.len() <= 200 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_') => Some(id),
        Some(_) => return Err("sessionId: a cutex session id".into()),
    };
    let state = match q("state").as_deref() {
        None | Some("pending") => StateFilter::Pending,
        Some("all") => StateFilter::All,
        Some(_) => return Err("state must be pending or all".into()),
    };
    let limit = match q("limit") {
        None => DEFAULT_LIMIT,
        Some(v) => match v.parse::<usize>() {
            Ok(n) if (1..=MAX_LIMIT).contains(&n) => n,
            _ => return Err(format!("limit must be 1..{MAX_LIMIT}")),
        },
    };
    Ok(ListQuery { session, state, limit })
}

/// The list resource for `query` over `rows` (pure; tests).
pub fn list_resource(mut rows: Vec<Alarm>, query: &ListQuery, names: &BTreeMap<String, String>, host_id: &str) -> Value {
    rows.retain(|a| {
        query.session.as_deref().is_none_or(|s| a.owner == s || a.to == s)
            && (query.state == StateFilter::All || a.state == "scheduled")
    });
    // RFC 3339 UTC strings of one format sort by time.
    rows.sort_by(|a, b| {
        let (sa, sb) = (a.state == "scheduled", b.state == "scheduled");
        sb.cmp(&sa).then_with(|| if sa { a.at.cmp(&b.at) } else { b.at.cmp(&a.at) }).then_with(|| a.message_id.cmp(&b.message_id))
    });
    let next_at = rows.iter().find(|a| a.state == "scheduled").map(|a| a.at.clone());
    rows.truncate(query.limit);
    json!({
        "hostId": host_id,
        "alarms": rows.iter().map(|a| alarm_view(a, names, query.session.as_deref())).collect::<Vec<_>>(),
        "nextAt": next_at,
    })
}

// ---------------------------------------------------------------------------
// Routes

fn error(stream: &mut TcpStream, status: u16, code: &str, message: &str, retryable: bool) -> anyhow::Result<()> {
    let reason = match status {
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        _ => "Service Unavailable",
    };
    super::server::write_v2_error(stream, status, reason, code, message, retryable, json!({}))
}

/// Socket entry point; the caller has already verified the root credential.
pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let store = match alarms::store_path() {
        Ok(store) => store,
        Err(e) => return error(stream, 503, "alarm_store_unavailable", &format!("{e:#}"), true),
    };
    match (request.method.as_str(), parse_path(path)) {
        ("GET", Some(Route::Collection)) => {
            let query = match parse_query(&request.path) {
                Ok(query) => query,
                Err(message) => return error(stream, 400, "invalid_request", &message, false),
            };
            match alarms::read_all_at(&store) {
                Ok(rows) => {
                    let body = list_resource(rows, &query, &session_names(), &crate::platform::host::current_host_name());
                    write_json_response(stream, 200, "OK", &body)
                }
                Err(e) => error(stream, 503, "alarm_store_unavailable", &format!("{e:#}"), true),
            }
        }
        ("GET", Some(Route::Item(id))) => match alarms::read_all_at(&store) {
            Ok(rows) => match rows.iter().find(|a| a.message_id == id) {
                Some(alarm) => {
                    let mut view = alarm_view(alarm, &session_names(), None);
                    view["message"] = json!(alarm.message);
                    write_json_response(stream, 200, "OK", &json!({ "alarm": view }))
                }
                None => error(stream, 404, "alarm_not_found", "no alarm with this id", false),
            },
            Err(e) => error(stream, 503, "alarm_store_unavailable", &format!("{e:#}"), true),
        },
        ("DELETE", Some(Route::Item(id))) => match alarms::cancel_by_message_id_at(&store, &id) {
            Ok(alarm) => {
                // Publish at once instead of on the next watcher tick.
                poll_now();
                write_json_response(stream, 200, "OK", &json!({ "alarm": alarm_view(&alarm, &session_names(), None) }))
            }
            Err(OwnerCancelError::NotFound) => error(stream, 404, "alarm_not_found", "no alarm with this id", false),
            Err(OwnerCancelError::AlreadyQueued(_)) => error(
                stream,
                409,
                "alarm_already_queued",
                "the alarm was already handed to the Agent Bus; its message cannot be retracted",
                false,
            ),
            Err(OwnerCancelError::Store(e)) => error(stream, 503, "alarm_store_unavailable", &format!("{e:#}"), true),
        },
        (_, Some(_)) => error(stream, 405, "method_not_allowed", "method not allowed for this alarms route", false),
        (_, None) => error(stream, 404, "not_found", "unknown alarms route", false),
    }
}

// ---------------------------------------------------------------------------
// Change events (store watcher)

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    state: String,
    attempts: u64,
    owner: String,
    to: String,
}

impl Fingerprint {
    fn of(alarm: &Alarm) -> Self {
        Self { state: alarm.state.clone(), attempts: alarm.attempts, owner: alarm.owner.clone(), to: alarm.to.clone() }
    }
}

#[derive(Debug)]
pub struct Change {
    pub reason: &'static str,
    pub id: String,
    pub owner: String,
    pub to: String,
    /// The alarm now (None = removed from the store).
    pub alarm: Option<Alarm>,
}

fn state_reason(state: &str) -> &'static str {
    match state {
        "queued" => "queued",
        "cancelled" => "cancelled",
        _ => "scheduled",
    }
}

/// What changed between the last seen fingerprints and the store now.
pub fn diff(known: &BTreeMap<String, Fingerprint>, now: &[Alarm]) -> Vec<Change> {
    let mut changes = Vec::new();
    for alarm in now {
        let reason = match known.get(&alarm.message_id) {
            None if alarm.state == "scheduled" => "created",
            None => state_reason(&alarm.state),
            Some(old) if old.state != alarm.state => state_reason(&alarm.state),
            Some(old) if old.attempts != alarm.attempts => "retrying",
            Some(_) => continue,
        };
        changes.push(Change { reason, id: alarm.message_id.clone(), owner: alarm.owner.clone(), to: alarm.to.clone(), alarm: Some(alarm.clone()) });
    }
    for (id, old) in known {
        if !now.iter().any(|a| &a.message_id == id) {
            changes.push(Change { reason: "removed", id: id.clone(), owner: old.owner.clone(), to: old.to.clone(), alarm: None });
        }
    }
    changes
}

pub fn event_params(change: &Change, names: &BTreeMap<String, String>, host_id: &str) -> Value {
    json!({
        "reason": change.reason,
        "id": change.id,
        "alarm": change.alarm.as_ref().map(|a| alarm_view(a, names, None)),
        "ownerCutexSessionId": if human_owner(&change.owner) { Value::Null } else { json!(change.owner) },
        "targetCutexSessionId": change.to,
        "hostId": host_id,
    })
}

#[derive(Default)]
struct Watch {
    primed: bool,
    stamp: Option<(SystemTime, u64)>,
    last_read: Option<Instant>,
    known: BTreeMap<String, Fingerprint>,
}

static WATCH: Mutex<Option<Watch>> = Mutex::new(None);

fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    std::fs::metadata(path).ok().map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()))
}

/// One watcher step: re-read the store when it changed (or every minute),
/// return the changes since the previous read. The first read only primes.
fn step(watch: &mut Watch, path: &Path, force: bool) -> anyhow::Result<Vec<Change>> {
    let now_stamp = stamp(path);
    let stale = watch.last_read.is_none_or(|t| t.elapsed() >= FULL_READ_INTERVAL);
    if watch.primed && !force && !stale && now_stamp == watch.stamp {
        return Ok(Vec::new());
    }
    let rows = alarms::read_all_at(path)?;
    watch.stamp = now_stamp;
    watch.last_read = Some(Instant::now());
    let changes = if watch.primed { diff(&watch.known, &rows) } else { Vec::new() };
    watch.known = rows.iter().map(|a| (a.message_id.clone(), Fingerprint::of(a))).collect();
    watch.primed = true;
    Ok(changes)
}

fn publish(changes: &[Change]) {
    if changes.is_empty() {
        return;
    }
    let Ok(repository) = super::repository::management_v2_repository() else {
        return;
    };
    let names = session_names();
    let host_id = crate::platform::host::current_host_name();
    for change in changes {
        if let Err(error) = super::server::append_cutex_event(
            repository,
            super::services::HOST_EVENT_SESSION_ID,
            super::model::EventCorrelation::default(),
            ALARMS_CHANGED_EVENT,
            event_params(change, &names, &host_id),
        ) {
            eprintln!("alarms: could not publish event: {error:#}");
        }
    }
}

fn poll(force: bool) -> anyhow::Result<()> {
    let path = alarms::store_path()?;
    let changes = {
        let mut guard = WATCH.lock().unwrap_or_else(|e| e.into_inner());
        let watch = guard.get_or_insert_with(Watch::default);
        step(watch, &path, force)?
    };
    publish(&changes);
    Ok(())
}

/// Publish pending changes now (after an owner cancel).
pub fn poll_now() {
    if let Err(error) = poll(true) {
        eprintln!("alarms watcher: {error:#}");
    }
}

/// Management serve: watch the alarm store for `cutex/alarms/changed`.
pub fn start_watcher() -> anyhow::Result<()> {
    std::thread::Builder::new().name("cutex-alarms-watch".into()).spawn(|| {
        let mut failing = false;
        loop {
            match poll(false) {
                Ok(()) => failing = false,
                Err(error) => {
                    if !failing {
                        eprintln!("alarms watcher: {error:#}");
                    }
                    failing = true;
                }
            }
            std::thread::sleep(WATCH_INTERVAL);
        }
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "cutex.00000000-0000-0000-0000-00000000000a";
    const B: &str = "cutex.00000000-0000-0000-0000-00000000000b";

    fn alarm(id: &str, owner: &str, to: &str, at: &str, state: &str) -> Alarm {
        Alarm {
            alarm_id: format!("name-{id}"),
            owner: owner.into(),
            to: to.into(),
            message: "check the build".into(),
            at: at.into(),
            delay_seconds: None,
            message_id: format!("alarm-{id}"),
            state: state.into(),
            attempts: 0,
            next_attempt: 0,
            receipt: None,
            last_error: None,
            last_attempt: None,
        }
    }

    fn names() -> BTreeMap<String, String> {
        BTreeMap::from([(A.to_string(), "Eva".to_string()), (B.to_string(), "Rei".to_string())])
    }

    #[test]
    fn routes_and_queries() {
        assert_eq!(parse_path("/v2/alarms"), Some(Route::Collection));
        assert_eq!(parse_path("/v2/alarms/alarm-1f2e-ab"), Some(Route::Item("alarm-1f2e-ab".into())));
        assert_eq!(parse_path("/v2/alarms/remind"), None);
        assert_eq!(parse_path("/v2/alarms/alarm-x/y"), None);
        assert!(is_path("/v2/alarms/anything") && !is_path("/v2/alarmsx"));
        let q = parse_query("/v2/alarms?sessionId=cutex.abc&state=all&limit=5").unwrap();
        assert_eq!((q.session.as_deref(), q.state, q.limit), (Some("cutex.abc"), StateFilter::All, 5));
        let q = parse_query("/v2/alarms").unwrap();
        assert_eq!((q.session, q.state, q.limit), (None, StateFilter::Pending, DEFAULT_LIMIT));
        assert!(parse_query("/v2/alarms?state=done").is_err());
        assert!(parse_query("/v2/alarms?limit=0").is_err());
        assert!(parse_query("/v2/alarms?sessionId=a%20b").is_err());
    }

    #[test]
    fn list_filters_orders_and_matches_the_schema() {
        let mut long = alarm("4", A, A, "2026-10-11T03:00:00+00:00", "scheduled");
        long.message = "x".repeat(PREVIEW_CHARS + 5);
        let rows = vec![
            alarm("1", A, B, "2026-10-11T05:00:00+00:00", "scheduled"),
            alarm("2", B, A, "2026-10-11T04:00:00+00:00", "scheduled"),
            alarm("3", A, B, "2026-10-10T04:00:00+00:00", "queued"),
            long,
            alarm("5", "human", B, "2026-10-11T01:00:00+00:00", "scheduled"),
            alarm("6", A, B, "2026-10-10T06:00:00+00:00", "cancelled"),
        ];
        let pending = ListQuery { session: Some(A.into()), state: StateFilter::Pending, limit: 10 };
        let value = list_resource(rows.clone(), &pending, &names(), "h");
        let ids: Vec<_> = value["alarms"].as_array().unwrap().iter().map(|a| a["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["alarm-4", "alarm-2", "alarm-1"]);
        assert_eq!(value["nextAt"], "2026-10-11T03:00:00+00:00");
        let rel: Vec<_> = value["alarms"].as_array().unwrap().iter().map(|a| a["relation"].as_str().unwrap()).collect();
        assert_eq!(rel, ["self", "incoming", "outgoing"]);
        assert_eq!(value["alarms"][0]["messageTruncated"], true);
        assert_eq!(value["alarms"][0]["messagePreview"].as_str().unwrap().chars().count(), PREVIEW_CHARS);
        assert_eq!(value["alarms"][1]["ownerName"], "Rei");
        assert_eq!(value["alarms"][1]["targetName"], "Eva");
        super::super::schema_export::assert_valid("alarms", "alarmList", &value);

        let all = ListQuery { session: None, state: StateFilter::All, limit: 10 };
        let value = list_resource(rows.clone(), &all, &names(), "h");
        let ids: Vec<_> = value["alarms"].as_array().unwrap().iter().map(|a| a["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["alarm-5", "alarm-4", "alarm-2", "alarm-1", "alarm-6", "alarm-3"]);
        assert_eq!(value["alarms"][0]["ownerKind"], "human");
        assert!(value["alarms"][0]["ownerCutexSessionId"].is_null());
        assert!(value["alarms"][0].get("relation").is_none());
        super::super::schema_export::assert_valid("alarms", "alarmList", &value);

        let limited = ListQuery { session: None, state: StateFilter::All, limit: 1 };
        let value = list_resource(rows, &limited, &names(), "h");
        assert_eq!(value["alarms"].as_array().unwrap().len(), 1);
        assert_eq!(value["nextAt"], "2026-10-11T01:00:00+00:00");

        let mut item = alarm_view(&alarm("9", A, B, "2026-10-11T05:00:00+00:00", "cancelled"), &BTreeMap::new(), None);
        assert!(item["targetName"].is_null());
        item["message"] = json!("full");
        super::super::schema_export::assert_valid("alarms", "alarmItem", &json!({ "alarm": item }));
    }

    #[test]
    fn diff_reports_each_kind_of_change() {
        let mut known = BTreeMap::new();
        let base = vec![alarm("1", A, B, "t1", "scheduled"), alarm("2", A, B, "t2", "scheduled"), alarm("3", A, B, "t3", "scheduled")];
        for a in &base {
            known.insert(a.message_id.clone(), Fingerprint::of(a));
        }
        assert!(diff(&known, &base).is_empty());
        let mut now = base.clone();
        now[0].state = "queued".into();
        now[1].attempts = 1;
        now.remove(2);
        now.push(alarm("4", B, A, "t4", "scheduled"));
        now.push(alarm("5", B, A, "t5", "cancelled"));
        let changes = diff(&known, &now);
        let got: Vec<_> = changes.iter().map(|c| (c.reason, c.id.as_str())).collect();
        assert_eq!(got, [("queued", "alarm-1"), ("retrying", "alarm-2"), ("created", "alarm-4"), ("cancelled", "alarm-5"), ("removed", "alarm-3")]);
        let removed = changes.iter().find(|c| c.reason == "removed").unwrap();
        assert_eq!((removed.owner.as_str(), removed.to.as_str(), removed.alarm.is_none()), (A, B, true));
        for change in &changes {
            let params = event_params(change, &names(), "h");
            crate::management::v2::contract_validation::validate_cutex_event_message(&json!({"method": ALARMS_CHANGED_EVENT, "params": params}))
                .unwrap();
        }
        assert!(crate::management::v2::contract_validation::validate_cutex_event_message(
            &json!({"method": ALARMS_CHANGED_EVENT, "params": {"reason": "bogus", "id": "x", "alarm": null, "ownerCutexSessionId": null, "targetCutexSessionId": A, "hostId": "h"}})
        )
        .is_err());
    }

    #[test]
    fn watcher_primes_then_reports_store_changes() {
        let dir = std::env::temp_dir().join(format!("alarm-watch-{}", uuid::Uuid::new_v4()));
        let path = dir.join("alarms.json");
        let mut watch = Watch::default();
        assert!(step(&mut watch, &path, false).unwrap().is_empty());
        let write = |rows: &[Alarm]| {
            std::fs::create_dir_all(&dir).unwrap();
            let map: BTreeMap<String, &Alarm> = rows.iter().map(|a| (a.message_id.clone(), a)).collect();
            std::fs::write(&path, serde_json::to_vec_pretty(&map).unwrap()).unwrap();
        };
        write(&[alarm("1", A, B, "t1", "scheduled")]);
        let changes = step(&mut watch, &path, false).unwrap();
        assert_eq!(changes.iter().map(|c| c.reason).collect::<Vec<_>>(), ["created"]);
        // Unchanged metadata: no re-read, nothing reported.
        assert!(step(&mut watch, &path, false).unwrap().is_empty());
        write(&[alarm("1", A, B, "t1", "cancelled")]);
        let changes = step(&mut watch, &path, true).unwrap();
        assert_eq!(changes.iter().map(|c| c.reason).collect::<Vec<_>>(), ["cancelled"]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
