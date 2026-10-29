//! Owner-only host storage inventory. Walks inspect metadata, never file contents.
//! Directory results are persisted; GET only reads the cache and stats named files.
use crate::http::server::{write_json_response, SimpleHttpRequest};
use anyhow::Context;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

pub const PATH: &str = "/v2/storage";
const AUTO_INTERVAL: i64 = 24 * 3600;

struct Store {
    id: &'static str,
    group: &'static str,
    path: &'static str,
    rule: &'static str,
    kind: &'static str,
}
macro_rules! stores { ($(($id:literal,$group:literal,$path:literal,$rule:literal,$kind:literal)),* $(,)?) => { const STORES: &[Store] = &[$(Store { id:$id, group:$group, path:$path, rule:$rule, kind:$kind }),*]; }; }
stores![
    (
        "light-homes",
        "history",
        "runtime/light",
        "none",
        "directory"
    ),
    (
        "codex-sessions",
        "history",
        "codex-home/sessions",
        "native",
        "directory"
    ),
    (
        "codex-thread-history",
        "history",
        "codex-home/thread_history_1.sqlite",
        "native",
        "sqlite"
    ),
    (
        "session-store",
        "history",
        "cutex-sessions.sqlite3",
        "internal",
        "sqlite"
    ),
    (
        "history-media",
        "media",
        "runtime/management-v2/history-media",
        "size",
        "directory"
    ),
    (
        "attachments",
        "media",
        "runtime/attachments",
        "forever",
        "directory"
    ),
    (
        "generated-images",
        "media",
        "codex-home/generated_images",
        "native",
        "directory"
    ),
    (
        "artifacts",
        "results",
        "runtime/artifacts",
        "forever",
        "directory"
    ),
    (
        "documents",
        "results",
        "runtime/documents",
        "internal",
        "directory"
    ),
    ("asks", "messages", "runtime/asks", "archive", "directory"),
    (
        "bus-messages",
        "messages",
        "runtime/management-v2/agent-bus-message-state.json",
        "archive",
        "json"
    ),
    ("archive", "messages", "archive", "forever", "directory"),
    (
        "owner-notify",
        "messages",
        "runtime/owner-notify",
        "archive",
        "directory"
    ),
    (
        "approvals",
        "messages",
        "runtime/approvals",
        "internal",
        "directory"
    ),
    (
        "alarms",
        "messages",
        "runtime/alarms",
        "internal",
        "directory"
    ),
    (
        "usage-ledger",
        "usage",
        "runtime/management-v2/usage-ledger",
        "forever",
        "directory"
    ),
    (
        "usage-ledger-external",
        "usage",
        "runtime/management-v2/usage-ledger-external",
        "forever",
        "directory"
    ),
    (
        "quota-history",
        "usage",
        "runtime/management-v2/usage-quota-history",
        "forever",
        "directory"
    ),
    (
        "project-events",
        "activity",
        "runtime/project-events",
        "forever",
        "directory"
    ),
    (
        "workflow-runs",
        "activity",
        "runtime/workflows/runs.sqlite3",
        "forever",
        "sqlite"
    ),
    (
        "history-reverts",
        "activity",
        "runtime/history-reverts",
        "forever",
        "directory"
    ),
    (
        "events",
        "activity",
        "runtime/management-v2/events.jsonl",
        "rotate",
        "jsonl"
    ),
    ("summary", "activity", "summary", "derived", "directory"),
    ("notes", "activity", "notes", "forever", "directory"),
    (
        "notifications",
        "activity",
        "notifications",
        "count",
        "directory"
    ),
    ("daemon-logs", "logs", "logs", "none", "directory"),
    ("release-logs", "logs", "logs", "none", "mixed"),
    (
        "diagnostics",
        "logs",
        "runtime/diagnostics",
        "none",
        "directory"
    ),
    (
        "app-server-journal",
        "logs",
        "runtime/app-server-journal",
        "none",
        "directory"
    ),
    (
        "events-orphaned",
        "logs",
        "runtime/management-v2/orphaned",
        "none",
        "directory"
    ),
    (
        "claude-runtime",
        "logs",
        "runtime/claude",
        "rotate",
        "directory"
    ),
    (
        "audit-logs",
        "logs",
        "runtime/agent-bus-audit.jsonl",
        "rotate",
        "mixed"
    ),
    (
        "claude-fork-leftovers",
        "backups",
        "runtime/claude/fork-copy-leftovers",
        "forever",
        "directory"
    ),
    (
        "codex-home-backups",
        "backups",
        "codex-home",
        "none",
        "mixed"
    ),
    ("migration-leftovers", "backups", ".", "none", "mixed"),
    (
        "agent-management",
        "management",
        "runtime/agent-management",
        "internal",
        "directory"
    ),
    (
        "user-input",
        "management",
        "runtime/management-v2/user-input-state.json",
        "internal",
        "json"
    ),
    (
        "activity-projector",
        "management",
        "runtime/management-v2/activity-projector-state.json",
        "internal",
        "json"
    ),
    (
        "session-projection",
        "management",
        "runtime/management-v2/session-projection-state.json",
        "internal",
        "json"
    ),
    (
        "session-activity",
        "management",
        "runtime/management-v2/session-activity-state.json",
        "internal",
        "json"
    ),
    (
        "session-usage",
        "management",
        "runtime/management-v2/session-usage-state.json",
        "internal",
        "json"
    ),
    (
        "integration-dedupe",
        "management",
        "runtime/management-v2/integration-event-dedupe-v1.json",
        "internal",
        "json"
    ),
    (
        "request-idempotency",
        "management",
        "runtime/management-v2/request-idempotency.sqlite3",
        "internal",
        "sqlite"
    ),
    (
        "registries",
        "management",
        "runtime/agent-bus-registry.json",
        "internal",
        "mixed"
    ),
    (
        "profiles",
        "management",
        "runtime/profile-avatars",
        "internal",
        "mixed"
    ),
    (
        "storage",
        "management",
        "runtime/storage",
        "internal",
        "directory"
    ),
    (
        "codex-plugins",
        "native",
        "codex-home/plugins",
        "native",
        "directory"
    ),
    ("other", "other", ".", "none", "mixed"),
];

