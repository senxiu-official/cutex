//! Notification adapter over the owner event journal. Never mutates source events.
use super::session::Level;
use crate::management::v2::{
    model::{EventCheckpoint, EventEnvelope, NativeMessageKind},
    repository::{management_v2_repository, ReplayError, ReplayQuery},
};
use anyhow::{ensure, Context};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

const MAX_ROWS: i64 = 2048;
const MAX_PAYLOAD: usize = 8192;
const RETAIN_SECONDS: i64 = 7 * 86400;
const MAX_ATTEMPTS: i64 = 6;
pub const EVENT_TYPES: &[&str] = &[
    "agent.running",
    "agent.attention_required",
    "agent.turn_completed",
    "agent.turn_failed",
    "agent.turn_interrupted",
    "job.completed",
    // owner_notify messages whose channels include the phone (owner-messages).
    "agent.message_to_owner",
];
/// Removed with the Task Service in r219. An r218 `outbound.json` lists them
/// (they were in the default set): dropped on read and on save instead of
/// failing validation. Drop this list in r220.
const RETIRED_EVENT_TYPES: &[&str] = &["task.review_ready", "task.closed"];

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub enabled: bool,
    pub url: Option<String>,
    pub token: Option<String>,
    pub events: Vec<String>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            url: None,
            token: None,
            events: EVENT_TYPES.iter().map(|s| s.to_string()).collect(),
        }
    }
}
fn root() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::config_dir()?.join("notifications"))
}
fn read_config_at(root: &Path) -> anyhow::Result<Config> {
    let path = root.join("outbound.json");
    if !path.exists() {
        return Ok(Config::default());
    }
    let mut data = Vec::new();
    fs::File::open(path)?.take(16385).read_to_end(&mut data)?;
    ensure!(data.len() <= 16384, "notification config exceeds 16 KiB");
    let mut config: Config =
        serde_json::from_slice(&data).context("invalid outbound configuration")?;
    drop_retired_events(&mut config);
    validate(&config)?;
    Ok(config)
}
fn drop_retired_events(config: &mut Config) {
    config
        .events
        .retain(|event| !RETIRED_EVENT_TYPES.contains(&event.as_str()));
}
pub fn config() -> anyhow::Result<Config> {
    read_config_at(&root()?)
}
fn validate(config: &Config) -> anyhow::Result<()> {
    if config.enabled {
        ensure!(config.url.is_some(), "webhook URL required when enabled");
    }
    if let Some(raw) = &config.url {
        let url = url::Url::parse(raw).map_err(|_| anyhow::anyhow!("invalid webhook URL"))?;
        ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none(),
            "webhook requires HTTP(S) without embedded credentials or fragment"
        );
        ensure!(
            raw.len() <= 2048 && !raw.chars().any(char::is_control),
            "invalid webhook URL length/characters"
        );
    }
    if let Some(token) = &config.token {
        ensure!(
            token.len() <= 2048 && !token.chars().any(char::is_control),
            "invalid webhook token"
        );
    }
    ensure!(
        config
            .events
            .iter()
            .all(|s| EVENT_TYPES.contains(&s.as_str())),
        "unsupported notification event type"
    );
    Ok(())
}
pub fn set_config(config: &Config) -> anyhow::Result<Value> {
    set_config_at(&root()?, config)
}
fn set_config_at(root: &Path, config: &Config) -> anyhow::Result<Value> {
    let mut config = config.clone();
    drop_retired_events(&mut config);
    validate(&config)?;
    crate::config::atomic::write_private_pretty_json_atomic(
        &root.join("outbound.json"),
        &config,
        "notification outbound",
    )?;
    Ok(public_config(&config))
}
fn public_config(c: &Config) -> Value {
    json!({"enabled":c.enabled,"url":c.url,"urlConfigured":c.url.is_some(),"tokenConfigured":c.token.is_some(),"events":c.events})
}
fn destination(c: &Config) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(&c.url, &c.token)).unwrap())
    )
}
fn now() -> i64 {
    Utc::now().timestamp()
}

