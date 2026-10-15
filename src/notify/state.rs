//! Materialized per-session notification state. Consumers choose their own aggregation.
//! This projection never changes runtime state and never stores output text.
use crate::management::v2::{
    model::{EventCheckpoint, EventEnvelope, NativeMessageKind},
    repository::{management_v2_repository, ReplayError, ReplayQuery},
};
use anyhow::{ensure, Context};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeSet, path::Path, time::Duration};

const IDLE_SECONDS: i64 = 1800;
const HEALTH_SECONDS: i64 = 20;
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Session {
    agent_id: String,
    thread_id: String,
    generation: u64,
    state: String,
    activity: i64,
    sequence: u64,
    reminder: Option<String>,
    #[serde(default)]
    reminder_at: Option<String>,
    acknowledged: bool,
    requests: BTreeSet<String>,
    turn_id: Option<String>,
}
fn open() -> anyhow::Result<Connection> {
    let root = crate::config::paths::config_dir()?.join("notifications");
    std::fs::create_dir_all(&root)?;
    let path = root.join("states.sqlite3");
    let db = open_at(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(db)
}
fn open_at(path: &Path) -> anyhow::Result<Connection> {
    let db = Connection::open(path)?;
    db.busy_timeout(Duration::from_secs(1))?;
    db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA max_page_count=4096; CREATE TABLE IF NOT EXISTS state(thread TEXT PRIMARY KEY, value TEXT NOT NULL, activity INTEGER NOT NULL); CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);")?;
    Ok(db)
}
fn meta(db: &Connection, key: &str) -> anyhow::Result<Option<String>> {
    Ok(db
        .query_row("SELECT value FROM meta WHERE key=?", [key], |r| r.get(0))
        .optional()?)
}
fn put(db: &Connection, key: &str, value: &str) -> anyhow::Result<()> {
    db.execute(
        "INSERT INTO meta VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, value],
    )?;
    Ok(())
}
fn reduce(s: &mut Session, e: &EventEnvelope) -> bool {
    if e.sequence <= s.sequence {
        return false;
    }
    let generation = e
        .correlation
        .runtime_generation
        .or_else(|| {
            e.cutex
                .as_ref()
                .and_then(|c| c.params["runtimeGeneration"].as_u64())
        })
        .unwrap_or(s.generation);
    if generation < s.generation {
        return false;
    }
    let (method, p, request) = if let Some(n) = &e.native {
        (
            n.message["method"].as_str().unwrap_or(""),
            &n.message["params"],
            n.kind == NativeMessageKind::ServerRequest,
        )
    } else if let Some(c) = &e.cutex {
        (c.method.as_str(), &c.params, false)
    } else {
        return false;
    };
    let next = match method {
        "turn/started" => Some("working"),
        "turn/completed" => match p.pointer("/turn/status").and_then(Value::as_str) {
            Some("completed" | "interrupted") => Some("waiting"),
            Some("failed") => Some("attention"),
            _ => None,
        },
        "item/tool/requestUserInput"
        | "item/commandExecution/requestApproval"
        | "item/fileChange/requestApproval"
        | "mcpServer/elicitation/request"
        | "item/permissions/requestApproval"
            if request =>
        {
            Some("attention")
        }
        "thread/closed" | "cutex/runtime/offline" | "cutex/runtime/closed" => Some("inactive"),
        "serverRequest/resolved" => Some("resolved"),
        // Only output/progress refreshes inactivity; transport heartbeats do not.
        "item/agentMessage/delta"
        | "item/reasoning/textDelta"
        | "item/reasoning/summaryTextDelta"
        | "item/commandExecution/outputDelta"
        | "item/started"
        | "item/completed" => Some("activity"),
        _ => None,
    };
    let Some(next) = next else {
        return false;
    };
    let Ok(at) = chrono::DateTime::parse_from_rfc3339(&e.received_at) else {
        return false;
    };
    if generation > s.generation {
        *s = Session {
            agent_id: s.agent_id.clone(),
            thread_id: s.thread_id.clone(),
            generation,
            ..Default::default()
        };
    }
    let turn = e.correlation.turn_id.clone().or_else(|| {
        p.pointer("/turn/id")
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    if method != "turn/started" && turn.is_some() && s.turn_id.is_some() && turn != s.turn_id {
        return false;
    }
    if method == "turn/started" {
        s.turn_id = turn;
        s.requests.clear();
    }
    if request {
        let id = e
            .native
            .as_ref()
            .and_then(|n| n.message.get("id"))
            .cloned()
            .unwrap_or(Value::Null);
        s.requests.insert(id.to_string());
    }
    if next == "resolved" {
        let removed = s.requests.remove(&p["requestId"].to_string());
        if removed && s.state == "attention" && s.requests.is_empty() {
            s.state = "working".into();
            s.reminder = None;
            s.reminder_at = None;
        }
    } else if next != "activity" {
        s.state = next.into();
        if method == "turn/completed" || next == "inactive" {
            s.requests.clear();
        }
        s.reminder = if next == "attention" || next == "waiting" {
            Some(e.event_id.clone())
        } else {
            None
        };
        s.reminder_at = s.reminder.as_ref().map(|_| e.received_at.clone());
        s.acknowledged = false;
    } else if s.state.is_empty() {
        s.state = "working".into();
    }
    s.activity = at.timestamp();
    s.sequence = e.sequence;
    true
}

/// Consume a bounded page independently of whether webhook delivery is enabled.
pub fn tick() -> anyhow::Result<bool> {
    let mut db = open()?;
    tick_at(&mut db, management_v2_repository()?)
}
fn tick_at(
    db: &mut Connection,
    repo: &crate::management::v2::repository::EventRepository,
) -> anyhow::Result<bool> {
    let checkpoint = meta(&db, "checkpoint")?
        .map(|v| serde_json::from_str::<EventCheckpoint>(&v))
        .transpose()?;
    let page = match repo.page(ReplayQuery {
        stream_id: checkpoint.as_ref().map(|c| c.stream_id.clone()),
        after: checkpoint.as_ref().and_then(|c| c.cursor.clone()),
        limit: 500,
        cutex_session_id: None,
    }) {
        Ok(page) => page,
        Err(ReplayError::CursorExpired { .. } | ReplayError::StreamChanged { .. }) => {
            let tx = db.transaction()?;
            tx.execute("DELETE FROM state", [])?;
            tx.execute("DELETE FROM meta WHERE key='checkpoint'", [])?;
            put(&tx, "gap", "true")?;
            tx.commit()?;
            return Ok(true);
        }
        Err(e) => return Err(e.into()),
    };
    let tx = db.transaction()?;
    for event in &page.events {
        let Some(thread) = event.correlation.thread_id.as_deref() else {
            continue;
        };
        if uuid::Uuid::parse_str(thread).is_err() {
            continue;
        }
        // Old events are not live notifications. The source retains full history.
        if chrono::DateTime::parse_from_rfc3339(&event.received_at)
            .map(|t| t.timestamp() < Utc::now().timestamp() - IDLE_SECONDS)
            .unwrap_or(true)
        {
            continue;
        }
        let text: Option<String> = tx
            .query_row("SELECT value FROM state WHERE thread=?", [thread], |r| {
                r.get(0)
            })
            .optional()?;
        let mut state = text
            .map(|t| serde_json::from_str::<Session>(&t))
            .transpose()?
            .unwrap_or_else(|| Session {
                agent_id: event.cutex_session_id.clone(),
                thread_id: thread.into(),
                ..Default::default()
            });
        if reduce(&mut state, event) {
            tx.execute("INSERT INTO state VALUES(?,?,?) ON CONFLICT(thread) DO UPDATE SET value=excluded.value,activity=excluded.activity",params![thread,serde_json::to_string(&state)?,state.activity])?;
        }
    }
    let checkpoint = page
        .events
        .last()
        .map(|e| EventCheckpoint {
            stream_id: e.stream_id.clone(),
            sequence: e.sequence,
            cursor: Some(e.cursor.clone()),
        })
        .unwrap_or(page.checkpoint);
    put(&tx, "checkpoint", &serde_json::to_string(&checkpoint)?)?;
    put(&tx, "heartbeat", &Utc::now().timestamp().to_string())?;
    put(
        &tx,
        "caughtUp",
        if page.has_more { "false" } else { "true" },
    )?;
    put(&tx, "error", "")?;
    tx.execute(
        "DELETE FROM state WHERE activity < ?",
        [Utc::now().timestamp() - 86400],
    )?;
    // Bound the projection independently of journal/output volume.
    tx.execute("DELETE FROM state WHERE thread IN (SELECT thread FROM state ORDER BY activity DESC LIMIT -1 OFFSET 4096)",[])?;
    tx.commit()?;
    Ok(page.has_more)
}

fn inactive_reason(s: &Session, now: i64, healthy: bool) -> Option<&'static str> {
    if !healthy {
        Some("source_unavailable")
    } else if now - s.activity >= IDLE_SECONDS {
        Some("inactive_timeout")
    } else if s.state == "inactive" {
        Some("runtime_closed")
    } else {
        None
    }
}

/// Full replacement snapshot. Consumers must expire stale responses themselves.
pub fn snapshot() -> anyhow::Result<Value> {
    let db = open()?;
    let now = Utc::now().timestamp();
    let heartbeat = meta(&db, "heartbeat")?.and_then(|t| t.parse::<i64>().ok());
    let healthy = heartbeat.is_some_and(|t| now - t < HEALTH_SECONDS)
        && meta(&db, "caughtUp")?.as_deref() == Some("true");
    let store = crate::session::store::load_cutex_session_store_shared()?;
    let mut rows = db.prepare("SELECT value FROM state ORDER BY thread")?;
    let values = rows
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut sessions = Vec::new();
    for value in values {
        let s: Session = serde_json::from_str(&value)?;
        let level = super::session::read_level(&s.thread_id);
        let reason = inactive_reason(&s, now, healthy);
        let record = store
            .sessions
            .values()
            .find(|r| r.cutex_session_id == s.agent_id);
        let name = record
            .and_then(|r| r.formal_agent_name.as_deref().or(r.display_name_hint.as_deref()).or(r.thread_name.as_deref()))
            .unwrap_or(&s.agent_id);
        let enabled = matches!(
            level,
            Ok(super::session::Level::Important | super::session::Level::Normal)
        );
        sessions.push(json!({"agentId":s.agent_id,"agentName":name,"threadId":s.thread_id,"priority":level.as_ref().ok(),"preferenceError":level.is_err(),"state":if reason.is_some(){"inactive"}else{&s.state},"reason":reason,"lastActivityAt":s.activity,"inactiveAt":s.activity+IDLE_SECONDS,"reminderId":s.reminder,"unread":enabled && reason.is_none() && s.reminder.is_some() && !s.acknowledged}));
    }
    Ok(
        json!({"schema":"cutex/notification-state/v1","generatedAt":now,"expiresAt":now+HEALTH_SECONDS,"healthy":healthy,"sourceHeartbeat":heartbeat,"checkpoint":meta(&db,"checkpoint")?.and_then(|s|serde_json::from_str::<Value>(&s).ok()),"retentionGapObserved":meta(&db,"gap")?.is_some(),"sessions":sessions,"coverage":"registered_runtime_events","runtimeKinds":["cutex_agent","cutex_session"],"automaticInteractionAck":true,"interactionAckScope":"updated_cute_codex_frontends","interactionAckInputs":["key","paste"]}),
    )
}

/// Current unread reminder for the session control, without loading the agent catalog.
pub fn current_reminder(thread: &str) -> anyhow::Result<Option<(String, Option<String>)>> {
    uuid::Uuid::parse_str(thread)?;
    let db = open()?;
    let now = Utc::now().timestamp();
    let healthy = meta(&db, "heartbeat")?
        .and_then(|v| v.parse::<i64>().ok())
        .is_some_and(|t| now - t < HEALTH_SECONDS)
        && meta(&db, "caughtUp")?.as_deref() == Some("true");
    let value: Option<String> = db
        .query_row("SELECT value FROM state WHERE thread=?1", [thread], |r| {
            r.get(0)
        })
        .optional()?;
    let Some(value) = value else { return Ok(None) };
    let s: Session = serde_json::from_str(&value)?;
    Ok(
        if !s.acknowledged && inactive_reason(&s, now, healthy).is_none() {
            s.reminder.map(|id| (id, s.reminder_at))
        } else {
            None
        },
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Ack {
    pub thread_id: String,
    pub reminder_id: String,
}
pub fn acknowledge(ack: Ack) -> anyhow::Result<Value> {
    uuid::Uuid::parse_str(&ack.thread_id)?;
    ensure!(
        !ack.reminder_id.is_empty() && ack.reminder_id.len() <= 512,
        "invalid reminderId"
    );
    acknowledge_at(&mut open()?, ack)
}
/// The owner read the session (`POST /v2/summary/read`): acknowledge its
/// current reminder, whatever it is. Ok(false) when nothing was unread or
/// the session has no notification state yet.
pub fn mark_thread_read(thread_id: &str) -> anyhow::Result<bool> {
    mark_thread_read_at(&mut open()?, thread_id)
}
fn mark_thread_read_at(db: &mut Connection, thread_id: &str) -> anyhow::Result<bool> {
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let text: Option<String> = tx
        .query_row("SELECT value FROM state WHERE thread=?", [thread_id], |r| r.get(0))
        .optional()?;
    let Some(text) = text else {
        return Ok(false);
    };
    let mut s: Session = serde_json::from_str(&text)?;
    let changed = s.reminder.is_some() && !s.acknowledged;
    if changed {
        s.acknowledged = true;
        tx.execute(
            "UPDATE state SET value=? WHERE thread=?",
            params![serde_json::to_string(&s)?, thread_id],
        )?;
    }
    tx.commit()?;
    Ok(changed)
}
fn acknowledge_at(db: &mut Connection, ack: Ack) -> anyhow::Result<Value> {
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let text: String = tx
        .query_row(
            "SELECT value FROM state WHERE thread=?",
            [&ack.thread_id],
            |r| r.get(0),
        )
        .context("session has no observed notification state")?;
    let mut s: Session = serde_json::from_str(&text)?;
    let matched = s.reminder.as_deref() == Some(&ack.reminder_id);
    if matched {
        s.acknowledged = true;
        tx.execute(
            "UPDATE state SET value=? WHERE thread=?",
            params![serde_json::to_string(&s)?, ack.thread_id],
        )?;
    }
    tx.commit()?;
    Ok(json!({"acknowledged":matched,"threadId":ack.thread_id,"reminderId":ack.reminder_id}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::management::v2::model::{EventCorrelation, EventSource, NativeMessage};
    fn event(seq: u64, method: &str, p: Value) -> EventEnvelope {
        EventEnvelope {
            contract_version: 2,
            event_id: format!("event-{seq}"),
            cursor: format!("cursor-{seq}"),
            stream_id: "stream".into(),
            sequence: seq,
            received_at: Utc::now().to_rfc3339(),
            cutex_session_id: "agent".into(),
            host_id: "host".into(),
            source: EventSource::AppServer,
            sensitivity: "owner".into(),
            schema: None,
            correlation: EventCorrelation {
                runtime_generation: Some(1),
                ..Default::default()
            },
            native: Some(NativeMessage {
                kind: NativeMessageKind::Notification,
                message: json!({"method":method,"params":p}),
            }),
            cutex: None,
        }
    }
    fn question(seq: u64, id: i64) -> EventEnvelope {
        let mut e = event(seq, "item/tool/requestUserInput", json!({}));
        let n = e.native.as_mut().unwrap();
        n.kind = NativeMessageKind::ServerRequest;
        n.message["id"] = json!(id);
        e
    }
    #[test]
    fn collector_checkpoint_and_ack_survive_reopen_without_replaying_reminders() {
        use crate::management::v2::{
            model::{AppServerSchema, AppServerSchemaChannel, PendingEvent},
            repository::EventRepository,
        };
        let root = std::env::temp_dir().join(format!("cutex-state-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let repo = EventRepository::open(root.join("events"), "host").unwrap();
        let thread = uuid::Uuid::new_v4().to_string();
        let mut e = event(1, "turn/completed", json!({"turn":{"status":"completed"}}));
        e.correlation.thread_id = Some(thread.clone());
        let stored = repo
            .append(PendingEvent {
                cutex_session_id: e.cutex_session_id,
                host_id: e.host_id,
                source: e.source,
                schema: Some(AppServerSchema {
                    protocol: "codex-app-server".into(),
                    major_version: 2,
                    version: "test".into(),
                    sha256: "a".repeat(64),
                    channel: AppServerSchemaChannel::Stable,
                    capabilities: json!({}),
                    extensions: vec![],
                }),
                correlation: e.correlation,
                native: e.native,
                cutex: None,
            })
            .unwrap();
        let mut db = open_at(&root.join("state.sqlite")).unwrap();
        assert!(!tick_at(&mut db, &repo).unwrap());
        acknowledge_at(
            &mut db,
            Ack {
                thread_id: thread.clone(),
                reminder_id: stored.event_id.clone(),
            },
        )
        .unwrap();
        drop(db);
        let mut db = open_at(&root.join("state.sqlite")).unwrap();
        tick_at(&mut db, &repo).unwrap();
        let value: String = db
            .query_row("SELECT value FROM state WHERE thread=?", [&thread], |r| {
                r.get(0)
            })
            .unwrap();
        let s: Session = serde_json::from_str(&value).unwrap();
        assert!(s.acknowledged);
        assert_eq!(s.reminder, Some(stored.event_id));
        assert_eq!(s.state, "waiting");
        drop(db);
        drop(repo);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mark_thread_read_acknowledges_any_current_reminder() {
        let root = std::env::temp_dir().join(format!("cutex-state-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let mut db = open_at(&root.join("state.sqlite")).unwrap();
        assert!(!mark_thread_read_at(&mut db, "missing").unwrap());
        let s = Session {
            state: "attention".into(),
            reminder: Some("r1".into()),
            ..Default::default()
        };
        db.execute(
            "INSERT INTO state(thread, value, activity) VALUES(?, ?, 0)",
            params!["t", serde_json::to_string(&s).unwrap()],
        )
        .unwrap();
        assert!(mark_thread_read_at(&mut db, "t").unwrap());
        assert!(!mark_thread_read_at(&mut db, "t").unwrap());
        let value: String = db
            .query_row("SELECT value FROM state WHERE thread='t'", [], |r| r.get(0))
            .unwrap();
        let s: Session = serde_json::from_str(&value).unwrap();
        assert!(s.acknowledged);
        assert_eq!(s.reminder.as_deref(), Some("r1"));
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn inactivity_and_source_loss_override_reminders_without_acknowledging_them() {
        let mut s = Session {
            activity: 100,
            state: "attention".into(),
            reminder: Some("r".into()),
            ..Default::default()
        };
        assert_eq!(inactive_reason(&s, 1899, true), None);
        assert_eq!(inactive_reason(&s, 1900, true), Some("inactive_timeout"));
        assert_eq!(inactive_reason(&s, 101, false), Some("source_unavailable"));
        assert!(!s.acknowledged);
        reduce(&mut s, &event(1, "thread/closed", json!({})));
        assert_eq!(
            inactive_reason(&s, s.activity, true),
            Some("runtime_closed")
        );
        assert!(s.reminder.is_none());
    }

    #[test]
    fn lifecycle_reminders_and_output_are_separate() {
        let mut s = Session::default();
        reduce(
            &mut s,
            &event(1, "turn/started", json!({"turn":{"id":"t"}})),
        );
        assert_eq!(s.state, "working");
        assert!(s.reminder.is_none());
        reduce(
            &mut s,
            &event(
                2,
                "turn/completed",
                json!({"turn":{"id":"t","status":"completed"}}),
            ),
        );
        s.acknowledged = true;
        reduce(
            &mut s,
            &event(3, "item/agentMessage/delta", json!({"delta":"SECRET"})),
        );
        assert_eq!(s.state, "waiting");
        assert!(s.acknowledged);
        assert!(!serde_json::to_string(&s).unwrap().contains("SECRET"));
        reduce(
            &mut s,
            &event(4, "turn/started", json!({"turn":{"id":"new"}})),
        );
        assert_eq!(s.state, "working");
        assert!(s.reminder.is_none());
        reduce(
            &mut s,
            &event(
                5,
                "turn/completed",
                json!({"turn":{"id":"t","status":"failed"}}),
            ),
        );
        assert_eq!(s.state, "working");
    }
    #[test]
    fn elicitation_and_permissions_are_attention_until_both_resolve() {
        let mut s = Session::default();
        for (seq, method) in [(1, "mcpServer/elicitation/request"), (2, "item/permissions/requestApproval")] {
            let mut e = event(seq, method, json!({}));
            assert!(!reduce(&mut s, &e), "notifications with request names are not requests");
            let native = e.native.as_mut().unwrap();
            native.kind = NativeMessageKind::ServerRequest;
            native.message["id"] = json!(seq);
            assert!(reduce(&mut s, &e));
            assert_eq!(s.state, "attention");
            assert_eq!(s.reminder_at.as_deref(), Some(e.received_at.as_str()));
        }
        reduce(&mut s, &event(3, "serverRequest/resolved", json!({"requestId":1})));
        assert_eq!(s.state, "attention");
        reduce(&mut s, &event(4, "serverRequest/resolved", json!({"requestId":2})));
        assert_eq!(s.state, "working");
        assert!(s.reminder.is_none());
    }

    #[test]
    fn multiple_questions_clear_only_when_all_resolved() {
        let mut s = Session::default();
        reduce(&mut s, &question(1, 10));
        reduce(&mut s, &question(2, 11));
        reduce(
            &mut s,
            &event(3, "serverRequest/resolved", json!({"requestId":10})),
        );
        assert_eq!(s.state, "attention");
        reduce(
            &mut s,
            &event(4, "serverRequest/resolved", json!({"requestId":99})),
        );
        assert_eq!(s.state, "attention");
        reduce(
            &mut s,
            &event(5, "serverRequest/resolved", json!({"requestId":11})),
        );
        assert_eq!(s.state, "working");
        assert!(s.reminder.is_none());
    }
    #[test]
    fn old_generation_and_replayed_events_cannot_replace_new_state() {
        let mut s = Session::default();
        let e = event(2, "turn/started", json!({}));
        reduce(&mut s, &e);
        assert!(!reduce(&mut s, &e));
        let mut old = event(3, "thread/closed", json!({}));
        old.correlation.runtime_generation = Some(0);
        assert!(!reduce(&mut s, &old));
        assert_eq!(s.state, "working");
    }
    #[test]
    fn stale_ack_cannot_clear_new_reminder_or_other_session() {
        let mut db = open_at(Path::new(":memory:")).unwrap();
        for id in ["a", "b"] {
            let s = Session {
                thread_id: id.into(),
                reminder: Some("new".into()),
                ..Default::default()
            };
            db.execute(
                "INSERT INTO state VALUES(?,?,0)",
                params![id, serde_json::to_string(&s).unwrap()],
            )
            .unwrap();
        }
        assert_eq!(
            acknowledge_at(
                &mut db,
                Ack {
                    thread_id: "a".into(),
                    reminder_id: "old".into()
                }
            )
            .unwrap()["acknowledged"],
            false
        );
        assert_eq!(
            acknowledge_at(
                &mut db,
                Ack {
                    thread_id: "a".into(),
                    reminder_id: "new".into()
                }
            )
            .unwrap()["acknowledged"],
            true
        );
        let b: String = db
            .query_row("SELECT value FROM state WHERE thread='b'", [], |r| r.get(0))
            .unwrap();
        assert!(!serde_json::from_str::<Session>(&b).unwrap().acknowledged);
    }
}