fn label(id: &str) -> String {
    match id {
        "light-homes" => "Agents' local copies".into(),
        "codex-sessions" => "Native conversations".into(),
        "codex-thread-history" => "Native thread history".into(),
        "history" => "Conversation history".into(),
        "media" => "Images and attachments".into(),
        "results" => "Results".into(),
        "messages" => "Delegations and messages".into(),
        "logs" => "Logs and diagnostics".into(),
        "management" => "Management state".into(),
        _ => {
            let text = id.replace('-', " ");
            let mut chars = text.chars();
            chars
                .next()
                .map(|c| c.to_uppercase().collect::<String>() + chars.as_str())
                .unwrap_or_default()
        }
    }
}

fn under(path: &str, root: &str) -> bool {
    path == root || path.strip_prefix(root).is_some_and(|s| s.starts_with('/'))
}
fn sidecar(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|s| matches!(s, "-wal" | "-shm" | "-journal"))
}
fn classify(path: &str) -> &'static Store {
    let name = path.rsplit('/').next().unwrap_or(path);
    let special = if path.starts_with("logs/")
        && (name.starts_with("agent-bus-r") || name.starts_with("management-r"))
    {
        Some("release-logs")
    } else if path.starts_with("runtime/agent-bus-audit.jsonl")
        || path.starts_with("runtime/peers/peer-audit.jsonl")
        || path.starts_with("users/audit.jsonl")
    {
        Some("audit-logs")
    } else if path.starts_with("runtime/management-v2/events.jsonl") {
        Some("events")
    } else if path.starts_with("runtime/management-v2/integration-event-dedupe-") {
        Some("integration-dedupe")
    } else if under(path, "runtime/artifacts") {
        Some("artifacts")
    } else if path.starts_with("codex-home/")
        && path.split('/').nth(1).is_some_and(|n| {
            n.starts_with("logs-db-backup-")
                || n == "db-backups"
                || n.starts_with("logs_2") && n.contains("bak")
                || n.starts_with("sqlite-migration-backup")
                || n.starts_with("state-db-backup")
        })
    {
        Some("codex-home-backups")
    } else if !path.contains('/')
        && [
            "cutex-sessions.json.bak-",
            ".pre-sqlite-",
            ".sentinel-leaked-",
            "config.before-",
        ]
        .iter()
        .any(|prefix| name.starts_with(prefix))
        || under(path, "history-repair-backups")
    {
        Some("migration-leftovers")
    } else if path.contains("registry")
        || path == "runtime/peers.json"
        || path.starts_with("runtime/management-v2/event-owners")
        || path.starts_with("runtime/management-v2/pending-server-requests")
    {
        Some("registries")
    } else if path.starts_with("runtime/profile-") || path.starts_with("profiles/") {
        Some("profiles")
    } else {
        None
    };
    if let Some(id) = special {
        return STORES.iter().find(|s| s.id == id).unwrap();
    }
    STORES
        .iter()
        .filter(|s| {
            !matches!(
                s.id,
                "other" | "migration-leftovers" | "codex-home-backups" | "release-logs"
            )
        })
        .filter(|s| under(path, s.path) || s.kind == "sqlite" && sidecar(path, s.path))
        .max_by_key(|s| s.path.len())
        .unwrap_or_else(|| STORES.last().unwrap())
}

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Measurement {
    #[serde(default)]
    rollout_bytes: u64,
    #[serde(default)]
    item_count: Option<u64>,
    bytes: u64,
    files: u64,
    links: u64,
    unreadable: u64,
    oldest_at: Option<String>,
    newest_at: Option<String>,
}
impl Measurement {
    fn add(&mut self, meta: &fs::Metadata) {
        if link(meta) {
            self.links += 1;
            return;
        }
        if !meta.is_file() {
            return;
        }
        self.bytes += meta.len();
        self.files += 1;
        if let Ok(at) = meta.modified() {
            let at = chrono::DateTime::<chrono::Utc>::from(at).to_rfc3339();
            if self.oldest_at.as_ref().is_none_or(|v| v > &at) {
                self.oldest_at = Some(at.clone());
            }
            if self.newest_at.as_ref().is_none_or(|v| v < &at) {
                self.newest_at = Some(at);
            }
        }
    }
}
// Inspect each component before a live stat / SQL count, so a parent junction
// cannot make a named file escape the inventory root.
fn metadata_beneath(home: &Path, relative: &str) -> std::io::Result<Option<fs::Metadata>> {
    let mut path = home.to_owned();
    let parts: Vec<_> = Path::new(relative).components().collect();
    for (i, part) in parts.iter().enumerate() {
        if !matches!(part, std::path::Component::Normal(_)) {
            return Ok(None);
        }
        path.push(part.as_os_str());
        let meta = fs::symlink_metadata(&path)?;
        if i + 1 == parts.len() {
            return Ok(Some(meta));
        }
        if link(&meta) {
            return Ok(None);
        }
    }
    Ok(None)
}