pub struct Store {
    db: Connection,
}
impl Store {
    fn open(root: &Path) -> anyhow::Result<Self> {
        fs::create_dir_all(root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
        }
        let db = Connection::open(root.join("outbound.sqlite3"))?;
        db.busy_timeout(Duration::from_secs(1))?;
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA max_page_count=8192; CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY,value TEXT NOT NULL); CREATE TABLE IF NOT EXISTS delivery(id TEXT PRIMARY KEY,payload TEXT NOT NULL,target TEXT NOT NULL,state TEXT NOT NULL,attempts INTEGER NOT NULL DEFAULT 0,next_attempt INTEGER NOT NULL,created INTEGER NOT NULL,last_error TEXT); CREATE INDEX IF NOT EXISTS due ON delivery(state,next_attempt);")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                root.join("outbound.sqlite3"),
                fs::Permissions::from_mode(0o600),
            )?;
        }
        Ok(Self { db })
    }
    fn meta(&self, key: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .db
            .query_row("SELECT value FROM meta WHERE key=?", [key], |r| r.get(0))
            .optional()?)
    }
    fn put(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.db.execute(
            "INSERT INTO meta VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value WHERE meta.value<>excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
    fn prune(&self) -> anyhow::Result<()> {
        self.db.execute(
            "DELETE FROM delivery WHERE created < ?",
            [now() - RETAIN_SECONDS],
        )?;
        Ok(())
    }
    fn enqueue(&self, payload: &Value, target: &str) -> anyhow::Result<bool> {
        let text = serde_json::to_string(payload)?;
        ensure!(
            text.len() <= MAX_PAYLOAD,
            "notification payload exceeds bound"
        );
        let id = payload["eventId"].as_str().context("event ID missing")?;
        if self
            .db
            .query_row("SELECT 1 FROM delivery WHERE id=?", [id], |_| Ok(()))
            .optional()?
            .is_some()
        {
            return Ok(false);
        }
        self.prune()?;
        let count: i64 = self
            .db
            .query_row("SELECT COUNT(*) FROM delivery", [], |r| r.get(0))?;
        if count >= MAX_ROWS {
            self.db.execute("DELETE FROM delivery WHERE id IN (SELECT id FROM delivery WHERE state IN ('delivered','failed','cancelled') ORDER BY created LIMIT 1)",[])?;
            let count: i64 = self
                .db
                .query_row("SELECT COUNT(*) FROM delivery", [], |r| r.get(0))?;
            if count >= MAX_ROWS {
                self.put(
                    "lastError",
                    "outbox_full: newest notification dropped; source event retained",
                )?;
                self.db.execute("INSERT INTO meta VALUES('dropped','1') ON CONFLICT(key) DO UPDATE SET value=CAST(value AS INTEGER)+1",[])?;
                return Ok(false);
            }
        }
        self.db.execute("INSERT OR IGNORE INTO delivery(id,payload,target,state,next_attempt,created) VALUES(?,?,?,'pending',?,?)",params![id,text,target,now(),now()])?;
        Ok(true)
    }
    fn ingest(
        &mut self,
        checkpoint: &EventCheckpoint,
        payloads: &[Value],
        target: &str,
    ) -> anyhow::Result<()> {
        self.db.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| {
            for payload in payloads {
                self.enqueue(payload, target)?;
            }
            self.put("checkpoint", &serde_json::to_string(checkpoint)?)
        })();
        if result.is_ok() {
            self.db.execute_batch("COMMIT")?;
        } else {
            self.db.execute_batch("ROLLBACK")?;
        }
        result
    }
    fn list(&self, state: Option<&str>) -> anyhow::Result<Value> {
        let mut stmt = self.db.prepare("SELECT id,state,attempts,created,last_error FROM delivery WHERE (?1 IS NULL OR state=?1) ORDER BY created DESC LIMIT 100")?;
        let rows = stmt.query_map([state],|r|Ok(json!({"eventId":r.get::<_,String>(0)?,"state":r.get::<_,String>(1)?,"attempts":r.get::<_,i64>(2)?,"createdAt":r.get::<_,i64>(3)?,"lastError":r.get::<_,Option<String>>(4)?})))?.collect::<Result<Vec<_>,_>>()?;
        Ok(json!({"deliveries":rows}))
    }
}

/// Whitelist metadata only. Raw messages remain available through /v2/events.
pub fn map_event(e: &EventEnvelope, level: Level, name: &str) -> Option<Value> {
    if let Some(payload) = map_owner_message(e, name) {
        return payload;
    }
    if level == Level::Off {
        return None;
    }
    let (method, p, request) = if let Some(n) = &e.native {
        (
            n.message.get("method")?.as_str()?,
            n.message.get("params").unwrap_or(&Value::Null),
            n.kind == NativeMessageKind::ServerRequest,
        )
    } else {
        let c = e.cutex.as_ref()?;
        (c.method.as_str(), &c.params, false)
    };
    let kind = match method {
        "turn/started" => "agent.running",
        "turn/completed" => match p.pointer("/turn/status").and_then(Value::as_str) {
            Some("failed") => "agent.turn_failed",
            Some("interrupted") => "agent.turn_interrupted",
            Some("completed") => "agent.turn_completed",
            _ => return None,
        },
        "item/tool/requestUserInput"
        | "item/commandExecution/requestApproval"
        | "item/fileChange/requestApproval"
        | "mcpServer/elicitation/request"
        | "item/permissions/requestApproval"
            if request =>
        {
            "agent.attention_required"
        }
        "cutex/jobService/terminalObserved" if e.native.is_none() => "job.completed",
        _ => return None,
    };
    // Native events do not consistently carry their occurrence timestamp.
    let occurred = p
        .get("committed_at")
        .and_then(Value::as_str)
        .filter(|s| chrono::DateTime::parse_from_rfc3339(s).is_ok());
    let bounded = |s: &str| {
        s.chars()
            .filter(|c| !c.is_control())
            .take(256)
            .collect::<String>()
    };
    // `taskId` stays in the cutex/notification/v1 shape; always null since the
    // Task Service was removed in r219.
    let mut payload = json!({"schema":"cutex/notification/v1","eventId":e.event_id,"sourceStreamId":e.stream_id,"sourceCursor":e.cursor,"type":kind,"observedAt":e.received_at,"occurredAt":occurred,"priority":level,"severity":if kind=="agent.turn_failed" {"error"} else if kind=="agent.attention_required" {"attention"} else {"info"},"synthetic":false,"agent":{"id":e.cutex_session_id,"name":bounded(name)},"threadId":e.correlation.thread_id,"turnId":e.correlation.turn_id,"projectId":p.get("project_id").and_then(Value::as_str).map(bounded),"taskId":Value::Null,"summary":kind});
    if kind == "job.completed" {
        let job: crate::agent_bus::job_completion::CompletionV2 =
            serde_json::from_value(p.clone()).ok()?;
        job.validate().ok()?;
        if job.target_cutex_session_id != e.cutex_session_id {
            return None;
        }
        payload["job"] = json!({
            "id": job.job_id, "actionId": job.facts.action_id,
            "state": job.terminal_status, "exitCode": job.facts.exit_code,
            "execution": job.facts.execution,
            "stdout": job.facts.stdout, "stderr": job.facts.stderr,
            "outputReference": job.output_reference,
        });
    }
    Some(payload)
}

/// `agent.message_to_owner` for `cutex/ownerNotify/created`: `Some(None)`
/// when the record does not go to the phone, `None` for other events. The
/// owner's `ownerNotify` delivery settings decide, not the session's
/// notification level (an agent's message is addressed to the owner).
fn map_owner_message(e: &EventEnvelope, name: &str) -> Option<Option<Value>> {
    let c = e.cutex.as_ref().filter(|_| e.native.is_none())?;
    if c.method != crate::owner_notify::CREATED_EVENT {
        return None;
    }
    let record = &c.params["notification"];
    let phone = record["channels"].as_array().is_some_and(|channels| channels.iter().any(|ch| ch == "phone"));
    let old_system_notice = record["origin"] == "system" && record.get("actions").is_none();
    if !phone || old_system_notice || record["agentId"].as_str() != Some(e.cutex_session_id.as_str()) {
        return Some(None);
    }
    let bounded = |s: &str, max: usize| s.chars().filter(|c| !c.is_control() || *c == '\n').take(max).collect::<String>();
    let level = record["level"].as_str().unwrap_or("info");
    let preview = crate::management::v2::summary::preview_text(record["text"].as_str().unwrap_or_default(), 240).unwrap_or_default();
    Some(Some(json!({
        "schema": "cutex/notification/v1", "eventId": e.event_id, "sourceStreamId": e.stream_id,
        "sourceCursor": e.cursor, "type": "agent.message_to_owner", "observedAt": e.received_at,
        "occurredAt": record["createdAt"], "priority": if level == "info" { "normal" } else { "important" },
        "severity": if level == "info" { "info" } else { "attention" }, "synthetic": false,
        "agent": {"id": e.cutex_session_id, "name": bounded(name, 256)},
        "threadId": e.correlation.thread_id, "turnId": record["turnId"], "projectId": null, "taskId": null,
        "summary": preview,
        "message": {
            "notificationId": record["notificationId"], "level": level, "preview": preview,
            "needsReply": record["needsReply"].as_bool().unwrap_or(false),
        },
    })))
}