fn link(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    meta.file_type().is_symlink()
}
#[derive(Default)]
struct WalkState {
    running: bool,
    started: Option<String>,
    finished: Option<String>,
    duration_ms: Option<u64>,
    visited: u64,
    error: Option<String>,
    cancel: Arc<AtomicBool>,
}
static STATES: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<WalkState>>>>> = OnceLock::new();
fn state(home: &Path) -> Arc<Mutex<WalkState>> {
    STATES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(home.to_owned())
        .or_default()
        .clone()
}
fn ensure_storage_dir(home: &Path) -> anyhow::Result<PathBuf> {
    for relative in ["runtime", "runtime/storage"] {
        match metadata_beneath(home, relative) {
            Ok(Some(meta)) => anyhow::ensure!(
                meta.is_dir() && !link(&meta),
                "storage cache parent must be a real directory"
            ),
            Ok(None) => anyhow::bail!("storage cache parent contains a link"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    let dir = home.join("runtime/storage");
    crate::management::client_tokens::ensure_private_dir(&dir)?;
    Ok(dir)
}
fn database(home: &Path) -> anyhow::Result<Connection> {
    let dir = ensure_storage_dir(home)?;
    let db = Connection::open(dir.join("storage.sqlite3"))?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.pragma_update(None, "journal_mode", "WAL")?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS walk(store TEXT PRIMARY KEY, measured_at TEXT NOT NULL, value TEXT NOT NULL); CREATE TABLE IF NOT EXISTS walk_items(store TEXT NOT NULL, path TEXT NOT NULL, bytes INTEGER NOT NULL, value TEXT NOT NULL, PRIMARY KEY(store,path));")?;
    Ok(db)
}
fn item_path(path: &str, store: &Store) -> String {
    let parts: Vec<_> = path.split('/').collect();
    let count = match store.id {
        "light-homes" if path.starts_with("runtime/light/agents/") => 4,
        "codex-home-backups" => 2,
        "migration-leftovers" => 1,
        _ => store.path.split('/').count() + 1,
    };
    parts[..count.min(parts.len())].join("/")
}

fn light_sessions(home: &Path) -> Option<HashMap<String, Value>> {
    let relative = "cutex-sessions.sqlite3";
    if !metadata_beneath(home, relative)
        .ok()
        .flatten()
        .is_some_and(|m| m.is_file() && !link(&m))
    {
        return None;
    }
    let db = Connection::open_with_flags(
        home.join(relative),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .ok()?;
    db.busy_timeout(Duration::from_millis(50)).ok()?;
    let mut statement=db.prepare("SELECT cutex_session_id,formal_agent_name,archive_state,json_extract(record_json,'$.explicit_launch.native_home') FROM sessions").ok()?;
    let rows = statement
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })
        .ok()?;
    let mut out = HashMap::new();
    for row in rows {
        let (id, name, archive, native_home) = row.ok()?;
        let value = json!({"cutexSessionId":id,"name":name.unwrap_or_else(||id.clone()),"state":if archive=="retired" {"retired"} else {"active"}});
        out.insert(
            format!(
                "runtime/light/agents/{}",
                id.strip_prefix("cutex.").unwrap_or(&id)
            )
            .to_ascii_lowercase(),
            value.clone(),
        );
        if let Some(native) = native_home {
            let home_text = home.to_string_lossy().replace('\\', "/");
            let native = native.replace('\\', "/");
            if let Some(relative) = native.strip_prefix(&format!("{home_text}/")) {
                out.insert(relative.to_ascii_lowercase(), value);
            }
        }
    }
    Some(out)
}

fn walk(
    home: &Path,
    progress: &Arc<Mutex<WalkState>>,
    selected: Option<&std::collections::HashSet<String>>,
) -> anyhow::Result<()> {
    // The configured instance root is trusted and may itself be a junction.
    // Resolve only that root; nested links remain excluded by symlink_metadata.
    let canonical_home = home.canonicalize()?;
    let home = canonical_home.as_path();
    let dir = ensure_storage_dir(home)?;
    let lock = fs::File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("walk.lock"))?;
    lock.try_lock()
        .context("a storage walk is already running in another process")?;
    let cancel = progress.lock().unwrap().cancel.clone();
    let mut totals: BTreeMap<String, Measurement> = STORES
        .iter()
        .map(|s| (s.id.into(), Measurement::default()))
        .collect();
    let mut items: BTreeMap<(String, String), Measurement> = BTreeMap::new();
    let mut pending = vec![home.to_owned()];
    let mut visited = 0;
    while let Some(path) = pending.pop() {
        anyhow::ensure!(!cancel.load(Ordering::Relaxed), "storage walk cancelled");
        let rel = path
            .strip_prefix(home)?
            .to_string_lossy()
            .replace('\\', "/");
        if !rel.is_empty()
            && selected.is_some_and(|ids| {
                !STORES.iter().filter(|s| ids.contains(s.id)).any(|s| {
                    s.path == "."
                        || matches!(s.id, "registries" | "profiles" | "audit-logs")
                        || under(&rel, s.path)
                        || under(s.path, &rel)
                        || rel.starts_with(s.path) && matches!(s.kind, "sqlite" | "jsonl")
                        || s.id == "integration-dedupe"
                            && rel.starts_with("runtime/management-v2/integration-event-dedupe-")
                })
            })
        {
            continue;
        }
        let store = classify(&rel);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() && !link(&meta) => match fs::read_dir(&path) {
                Ok(entries) => {
                    for entry in entries {
                        match entry {
                            Ok(entry) => pending.push(entry.path()),
                            Err(_) => totals.get_mut(store.id).unwrap().unreadable += 1,
                        }
                    }
                }
                Err(_) => totals.get_mut(store.id).unwrap().unreadable += 1,
            },
            Ok(meta) => {
                if store.id == "light-homes"
                    && meta.is_file()
                    && !link(&meta)
                    && path.file_name().is_some_and(|name| {
                        name.to_string_lossy().starts_with("rollout")
                            && name.to_string_lossy().ends_with(".jsonl")
                    })
                {
                    totals.get_mut(store.id).unwrap().rollout_bytes += meta.len();
                    items
                        .entry((store.id.into(), item_path(&rel, store)))
                        .or_default()
                        .rollout_bytes += meta.len();
                }
                totals.get_mut(store.id).unwrap().add(&meta);
                items
                    .entry((store.id.into(), item_path(&rel, store)))
                    .or_default()
                    .add(&meta);
            }
            Err(_) => totals.get_mut(store.id).unwrap().unreadable += 1,
        }
        visited += 1;
        if visited % 2000 == 0 {
            progress.lock().unwrap().visited = visited;
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    for store in STORES {
        let count = match store.id {
            "session-store" => Some((store.path, "sessions")),
            "workflow-runs" => Some((store.path, "workflow_run")),
            "documents" => Some(("runtime/documents/documents-v1.sqlite3", "documents")),
            "artifacts" => Some(("runtime/artifacts/artifacts-v1.sqlite3", "artifacts")),
            "archive" => Some(("archive/index-v1.sqlite3", "records")),
            "project-events" => Some(("runtime/project-events/journal.sqlite3", "events")),
            _ => None,
        };
        if let Some((path, table)) = count {
            if metadata_beneath(home, path)
                .ok()
                .flatten()
                .is_some_and(|m| m.is_file() && !link(&m))
            {
                if let Ok(db) = Connection::open_with_flags(
                    home.join(path),
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
                ) {
                    let _ = db.busy_timeout(Duration::from_millis(50));
                    totals.get_mut(store.id).unwrap().item_count = db
                        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| {
                            r.get::<_, i64>(0)
                        })
                        .ok()
                        .and_then(|n| u64::try_from(n).ok());
                }
            }
        } else if store.kind == "directory" {
            totals.get_mut(store.id).unwrap().item_count =
                Some(items.keys().filter(|(id, _)| id == store.id).count() as u64);
        }
    }
    let measured = chrono::Utc::now().to_rfc3339();
    let mut db = database(home)?;
    let tx = db.transaction()?;
    if let Some(ids) = selected {
        for id in ids {
            tx.execute("DELETE FROM walk_items WHERE store=?1", [id])?;
        }
    } else {
        tx.execute("DELETE FROM walk_items", [])?;
    }
    for (store, value) in totals {
        if selected.is_some_and(|ids| !ids.contains(&store)) {
            continue;
        }
        tx.execute(
            "INSERT OR REPLACE INTO walk VALUES (?1,?2,?3)",
            params![store, measured, serde_json::to_string(&value)?],
        )?;
    }
    let sessions = light_sessions(home);
    for ((store, path), value) in items {
        if selected.is_some_and(|ids| !ids.contains(&store)) {
            continue;
        }
        let bytes = value.bytes.min(i64::MAX as u64) as i64;
        let mut value = serde_json::to_value(value)?;
        if store == "light-homes" && path.starts_with("runtime/light/agents/") {
            if let Some(session) = sessions
                .as_ref()
                .and_then(|sessions| sessions.get(&path.to_ascii_lowercase()))
            {
                for (key, entry) in session.as_object().unwrap() {
                    value[key] = entry.clone();
                }
            } else {
                value["name"] = json!(path.rsplit('/').next());
                value["state"] = json!(if sessions.is_some() {
                    "deleted"
                } else {
                    "unknown"
                });
            }
        }
        tx.execute(
            "INSERT INTO walk_items VALUES (?1,?2,?3,?4)",
            params![store, path, bytes, serde_json::to_string(&value)?],
        )?;
    }
    tx.commit()?;
    progress.lock().unwrap().visited = visited;
    Ok(())
}

pub fn refresh(home: &Path) -> anyhow::Result<Value> {
    refresh_selected(home, None)
}
pub(super) fn refresh_selected(
    home: &Path,
    selected: Option<std::collections::HashSet<String>>,
) -> anyhow::Result<Value> {
    let state = state(home);
    let mut guard = state.lock().unwrap();
    if !guard.running {
        guard.running = true;
        guard.visited = 0;
        guard.started = Some(chrono::Utc::now().to_rfc3339());
        guard.error = None;
        guard.cancel.store(false, Ordering::Relaxed);
        let home = home.to_owned();
        let worker = state.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("cutex-storage-walk".into())
            .spawn(move || {
                publish_walk("running", None);
                let started = Instant::now();
                let result = walk(&home, &worker, selected.as_ref());
                let mut guard = worker.lock().unwrap();
                guard.running = false;
                guard.finished = Some(chrono::Utc::now().to_rfc3339());
                guard.duration_ms =
                    Some(started.elapsed().as_millis().min(u64::MAX as u128) as u64);
                guard.error = result.err().map(|e| format!("{e:#}"));
                let finished = guard.finished.clone();
                drop(guard);
                publish_walk("idle", finished);
            })
        {
            guard.running = false;
            return Err(error.into());
        }
    }
    Ok(json!({"state":"running", "startedAt":guard.started}))
}

fn publish_walk(state: &str, finished: Option<String>) {
    let result = (|| -> anyhow::Result<()> {
        // Subscribers belong to this exact process repository instance. Opening
        // another writer at the same path persists events but never wakes them.
        let repository = super::repository::management_v2_repository()?;
        super::server::append_cutex_event(
            repository,
            super::services::HOST_EVENT_SESSION_ID,
            super::model::EventCorrelation::default(),
            "cutex/storage/walkChanged",
            json!({"state":state,"finishedAt":finished}),
        )?;
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!("storage: walk event failed: {error:#}");
    }
}

fn cache(home: &Path) -> anyhow::Result<BTreeMap<String, (String, Measurement)>> {
    if !home.join("runtime/storage/storage.sqlite3").exists() {
        return Ok(BTreeMap::new());
    }
    let db = database(home)?;
    let mut query = db.prepare("SELECT store, measured_at, value FROM walk")?;
    let rows = query.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut out = BTreeMap::new();
    for row in rows {
        let (id, at, value) = row?;
        out.insert(id, (at, serde_json::from_str(&value)?));
    }
    Ok(out)
}

pub fn overview(
    home: &Path,
    config: &crate::profiles::model::CodezConfig,
) -> anyhow::Result<Value> {
    let cached = cache(home)?;
    let rules = super::settings_api::retention_view(config);
    let now = chrono::Utc::now();
    let mut stores = Vec::new();
    let mut groups: BTreeMap<&str, u64> = BTreeMap::new();
    for store in STORES {
        let (mut at, mut value) = cached
            .get(store.id)
            .cloned()
            .map(|(a, v)| (Some(a), v))
            .unwrap_or_default();
        let stat = matches!(store.kind, "sqlite" | "json" | "jsonl")
            && !matches!(store.id, "events" | "integration-dedupe" | "audit-logs");
        if stat {
            let count = value.item_count;
            value = Measurement {
                item_count: count,
                ..Default::default()
            };
            for suffix in if store.kind == "sqlite" {
                vec!["", "-wal", "-shm", "-journal"]
            } else {
                vec![""]
            } {
                match metadata_beneath(home, &format!("{}{suffix}", store.path)) {
                    Ok(Some(meta)) => value.add(&meta),
                    Ok(None) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => value.unreadable += 1,
                }
            }
            at = Some(now.to_rfc3339());
        }
        let mut retention = rules.as_array().unwrap().iter().find(|r| r["id"] == store.id).cloned().unwrap_or_else(|| json!({"rule":store.rule,"days":null,"keepReferenced":false,"configurable":false,"setting":null}));
        if store.id == "events" {
            retention["minHours"] = json!(super::repository::EVENT_MIN_RETENTION_HOURS);
        }
        if store.id == "history-media" {
            retention["maxBytes"] = json!(super::media_refs::CACHE_MAX_BYTES);
        }
        *groups.entry(store.group).or_default() += value.bytes;
        let count = value.item_count;
        let mut row = serde_json::to_value(value)?;
        let object = row.as_object_mut().unwrap();
        for (key, value) in [
            ("id", json!(store.id)),
            ("group", json!(store.group)),
            ("labelKey", json!(format!("storage.store.{}", store.id))),
            ("label", json!(label(store.id))),
            ("path", json!(store.path)),
            ("kind", json!(store.kind)),
            ("measure", json!(if stat { "stat" } else { "walk" })),
            ("measuredAt", json!(at)),
            ("datesFrom", json!("mtime")),
            ("items", json!({"count":count,"unit":"item"})),
            ("retention", retention),
            ("actions", json!([])),
            ("drillDown", json!(!stat)),
        ] {
            object.insert(key.into(), value);
        }
        stores.push(row);
    }
    let state = state(home);
    let guard = state.lock().unwrap();
    let finished = cached.values().map(|(at, _)| at).max().cloned();
    let stale = cached.len() != STORES.len()
        || cached.values().any(|(at, _)| {
            chrono::DateTime::parse_from_rfc3339(at).map_or(true, |at| {
                now.signed_duration_since(at).num_seconds() > AUTO_INTERVAL
            })
        });
    Ok(
        json!({"host":crate::platform::host::current_host_name(), "generatedAt":now.to_rfc3339(),"home":home,"volume":fs2::total_space(home).ok().zip(fs2::available_space(home).ok()).map(|(total,free)| json!({"totalBytes":total,"freeBytes":free})),
        "totals":{"bytes":stores.iter().filter_map(|r| r["bytes"].as_u64()).sum::<u64>(),"files":stores.iter().filter_map(|r| r["files"].as_u64()).sum::<u64>()},
        "walk":{"state":if guard.running {"running"} else {"idle"},"startedAt":guard.started,"finishedAt":finished,"durationMs":guard.duration_ms,"nextAutoAt":Value::Null,"progress":if guard.running {json!({"entries":guard.visited})} else {Value::Null},"stale":stale,"error":guard.error},
        "groups":groups.into_iter().map(|(id,bytes)| json!({"id":id,"labelKey":format!("storage.group.{id}"),"label":label(id),"bytes":bytes})).collect::<Vec<_>>(),"stores":stores}),
    )
}

pub fn is_path(path: &str) -> bool {
    path == PATH || path.starts_with("/v2/storage/")
}
pub(super) fn handle(
    stream: &mut std::net::TcpStream,
    request: &SimpleHttpRequest,
) -> anyhow::Result<()> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    if super::storage_cleanup::is_path(path) {
        return super::storage_cleanup::handle(stream,request);
    }
    let home = crate::config::paths::config_dir()?;
    match (request.method.as_str(), path) {
        ("GET", PATH) => write_json_response(
            stream,
            200,
            "OK",
            &overview(&home, &crate::config::store::load_codez_config_checked()?)?,
        ),
        ("POST", "/v2/storage/refresh") => {
            let body: Value = if request.body.is_empty() {
                json!({})
            } else {
                match serde_json::from_slice(&request.body) {
                    Ok(v) => v,
                    Err(_) => return invalid(stream, "invalid refresh body"),
                }
            };
            if !body.is_object()
                || body.as_object().unwrap().keys().any(|k| k != "stores")
                || body.get("stores").is_some_and(|v| {
                    v.as_array()
                        .is_none_or(|ids| ids.iter().any(|id| !STORES.iter().any(|s| id == s.id)))
                })
            {
                return invalid(stream, "unknown storage store");
            }
            let selected = body.get("stores").and_then(Value::as_array).map(|ids| {
                ids.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<std::collections::HashSet<_>>()
            });
            if selected.as_ref().is_some_and(|ids| ids.is_empty()) {
                return invalid(stream, "stores must not be empty");
            }
            write_json_response(stream, 202, "Accepted", &refresh_selected(&home, selected)?)
        }
        ("GET", path) if path.starts_with("/v2/storage/stores/") && path.ends_with("/items") => {
            let id = path
                .trim_start_matches("/v2/storage/stores/")
                .trim_end_matches("/items");
            if !STORES.iter().any(|s| s.id == id) {
                return invalid(stream, "unknown storage store");
            }
            let query = super::workspace::query_params(request);
            let limit = query
                .get("limit")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(100)
                .clamp(1, 500);
            let offset = match query.get("cursor") {
                Some(v) => match v.parse::<usize>() {
                    Ok(v) => v,
                    Err(_) => return invalid(stream, "invalid cursor"),
                },
                None => 0,
            };
            if query.get("sort").is_some_and(|v| v != "bytes") {
                return invalid(stream, "sort must be bytes");
            }
            if i64::try_from(offset).is_err() {
                return invalid(stream, "invalid cursor");
            }
            write_json_response(stream, 200, "OK", &list_items(&home, id, limit, offset)?)
        }
        _ => super::server::write_v2_error(
            stream,
            404,
            "Not Found",
            "not_found",
            "storage route not available",
            false,
            json!({}),
        ),
    }
}
fn list_items(home: &Path, id: &str, limit: usize, offset: usize) -> anyhow::Result<Value> {
    let mut values: Vec<Value> = Vec::new();
    if home.join("runtime/storage/storage.sqlite3").exists() {
        let db = database(home)?;
        let mut statement=db.prepare("SELECT path,value FROM walk_items WHERE store=?1 ORDER BY bytes DESC,path ASC LIMIT ?2 OFFSET ?3")?;
        let rows = statement.query_map(
            params![
                id,
                limit.saturating_add(1).min(i64::MAX as usize) as i64,
                i64::try_from(offset)?
            ],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )?;
        for row in rows {
            let (path, value) = row?;
            let mut value: Value = serde_json::from_str(&value)?;
            value["id"] = json!(path);
            value["path"] = json!(path);
            values.push(value);
        }
    }
    let more = values.len() > limit;
    values.truncate(limit);
    Ok(
        json!({"store":id,"items":values,"nextCursor":more.then(||offset.saturating_add(limit).to_string())}),
    )
}

fn invalid(stream: &mut std::net::TcpStream, message: &str) -> anyhow::Result<()> {
    super::server::write_v2_error(
        stream,
        400,
        "Bad Request",
        "invalid_request",
        message,
        false,
        json!({}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn home() -> PathBuf {
        let path = std::env::temp_dir().join(format!("storage-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        path
    }
    fn file(home: &Path, relative: &str, data: &[u8]) {
        let path = home.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }
    fn scan(home: &Path) {
        walk(home, &state(home), None).unwrap();
    }
    fn unlink_directory_alias(path: &Path) {
        #[cfg(windows)]
        fs::remove_dir(path).unwrap();
        #[cfg(unix)]
        fs::remove_file(path).unwrap();
    }
    fn row<'a>(value: &'a Value, id: &str) -> &'a Value {
        value["stores"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["id"] == id)
            .unwrap()
    }

    #[test]
    fn p1_synthetic_light_home_inventory_preserves_restore_inputs() {
        // This is a representative path inventory, NOT a measurement of live
        // homes or an A2 deletion classifier. Every byte below is synthetic.
        let root = home();
        let id = "11111111-1111-4111-8111-111111111111";
        let ancestor = "22222222-2222-4222-8222-222222222222";
        let relative_home = format!("runtime/light/agents/{id}");
        let native = root.join(&relative_home);
        let current = format!("sessions/2026/10/24/rollout-current-{id}.jsonl");
        let inherited = format!("archived_sessions/rollout-parent-{ancestor}.jsonl");
        let parent_bytes = format!("{}\n{}\n", json!({"type":"session_meta","payload":{"id":ancestor}}),
            json!({"type":"response_item","payload":{"type":"message","content":"synthetic ancestor ".repeat(256)}})).into_bytes();
        let current_bytes = format!("{}\n{}\n", json!({"type":"session_meta","payload":{"id":id,
            "history_base":{"thread_id":ancestor,"end_byte_offset":parent_bytes.len()}}}),
            json!({"type":"response_item","payload":{"type":"message","content":"synthetic current ".repeat(512)}})).into_bytes();
        let rollout_bytes = parent_bytes.len() + current_bytes.len();
        let mut inventory: Vec<(String, String, Vec<u8>)> = Vec::new();
        let mut add = |relative: &str, category: &str, bytes: Vec<u8>| {
            file(&native, relative, &bytes);
            inventory.push((relative.into(), category.into(), bytes));
        };
        add(&current, "preserve-conversation", current_bytes);
        add(&inherited, "preserve-conversation-ancestor", parent_bytes.clone());
        add("adoption.json", "preserve-provenance", serde_json::to_vec(&json!({"native_id":id,
            "source_history":root.join("synthetic-original/sessions/source.jsonl"),
            "history_home":root.join("synthetic-original"),"sha256":"0".repeat(64),
            "dependencies":[{"path":native.join(&inherited),"sha256":crate::asks::sha256_hex(&parent_bytes)}]})).unwrap());
        add("config.toml", "preserve-launch-config", b"model = 'synthetic-model'\n".to_vec());
        add("bundle.json", "preserve-launch-contract", br#"{"version":4,"synthetic":true}"#.to_vec());
        add("auth.json", "preserve-auth-metadata", br#"{"synthetic":true,"credential":"NOT-A-REAL-CREDENTIAL"}"#.to_vec());
        add("checkpoints/recovery-v1.json", "preserve-unknown-checkpoint", br#"{"synthetic":true}"#.to_vec());
        add("models_cache.json", "candidate-rebuild-model-cache", br#"{"models":[],"synthetic":true}"#.to_vec());
        add(&format!("shell_snapshots/{id}.123456.sh"), "candidate-rebuild-shell-snapshot", b"# synthetic generated shell snapshot\n".repeat(16));
        add("log/codex-tui.log", "candidate-disposable-diagnostic-log", b"synthetic diagnostic\n".repeat(32));
        add("skills/local/SKILL.md", "preserve-user-assets", b"synthetic local skill".to_vec());
        add("memories/local.md", "preserve-user-assets", b"synthetic local memory".to_vec());
        add("plugins/local.json", "preserve-user-assets", br#"{"synthetic":true}"#.to_vec());
        add("cache/new-cache-looking-file.bin", "preserve-unknown", vec![42; 127]);
        add("tmp/new-unknown-state.bin", "preserve-unknown", vec![43; 91]);
        for (name, schema, category) in [
            ("state_5.sqlite", "CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT);", "preserve-native-state"),
            ("thread_history_1.sqlite", "CREATE TABLE thread_history(thread_id TEXT, event_json TEXT);", "preserve-native-history-index"),
            ("logs_2.sqlite", "CREATE TABLE logs(message TEXT);", "candidate-disposable-diagnostic-log-db"),
        ] {
            let db = Connection::open(native.join(name)).unwrap();
            db.execute_batch(schema).unwrap();
            if name == "state_5.sqlite" {
                db.execute("INSERT INTO threads VALUES (?1, ?2)", params![id, native.join(&current).to_string_lossy()]).unwrap();
            }
            drop(db);
            // Sidecars are opaque fake bytes: P1 must only stat these native
            // databases, never open them to count conversation/log rows.
            add(name, category, fs::read(native.join(name)).unwrap());
            add(&format!("{name}-wal"), category, vec![11; 257]);
            add(&format!("{name}-shm"), category, vec![12; 128]);
        }
        drop(add);
        let chain_before = crate::launch::native_history::recovery_sources(&native, &native.join(&current)).unwrap();
        assert_eq!(chain_before, vec![native.join(&current).canonicalize().unwrap(), native.join(&inherited).canonicalize().unwrap()]);
        scan(&root);
        let measured = cache(&root).unwrap();
        let expected_bytes: usize = inventory.iter().map(|(_, _, bytes)| bytes.len()).sum();
        assert_eq!(measured["light-homes"].1.bytes, expected_bytes as u64);
        assert_eq!(measured["light-homes"].1.files, inventory.len() as u64);
        assert_eq!(measured["light-homes"].1.rollout_bytes, rollout_bytes as u64);
        let items = list_items(&root, "light-homes", 100, 0).unwrap();
        assert_eq!(items["items"].as_array().unwrap().len(), 1);
        assert_eq!(items["items"][0]["path"], relative_home);
        assert_eq!(items["items"][0]["bytes"], expected_bytes as u64);
        assert_eq!(items["items"][0]["rolloutBytes"], rollout_bytes as u64);
        for (relative, _, expected) in &inventory {
            assert_eq!(&fs::read(native.join(relative)).unwrap(), expected, "P1 changed {relative}");
        }
        assert_eq!(crate::launch::native_history::recovery_sources(&native, &native.join(&current)).unwrap(), chain_before);
        let report = json!({"fixture":"synthetic-only-not-live-measurement","nativeHome":relative_home,
            "totalBytes":expected_bytes,"rolloutBytes":rollout_bytes,
            "rolloutShare":rollout_bytes as f64 / expected_bytes as f64,
            "files":inventory.iter().map(|(path,category,bytes)|json!({"path":path,"category":category,"bytes":bytes.len()})).collect::<Vec<_>>(),
            "policy":"Candidates require pinned-runtime regeneration proof and lifecycle/reference checks. All unknown paths are preserved. No files deleted."});
        eprintln!("P1_SYNTHETIC_LIGHT_HOME_INVENTORY {}", serde_json::to_string(&report).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn walk_changed_reaches_the_shared_live_subscription() {
        // Keep the process repository's HOME alive between subscribing and
        // publishing; parallel profile fixtures otherwise remove that root.
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        let repository = super::super::repository::management_v2_repository().unwrap();
        let subscription = repository
            .page_and_subscribe(Default::default(), 4096)
            .unwrap();
        let finished = chrono::Utc::now().to_rfc3339();
        publish_walk("idle", Some(finished.clone()));
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let event = subscription
                .receiver
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("walkChanged must be published on the process repository");
            if event.cutex.as_ref().is_some_and(|message| {
                message.method == "cutex/storage/walkChanged"
                    && message.params["finishedAt"] == finished
            }) {
                assert_eq!(event.cutex.unwrap().params["state"], "idle");
                break;
            }
        }
    }

    #[test]
    fn trusted_root_link_is_resolved_but_nested_links_are_not_followed() {
        let parent = home();
        let actual = home();
        let outside = home();
        file(&actual, "runtime/light/agents/a/data", b"hello");
        file(&outside, "foreign", b"must not be counted");
        let alias = parent.join("home-link");
        let nested = actual.join("runtime/light/nested-link");
        for (target, path) in [(&actual, &alias), (&outside, &nested)] {
            #[cfg(windows)]
            {
                let output = std::process::Command::new("cmd")
                    .args([
                        "/C",
                        "mklink",
                        "/J",
                        &path.to_string_lossy().replace('/', "\\"),
                        &target.to_string_lossy().replace('/', "\\"),
                    ])
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, path).unwrap();
        }
        scan(&alias);
        let value = overview(&alias, &Default::default()).unwrap();
        assert_eq!(row(&value, "light-homes")["bytes"], 5);
        assert_eq!(row(&value, "light-homes")["links"], 1);
        unlink_directory_alias(&nested);
        unlink_directory_alias(&alias);
        fs::remove_dir_all(parent).unwrap();
        fs::remove_dir_all(actual).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn registry_covers_known_paths_without_overlap_and_has_retention() {
        let paths = [
            ("runtime/light/agents/uuid/sessions/a.jsonl", "light-homes"),
            ("runtime/artifacts/artifacts-v1.sqlite3-wal", "artifacts"),
            ("runtime/artifacts/blobs/a", "artifacts"),
            ("runtime/documents/documents-v1.sqlite3", "documents"),
            ("runtime/management-v2/user-input-state.json", "user-input"),
            (
                "runtime/management-v2/integration-event-dedupe-seen-v1.json",
                "integration-dedupe",
            ),
            ("runtime/management-v2/events.jsonl.20261024", "events"),
            ("runtime/management-v2/orphaned/a", "events-orphaned"),
            ("logs/agent-bus-r253.log", "release-logs"),
            ("logs/management-api.log", "daemon-logs"),
            ("runtime/agent-bus-audit.jsonl.gz", "audit-logs"),
            ("runtime/peers/peer-audit.jsonl", "audit-logs"),
            ("codex-home/db-backups/a", "codex-home-backups"),
            ("runtime/claude/fork-copy-leftovers/receipt.json", "claude-fork-leftovers"),
            (
                "codex-home/state-db-backup-20261023/a",
                "codex-home-backups",
            ),
            ("cutex-sessions.json.bak-123", "migration-leftovers"),
            (
                "codex-home/thread_history_1.sqlite-wal",
                "codex-thread-history",
            ),
            ("new-unknown/a", "other"),
        ];
        for (path, id) in paths {
            assert_eq!(classify(path).id, id, "{path}");
        }
        let mut ids = std::collections::HashSet::new();
        for store in STORES {
            assert!(ids.insert(store.id));
            assert!(!store.rule.is_empty());
        }
    }

    #[test]
    fn cache_survives_reopen_live_files_stat_and_totals_partition() {
        let home = home();
        file(&home, "runtime/light/agents/a/rollout.jsonl", b"hello");
        file(&home, "codex-home/sessions/a.jsonl", b"abc");
        file(&home, "runtime/management-v2/user-input-state.json", b"{}");
        file(&home, "unclaimed.bin", b"unknown");
        scan(&home);
        let config = Default::default();
        let first = overview(&home, &config).unwrap();
        assert_eq!(row(&first, "light-homes")["bytes"], 5);
        assert_eq!(row(&first, "light-homes")["rolloutBytes"], 5);
        assert_eq!(row(&first, "other")["bytes"], 7);
        assert_eq!(
            row(&first, "user-input")["retention"]["days"],
            super::super::user_input::IDENTITY_RETENTION_DAYS
        );
        assert_eq!(
            row(&first, "events")["retention"]["minHours"],
            super::super::repository::EVENT_MIN_RETENTION_HOURS
        );
        file(
            &home,
            "runtime/light/agents/a/rollout.jsonl",
            b"hello world",
        );
        file(
            &home,
            "runtime/management-v2/user-input-state.json",
            b"{\"updated\":true}",
        );
        let next = overview(&home, &config).unwrap();
        assert_eq!(row(&next, "light-homes")["bytes"], 5, "GET must not scan");
        assert_eq!(
            row(&next, "user-input")["bytes"],
            16,
            "named files use live stat"
        );
        let sum: u64 = next["stores"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["bytes"].as_u64().unwrap())
            .sum();
        assert_eq!(next["totals"]["bytes"], sum);
        assert_eq!(cache(&home).unwrap()["light-homes"].1.bytes, 5);
        crate::management::v2::schema_export::assert_valid("storage", "overview", &next);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn get_without_cache_does_not_start_walk_or_create_cache() {
        let home = home();
        let value = overview(&home, &Default::default()).unwrap();
        assert_eq!(value["walk"]["state"], "idle");
        assert_eq!(value["walk"]["stale"], true);
        assert!(value["walk"]["nextAutoAt"].is_null());
        assert!(!home.join("runtime/storage/storage.sqlite3").exists());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn cancelled_walk_preserves_previous_cache() {
        let home = home();
        file(&home, "unclaimed", b"one");
        scan(&home);
        let prior = cache(&home).unwrap()["other"].1.bytes;
        file(&home, "unclaimed", b"longer");
        let progress = state(&home);
        progress
            .lock()
            .unwrap()
            .cancel
            .store(true, Ordering::Relaxed);
        assert!(walk(&home, &progress, None)
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
        assert_eq!(cache(&home).unwrap()["other"].1.bytes, prior);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn selective_refresh_keeps_other_cache_rows_and_paginates_items() {
        let home = home();
        file(&home, "runtime/light/agents/a/data", b"one");
        file(&home, "runtime/light/agents/b/data", b"longer");
        file(&home, "codex-home/sessions/a", b"original");
        scan(&home);
        file(&home, "codex-home/sessions/a", b"changed, but not selected");
        file(&home, "runtime/light/agents/a/data", b"even longer than b");
        let selected = ["light-homes".to_owned()].into_iter().collect();
        walk(&home, &state(&home), Some(&selected)).unwrap();
        let cached = cache(&home).unwrap();
        assert_eq!(cached["codex-sessions"].1.bytes, 8);
        let first = list_items(&home, "light-homes", 1, 0).unwrap();
        assert_eq!(first["items"][0]["path"], "runtime/light/agents/a");
        assert_eq!(first["nextCursor"], "1");
        let second = list_items(&home, "light-homes", 1, 1).unwrap();
        assert_eq!(second["items"][0]["path"], "runtime/light/agents/b");
        assert!(second["nextCursor"].is_null());
        crate::management::v2::schema_export::assert_valid("storage", "items", &first);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn sqlite_counts_are_background_only_and_include_sidecars() {
        let home = home();
        fs::create_dir_all(home.join("runtime/workflows")).unwrap();
        let db = Connection::open(home.join("runtime/workflows/runs.sqlite3")).unwrap();
        db.execute_batch(
            "CREATE TABLE workflow_run(id TEXT); INSERT INTO workflow_run VALUES('a'),('b');",
        )
        .unwrap();
        drop(db);
        scan(&home);
        let value = overview(&home, &Default::default()).unwrap();
        assert_eq!(row(&value, "workflow-runs")["items"]["count"], 2);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn directory_links_are_not_followed_in_walk_or_live_stat() {
        let home = home();
        let outside = self::home();
        file(
            &outside,
            "management-v2/user-input-state.json",
            b"outside data",
        );
        let linked = home.join("runtime");
        #[cfg(windows)]
        {
            let status = std::process::Command::new("cmd")
                .args([
                    "/C",
                    "mklink",
                    "/J",
                    &linked.to_string_lossy().replace('/', "\\"),
                    &outside.to_string_lossy().replace('/', "\\"),
                ])
                .output()
                .unwrap();
            assert!(
                status.status.success(),
                "{}",
                String::from_utf8_lossy(&status.stderr)
            );
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &linked).unwrap();
        // No scanner cache is created beneath the junction either.
        assert!(
            metadata_beneath(&home, "runtime/management-v2/user-input-state.json")
                .unwrap()
                .is_none()
        );
        let value = overview(&home, &Default::default()).unwrap();
        assert_eq!(row(&value, "user-input")["bytes"], 0);
        assert!(walk(&home, &state(&home), None).is_err());
        assert!(!outside.join("storage").exists());
        unlink_directory_alias(&linked);
        fs::create_dir_all(home.join("runtime/light")).unwrap();
        let linked = home.join("runtime/light/shared");
        #[cfg(windows)]
        {
            assert!(std::process::Command::new("cmd")
                .args([
                    "/C",
                    "mklink",
                    "/J",
                    &linked.to_string_lossy().replace('/', "\\"),
                    &outside.to_string_lossy().replace('/', "\\")
                ])
                .output()
                .unwrap()
                .status
                .success());
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &linked).unwrap();
        scan(&home);
        let value = overview(&home, &Default::default()).unwrap();
        assert_eq!(row(&value, "light-homes")["bytes"], 0);
        assert_eq!(row(&value, "light-homes")["links"], 1);
        unlink_directory_alias(&linked);
        fs::remove_dir_all(home).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    #[ignore = "manual 200k-file performance fixture"]
    fn performance_200k_files() {
        let home = home();
        let base = home.join("runtime/light/agents/fixture");
        fs::create_dir_all(&base).unwrap();
        let setup = Instant::now();
        for folder in 0..200 {
            let dir = base.join(folder.to_string());
            fs::create_dir_all(&dir).unwrap();
            for item in 0..1000 {
                fs::write(dir.join(item.to_string()), b"fixture").unwrap();
            }
        }
        let start = Instant::now();
        scan(&home);
        let walk_time = start.elapsed();
        let start = Instant::now();
        let value = overview(&home, &Default::default()).unwrap();
        let get_time = start.elapsed();
        assert_eq!(row(&value, "light-homes")["files"], 200000);
        eprintln!(
            "storage_perf files=200000 setup_and_walk_ms={} walk_ms={} get_ms={}",
            setup.elapsed().as_millis(),
            walk_time.as_millis(),
            get_time.as_millis()
        );
        assert!(
            get_time < Duration::from_millis(50),
            "GET took {get_time:?}"
        );
        fs::remove_dir_all(home).unwrap();
    }
}