// curl is the platform HTTP/TLS client. Credentials and payload travel through stdin,
// never shell interpolation or process arguments. No redirects or response-body storage.
fn post(c: &Config, payload: &str) -> anyhow::Result<()> {
    validate(c)?;
    let quote = |s: &str| {
        format!(
            "\"{}\"",
            s.replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
                .replace('\r', "\\r")
        )
    };
    let mut input = format!(
        "url = {}\nheader = \"Content-Type: application/json\"\ndata-binary = {}\n",
        quote(c.url.as_deref().context("webhook URL missing")?),
        quote(payload)
    );
    if let Some(token) = &c.token {
        input.push_str(&format!(
            "header = {}\n",
            quote(&format!("Authorization: Bearer {token}"))
        ));
    }
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let mut child = Command::new("curl")
        .args([
            "--disable",
            "--silent",
            "--connect-timeout",
            "2",
            "--max-time",
            "5",
            "--proto",
            "=http,https",
            "--output",
            null,
            "--write-out",
            "%{http_code}",
            "--config",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("curl is required for notification HTTP(S)")?;
    child
        .stdin
        .take()
        .context("curl stdin unavailable")?
        .write_all(input.as_bytes())?;
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "webhook transport failed (curl exit {:?})",
        output.status.code()
    );
    let code = std::str::from_utf8(&output.stdout)?.trim().parse::<u16>()?;
    ensure!((200..300).contains(&code), "webhook HTTP {code}");
    Ok(())
}
fn deliver_one(store: &Store, c: &Config) -> anyhow::Result<bool> {
    if !c.enabled {
        return Ok(false);
    }
    store.prune()?;
    let row:Option<(String,String,String,i64)> = store.db.query_row("SELECT id,payload,target,attempts FROM delivery WHERE state='pending' AND next_attempt<=? ORDER BY created LIMIT 1",[now()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    let Some((id, payload, target, attempts)) = row else {
        return Ok(false);
    };
    if target != destination(c) {
        store.db.execute("UPDATE delivery SET state='failed',last_error='destination_changed: explicit retry required' WHERE id=?",[id])?;
        return Ok(true);
    }
    // Turning a session OFF also cancels reminders that have not left the queue.
    let v: Value = serde_json::from_str(&payload)?;
    if !v["synthetic"].as_bool().unwrap_or(false) {
        if let Some(thread) = v["threadId"].as_str() {
            let level = match super::session::read_level(thread) {
                Ok(level) => level,
                Err(_) => {
                    store.db.execute("UPDATE delivery SET state='failed',last_error='invalid_session_preference' WHERE id=?",[id])?;
                    return Ok(true);
                }
            };
            if level == Level::Off {
                store.db.execute(
                    "UPDATE delivery SET state='cancelled',last_error='session_off' WHERE id=?",
                    [id],
                )?;
                return Ok(true);
            }
        }
    }
    let result = post(c, &payload);
    let attempts = attempts + 1;
    let (state, error) = match result {
        Ok(()) => ("delivered", None),
        Err(e) => (
            if attempts >= MAX_ATTEMPTS {
                "failed"
            } else {
                "pending"
            },
            Some(e.to_string()),
        ),
    };
    store.db.execute(
        "UPDATE delivery SET state=?,attempts=?,next_attempt=?,last_error=? WHERE id=?",
        params![
            state,
            attempts,
            now() + 2_i64.pow(attempts.min(8) as u32),
            error,
            id
        ],
    )?;
    Ok(true)
}

pub fn status() -> anyhow::Result<Value> {
    let store = Store::open(&root()?)?;
    let config = config()?;
    let heartbeat = store.meta("heartbeat")?.and_then(|v| v.parse::<i64>().ok());
    let delivery_heartbeat = store
        .meta("deliveryHeartbeat")?
        .and_then(|v| v.parse::<i64>().ok());
    let mut counts = serde_json::Map::new();
    let mut stmt = store
        .db
        .prepare("SELECT state,COUNT(*) FROM delivery GROUP BY state")?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
        let (k, v) = row?;
        counts.insert(k, json!(v));
    }
    Ok(
        json!({"config":public_config(&config),"running":heartbeat.is_some_and(|t|now()-t<20) && delivery_heartbeat.is_some_and(|t|now()-t<20),"heartbeat":heartbeat,"deliveryHeartbeat":delivery_heartbeat,"sourceCheckpoint":store.meta("checkpoint")?.and_then(|v|serde_json::from_str::<Value>(&v).ok()),"counts":counts,"lastError":store.meta("lastError")?,"dropped":store.meta("dropped")?,"retentionGaps":store.meta("gaps")?,"maxRows":MAX_ROWS,"retentionDays":7,"maxDatabaseBytes":33554432,"eventSource":"/v2/events and /v2/events/stream","coverage":["turn/started","turn/completed","item/tool/requestUserInput","item/commandExecution/requestApproval","item/fileChange/requestApproval"]}),
    )
}
pub fn deliveries(state: Option<&str>) -> anyhow::Result<Value> {
    Store::open(&root()?)?.list(state)
}
pub fn retry(id: &str) -> anyhow::Result<Value> {
    let store = Store::open(&root()?)?;
    ensure!(store.db.execute("UPDATE delivery SET state='pending',attempts=0,next_attempt=?,target=?,last_error=NULL WHERE id=? AND state IN ('failed','cancelled')",params![now(),destination(&config()?),id])?==1,"failed/cancelled notification not found");
    Ok(json!({"eventId":id,"state":"pending"}))
}
pub fn test_notification() -> anyhow::Result<Value> {
    let c = config()?;
    ensure!(
        c.enabled && c.url.is_some(),
        "configure and enable webhook before testing"
    );
    let id = format!("synthetic:{}", uuid::Uuid::new_v4());
    let payload = json!({"schema":"cutex/notification/v1","eventId":id,"type":"notification.test","synthetic":true,"observedAt":Utc::now().to_rfc3339(),"priority":"normal","summary":"Cutex webhook test"});
    ensure!(
        Store::open(&root()?)?.enqueue(&payload, &destination(&c))?,
        "notification outbox full"
    );
    Ok(json!({"eventId":id,"state":"pending"}))
}
pub fn run() -> anyhow::Result<()> {
    use fs2::FileExt;
    let root = root()?;
    let mut store = Store::open(&root)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("outbound.lock"))?;
    lock.try_lock_exclusive()
        .context("notification worker already running")?;
    crate::platform::heap::start_daemon_heap_maintenance();
    std::thread::Builder::new().name("cutex-notification-state".into()).spawn(|| {
        loop {
            match super::state::tick() {
                Ok(true) => {},
                Ok(false) => std::thread::sleep(Duration::from_secs(1)),
                Err(error) => {
                    eprintln!("notification state collector: {error}");
                    std::thread::sleep(Duration::from_secs(5));
                }
            }
        }
    })?;
    let delivery_root = root.clone();
    std::thread::spawn(move || {
        let Ok(store) = Store::open(&delivery_root) else {
            return;
        };
        let mut heartbeat = 0;
        loop {
            if now() - heartbeat >= 5 {
                heartbeat = now();
                let _ = store.put("deliveryHeartbeat", &heartbeat.to_string());
            }
            let result = read_config_at(&delivery_root).and_then(|c| deliver_one(&store, &c));
            if let Err(e) = &result {
                let _ = store.put(
                    "lastError",
                    &e.to_string().chars().take(512).collect::<String>(),
                );
            }
            if !matches!(result, Ok(true)) {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    });
    let repo = management_v2_repository()?;
    let mut heartbeat = 0;
    loop {
        if now() - heartbeat >= 5 {
            heartbeat = now();
            store.put("heartbeat", &heartbeat.to_string())?;
            store.prune()?;
        }
        let result = (|| -> anyhow::Result<bool> {
            let c = read_config_at(&root)?;
            let checkpoint = store
                .meta("checkpoint")?
                .map(|s| serde_json::from_str::<EventCheckpoint>(&s))
                .transpose()?;
            if !c.enabled {
                store.put("active", "false")?;
                return Ok(false);
            }
            if checkpoint.is_none() || store.meta("active")?.as_deref() == Some("false") {
                store.put("checkpoint", &serde_json::to_string(&repo.checkpoint()?)?)?;
                store.put("active", "true")?;
                return Ok(false);
            }
            let checkpoint = checkpoint.unwrap();
            let page = match repo.page(ReplayQuery {
                stream_id: Some(checkpoint.stream_id),
                after: checkpoint.cursor,
                limit: 500,
                cutex_session_id: None,
            }) {
                Ok(page) => page,
                Err(ReplayError::CursorExpired { .. } | ReplayError::StreamChanged { .. }) => {
                    store.put(
                        "lastError",
                        "source retention gap: resumed from current head; old events not replayed",
                    )?;
                    store.db.execute("INSERT INTO meta VALUES('gaps','1') ON CONFLICT(key) DO UPDATE SET value=CAST(value AS INTEGER)+1",[])?;
                    store.put("checkpoint", &serde_json::to_string(&repo.checkpoint()?)?)?;
                    return Ok(false);
                }
                Err(e) => return Err(e.into()),
            };
            if page.events.is_empty() {
                return Ok(false);
            }
            let sessions = crate::session::store::load_cutex_session_store_shared()?;
            let mut payloads = Vec::new();
            for event in &page.events {
                if map_event(event, Level::Normal, "").is_none() {
                    continue;
                }
                let record = sessions
                    .sessions
                    .values()
                    .find(|r| r.cutex_session_id == event.cutex_session_id);
                // Sub-agent thread events share the parent session's stream;
                // only the session's own thread drives agent notifications.
                if let (Some(event_thread), Some(session_thread)) = (
                    event.correlation.thread_id.as_deref(),
                    record.and_then(|r| r.app_server_thread_id()),
                ) {
                    if event_thread != session_thread {
                        continue;
                    }
                }
                let thread = event
                    .correlation
                    .thread_id
                    .as_deref()
                    .or_else(|| record.and_then(|r| r.codex_session_id.as_deref()));
                let level = match thread.map(super::session::read_level).transpose() {
                    Ok(value) => value.unwrap_or(Level::Off),
                    Err(_) => {
                        store.put("lastError","invalid session preference: notification skipped; original event retained")?;
                        continue;
                    }
                };
                let name = record
                    .and_then(|r| r.display_name_hint.as_deref().or(r.thread_name.as_deref()))
                    .unwrap_or(&event.cutex_session_id);
                if let Some(mut payload) = map_event(event, level, name) {
                    payload["threadId"] = json!(thread);
                    if c.events
                        .iter()
                        .any(|kind| Some(kind.as_str()) == payload["type"].as_str())
                    {
                        payloads.push(payload);
                    }
                }
            }
            let checkpoint = if let Some(last) = page.events.last() {
                EventCheckpoint {
                    stream_id: last.stream_id.clone(),
                    sequence: last.sequence,
                    cursor: Some(last.cursor.clone()),
                }
            } else {
                page.checkpoint
            };
            store.ingest(&checkpoint, &payloads, &destination(&c))?;
            Ok(page.has_more)
        })();
        if let Err(e) = &result {
            store.put(
                "lastError",
                &e.to_string().chars().take(512).collect::<String>(),
            )?;
        }
        if !matches!(result, Ok(true)) {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

pub fn handle_request(
    stream: &mut std::net::TcpStream,
    request: &crate::http::server::SimpleHttpRequest,
) -> anyhow::Result<()> {
    let path = request.path.split('?').next().unwrap_or_default();
    let result = (|| -> anyhow::Result<Value> {
        match (request.method.as_str(), path) {
            ("GET", "/v2/notifications/states") => super::state::snapshot(),
            ("POST", "/v2/notifications/ack") => {
                let result = super::state::acknowledge(serde_json::from_slice(&request.body)?)?;
                if result["acknowledged"].as_bool() == Some(true) {
                    if let Some(thread) = result["threadId"].as_str() {
                        crate::management::v2::summary::mark_read_thread(thread);
                    }
                }
                Ok(result)
            }
            ("GET", "/v2/notifications/outbound") => status(),
            ("POST", "/v2/notifications/outbound") => {
                set_config(&serde_json::from_slice(&request.body)?)
            }
            ("GET", "/v2/notifications/deliveries") => deliveries(None),
            ("POST", "/v2/notifications/test") => test_notification(),
            ("POST", "/v2/notifications/retry") => {
                let v: Value = serde_json::from_slice(&request.body)?;
                retry(v["eventId"].as_str().context("eventId required")?)
            }
            _ => anyhow::bail!(super::UNSUPPORTED_OPERATION),
        }
    })();
    match result {
        Ok(v) => crate::http::server::write_json_response(stream, 200, "OK", &v),
        Err(e) => super::write_route_error(stream, &e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::management::v2::model::{EventCorrelation, EventSource, NativeMessage};
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("cutex-notify-{}", uuid::Uuid::new_v4())))
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn event(method: &str, params: Value) -> EventEnvelope {
        EventEnvelope {
            contract_version: 2,
            event_id: "e1".into(),
            cursor: "c1".into(),
            stream_id: "s1".into(),
            sequence: 1,
            received_at: "2026-09-14T00:00:00Z".into(),
            cutex_session_id: "cutex.test".into(),
            host_id: "test".into(),
            source: EventSource::AppServer,
            sensitivity: "owner".into(),
            schema: None,
            correlation: EventCorrelation::default(),
            native: Some(NativeMessage {
                kind: NativeMessageKind::Notification,
                message: json!({"method":method,"params":params}),
            }),
            cutex: None,
        }
    }
    #[test]
    fn owner_messages_to_the_phone_map_regardless_of_session_level() {
        let created = |channels: Value, agent: &str| {
            let mut e = event("x", json!({}));
            e.native = None;
            e.source = EventSource::Cutex;
            e.cutex = Some(crate::management::v2::model::CutexMessage {
                method: crate::owner_notify::CREATED_EVENT.into(),
                params: json!({"notification": {"notificationId": "ntf-1", "agentId": agent, "text": "Need\nyour OK",
                    "level": "urgent", "needsReply": true, "channels": channels, "createdAt": "2026-10-08T00:00:00.000Z"}}),
            });
            e
        };
        let mapped = map_event(&created(json!(["desktop", "phone"]), "cutex.test"), Level::Off, "Kimi").unwrap();
        assert_eq!(mapped["type"], "agent.message_to_owner");
        assert_eq!(mapped["priority"], "important");
        assert_eq!(mapped["message"], json!({"notificationId": "ntf-1", "level": "urgent", "preview": "Need your OK", "needsReply": true}));
        assert_eq!(mapped["agent"]["name"], "Kimi");
        assert!(map_event(&created(json!(["desktop"]), "cutex.test"), Level::Important, "Kimi").is_none());
        assert!(map_event(&created(json!(["phone"]), "cutex.other"), Level::Important, "Kimi").is_none());
        // A cutex notice is a record only, even an older one that lists the phone.
        let mut notice = created(json!(["desktop", "phone"]), "cutex.test");
        notice.cutex.as_mut().unwrap().params["notification"]["origin"] = json!("system");
        assert!(map_event(&notice, Level::Important, "Kimi").is_none());
        assert!(Config::default().events.iter().any(|kind| kind == "agent.message_to_owner"));
    }
    fn payload(id: &str) -> Value {
        json!({"eventId":id,"synthetic":true,"schema":"cutex/notification/v1","summary":"test"})
    }
    #[test]
    fn adapter_drops_deltas_and_off_without_changing_raw_event() {
        let e = event("item/agentMessage/delta", json!({"delta":"SECRET"}));
        let original = e.clone();
        assert!(map_event(&e, Level::Important, "agent").is_none());
        assert_eq!(e, original);
        let e = event("turn/started", json!({"prompt":"SECRET"}));
        assert!(map_event(&e, Level::Off, "agent").is_none());
        let mapped = map_event(&e, Level::Normal, "agent").unwrap();
        assert_eq!(mapped["type"], "agent.running");
        assert!(!mapped.to_string().contains("SECRET"));
        assert!(mapped["occurredAt"].is_null());
    }
    #[test]
    fn terminal_and_attention_are_explicit_not_idle_or_history() {
        for (status, kind) in [
            ("failed", "agent.turn_failed"),
            ("completed", "agent.turn_completed"),
            ("interrupted", "agent.turn_interrupted"),
        ] {
            assert_eq!(
                map_event(
                    &event("turn/completed", json!({"turn":{"status":status}})),
                    Level::Important,
                    "a"
                )
                .unwrap()["type"],
                kind
            );
        }
        assert!(map_event(&event("turn/completed", json!({})), Level::Important, "a").is_none());
        let mut e = event(
            "item/tool/requestUserInput",
            json!({"questions":["SECRET"]}),
        );
        assert!(map_event(&e, Level::Important, "a").is_none());
        e.native.as_mut().unwrap().kind = NativeMessageKind::ServerRequest;
        let mapped = map_event(&e, Level::Important, "a").unwrap();
        assert_eq!(mapped["type"], "agent.attention_required");
        assert!(!mapped.to_string().contains("SECRET"));
        assert!(map_event(
            &event("thread/read", json!({"turns":[]})),
            Level::Normal,
            "a"
        )
        .is_none());
    }
    #[test]
    fn job_terminal_projects_metadata_only_and_requires_matching_owner() {
        let params = json!({
            "schema":"cutex.job_service.completion.v2", "eventId":"job-terminal:job_123:3",
            "jobId":"job_123", "jobRevision":3, "terminalStatus":"exited",
            "resultSha256":"a".repeat(64),
            "targetCutexSessionId":"cutex.11111111-1111-4111-8111-111111111111",
            "facts":{"factsVersion":1,"actionId":"run-training", "exitCode":0,
                "execution":{"basis":"runner_release_to_wait_v1", "observedRunDurationMillis":0},
                "stdout":{"observedBytes":100,"retainedBytes":39,"truncated":true},
                "stderr":{"observedBytes":0,"retainedBytes":0,"truncated":false}},
            "outputReference":"job-output:job_123"
        });
        let mut e = event("cutex/jobService/terminalObserved", params.clone());
        e.cutex_session_id = params["targetCutexSessionId"].as_str().unwrap().into();
        assert!(map_event(&e, Level::Normal, "a").is_none());
        e.native = None;
        e.source = EventSource::Cutex;
        e.cutex = Some(crate::management::v2::model::CutexMessage {
            method: "cutex/jobService/terminalObserved".into(), params,
        });
        let mapped = map_event(&e, Level::Normal, "a").unwrap();
        assert_eq!(mapped["type"], "job.completed");
        assert_eq!(mapped["job"]["exitCode"], 0);
        assert_eq!(mapped["job"]["execution"]["observedRunDurationMillis"], 0);
        assert_eq!(mapped["job"]["stdout"]["truncated"], true);
        assert!(mapped["occurredAt"].is_null());
        assert!(mapped["job"]["stdout"].get("text").is_none());
        assert!(map_event(&e, Level::Off, "a").is_none());
        e.cutex_session_id = "another-owner".into();
        assert!(map_event(&e, Level::Normal, "a").is_none());
    }

    #[test]
    fn queue_cursor_and_enqueue_commit_together_and_dedupe() {
        let temp = Temp::new();
        let mut store = Store::open(&temp.0).unwrap();
        let checkpoint = EventCheckpoint {
            stream_id: "s".into(),
            sequence: 1,
            cursor: Some("c".into()),
        };
        assert!(store
            .ingest(
                &checkpoint,
                &[payload("ok"), json!({"missing":"eventId"})],
                "target"
            )
            .is_err());
        assert!(store.meta("checkpoint").unwrap().is_none());
        assert_eq!(
            store.list(None).unwrap()["deliveries"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        store
            .ingest(&checkpoint, &[payload("ok"), payload("ok")], "target")
            .unwrap();
        drop(store);
        let store = Store::open(&temp.0).unwrap();
        assert!(store.meta("checkpoint").unwrap().is_some());
        assert_eq!(
            store.list(None).unwrap()["deliveries"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn outbox_bound_and_retention_are_enforced() {
        let temp = Temp::new();
        let store = Store::open(&temp.0).unwrap();
        store.db.execute("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<2048) INSERT INTO delivery(id,payload,target,state,next_attempt,created) SELECT CAST(x AS TEXT),'{}','t','pending',0,? FROM n",[now()]).unwrap();
        assert!(!store.enqueue(&payload("overflow"), "t").unwrap());
        assert_eq!(store.meta("dropped").unwrap().as_deref(), Some("1"));
        store
            .db
            .execute("UPDATE delivery SET state='delivered' WHERE id='1'", [])
            .unwrap();
        assert!(store.enqueue(&payload("next"), "t").unwrap());
        store
            .db
            .execute("UPDATE delivery SET created=0", [])
            .unwrap();
        store.prune().unwrap();
        assert_eq!(store.list(None).unwrap()["deliveries"], json!([]));
    }
    #[test]
    fn config_requires_supported_transport_and_does_not_expose_token() {
        let c = Config {
            enabled: true,
            url: Some("https://example.test/hook".into()),
            token: Some("PRIVATE_TOKEN".into()),
            ..Default::default()
        };
        validate(&c).unwrap();
        assert!(!public_config(&c).to_string().contains("PRIVATE_TOKEN"));
        for url in ["file:///tmp/out", "http://user:password@example.test/hook"] {
            assert!(validate(&Config {
                url: Some(url.into()),
                ..c.clone()
            })
            .is_err());
        }
        assert!(validate(&Config {
            token: Some("bad\r\nheader".into()),
            ..c
        })
        .is_err());
    }
    fn receiver(responses: Vec<&'static str>) -> (String, std::thread::JoinHandle<Vec<Value>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut bodies = Vec::new();
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let request = crate::http::server::read_simple_http_request(&mut stream).unwrap();
                bodies.push(serde_json::from_slice(&request.body).unwrap());
                // Split response to cover the old one-read HTTP-client bug.
                stream.write_all(b"HTTP/1.1 ").unwrap();
                std::thread::sleep(Duration::from_millis(10));
                stream.write_all(response.as_bytes()).unwrap();
            }
            bodies
        });
        (url, handle)
    }
    #[test]
    fn webhook_retry_keeps_event_identity_and_only_2xx_delivers() {
        let (url, receiver) = receiver(vec![
            "503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        ]);
        let c = Config {
            enabled: true,
            url: Some(url),
            token: Some("token-with-quote-\"-and-backslash-\\".into()),
            ..Default::default()
        };
        let temp = Temp::new();
        let store = Store::open(&temp.0).unwrap();
        store
            .enqueue(&payload("same-id"), &destination(&c))
            .unwrap();
        assert!(deliver_one(&store, &c).unwrap());
        assert_eq!(
            store.list(None).unwrap()["deliveries"][0]["state"],
            "pending"
        );
        store
            .db
            .execute("UPDATE delivery SET next_attempt=0", [])
            .unwrap();
        deliver_one(&store, &c).unwrap();
        assert_eq!(
            store.list(None).unwrap()["deliveries"][0]["state"],
            "delivered"
        );
        let bodies = receiver.join().unwrap();
        assert_eq!(bodies[0], bodies[1]);
    }
    #[test]
    fn disabled_or_changed_destination_cannot_send_queued_event() {
        let temp = Temp::new();
        let store = Store::open(&temp.0).unwrap();
        store.enqueue(&payload("e"), "old").unwrap();
        assert!(!deliver_one(&store, &Config::default()).unwrap());
        let c = Config {
            enabled: true,
            url: Some("http://127.0.0.1:1".into()),
            ..Default::default()
        };
        assert!(deliver_one(&store, &c).unwrap());
        assert_eq!(
            store.list(None).unwrap()["deliveries"][0]["lastError"],
            "destination_changed: explicit retry required"
        );
    }
    #[test]
    fn connection_failure_is_not_success_and_attempt_limit_is_finite() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let c = Config {
            enabled: true,
            url: Some(format!("http://{addr}")),
            ..Default::default()
        };
        let temp = Temp::new();
        let store = Store::open(&temp.0).unwrap();
        store.enqueue(&payload("e"), &destination(&c)).unwrap();
        store
            .db
            .execute("UPDATE delivery SET attempts=5", [])
            .unwrap();
        deliver_one(&store, &c).unwrap();
        assert_eq!(
            store.list(None).unwrap()["deliveries"][0]["state"],
            "failed"
        );
    }
    /// r218 wrote its default event set, incl. the Task Service events removed
    /// in r219: the config still loads (retired names dropped) and the next
    /// save no longer lists them. Unknown names still fail validation.
    #[test]
    fn r218_config_with_task_events_loads_and_saves_without_them() {
        let temp = Temp::new();
        fs::create_dir_all(&temp.0).unwrap();
        let r218 = json!({
            "enabled": true,
            "url": "https://example.test/hook",
            "token": "PRIVATE_TOKEN",
            "events": [
                "agent.running", "agent.attention_required", "agent.turn_completed",
                "agent.turn_failed", "agent.turn_interrupted", "task.review_ready",
                "task.closed", "job.completed", "agent.message_to_owner"
            ]
        });
        fs::write(temp.0.join("outbound.json"), serde_json::to_vec_pretty(&r218).unwrap()).unwrap();
        let loaded = read_config_at(&temp.0).unwrap();
        assert!(loaded.enabled);
        assert_eq!(loaded.events, Config::default().events);
        assert!(!loaded.events.iter().any(|e| e.starts_with("task.")));

        // Saving a config that still names them (an older client) drops them too.
        let public = set_config_at(
            &temp.0,
            &Config {
                events: vec!["task.closed".into(), "job.completed".into()],
                ..loaded.clone()
            },
        )
        .unwrap();
        assert_eq!(public["events"], json!(["job.completed"]));
        assert!(!public.to_string().contains("PRIVATE_TOKEN"));
        let saved: Value =
            serde_json::from_slice(&fs::read(temp.0.join("outbound.json")).unwrap()).unwrap();
        assert_eq!(saved["events"], json!(["job.completed"]));
        assert_eq!(read_config_at(&temp.0).unwrap().events, vec!["job.completed".to_string()]);

        // Other unknown names are still refused.
        let unknown = json!({"events": ["agent.running", "task.someday"]});
        fs::write(temp.0.join("outbound.json"), serde_json::to_vec(&unknown).unwrap()).unwrap();
        assert!(read_config_at(&temp.0).is_err());
        assert!(set_config_at(
            &temp.0,
            &Config { events: vec!["task.someday".into()], ..Config::default() }
        )
        .is_err());
    }
    #[test]
    fn task_service_events_no_longer_map_and_task_id_is_always_null() {
        let mut e = event(
            "cutex/taskService/assignmentTransitionCommitted",
            json!({"transition": "review_ready", "task_id": "task-1"}),
        );
        e.native = None;
        e.source = EventSource::Cutex;
        e.cutex = Some(crate::management::v2::model::CutexMessage {
            method: "cutex/taskService/assignmentTransitionCommitted".into(),
            params: json!({"transition": "closed", "task_id": "task-1"}),
        });
        assert!(map_event(&e, Level::Important, "a").is_none());
        let mapped = map_event(
            &event("turn/started", json!({"task_id": "task-1", "project_id": "p1"})),
            Level::Normal,
            "a",
        )
        .unwrap();
        assert!(mapped["taskId"].is_null());
        assert_eq!(mapped["projectId"], "p1");
        assert!(!EVENT_TYPES.iter().any(|kind| kind.starts_with("task.")));
    }
}
