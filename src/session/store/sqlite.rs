//! SQLite backend of the durable cutex session store (session store Phase 2,
//! `docs/research/session-store-sqlite.md` in cutex-desktop).
//!
//! The store keeps its logical path (`cutex-sessions.json`, which callers
//! and tests pass around); the database lives next to it with the extension
//! replaced (`cutex-sessions.sqlite3`). One row per session record and one
//! row per receipt; every commit stamps the rows it changed with the new
//! store revision (`rev`), so a reader refreshes its cached snapshot by
//! parsing only rows newer than the revision it already holds.
//!
//! The first open of a home that still has the JSON store imports it once
//! (under the store lock, verified, atomically renamed into place), renames
//! the JSON to `<name>.pre-sqlite-<YYYYMMDD>` (kept forever, never read
//! again) and leaves a sentinel JSON at the old path that older cutex cannot
//! parse, so an older binary fails loudly instead of starting an empty store.
//! There is no way back: rolling back means restoring a backup.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::bail;
use anyhow::Context;
use fs2::FileExt;
use rusqlite::params;
use rusqlite::Connection;
use rusqlite::OpenFlags;
use rusqlite::TransactionBehavior;
use serde::de::DeserializeOwned;
use serde::Serialize;
use sha2::Digest;

use super::CutexSessionStoreRevisionConflict;
use crate::session::model::CutexSessionRecord;
use crate::session::model::CutexSessionStore;

/// `PRAGMA user_version` of the session database.
pub(crate) const SCHEMA_VERSION: i64 = 1;
/// Value of `cutexSessionStore` in the sentinel left at the old JSON path.
pub const SENTINEL_MARKER: &str = "migrated-to-sqlite";
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);
const IMPORTING_INFIX: &str = ".importing-";
const MAX_POOLED_PER_PATH: usize = 2;
const MAX_POOLED_PATHS: usize = 4;
#[cfg(not(test))]
const MAX_CACHED_PATHS: usize = 8;
/// Parallel tests use many stores; never evict one mid-test.
#[cfg(test)]
const MAX_CACHED_PATHS: usize = 4096;

const SCHEMA: &str = "
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE sessions (
    key                TEXT PRIMARY KEY,
    cutex_session_id   TEXT NOT NULL,
    codex_session_id   TEXT,
    formal_agent_name  TEXT,
    archive_state      TEXT NOT NULL,
    rev                INTEGER NOT NULL,
    record_json        TEXT NOT NULL
);
CREATE INDEX sessions_rev   ON sessions(rev);
CREATE INDEX sessions_codex ON sessions(codex_session_id) WHERE codex_session_id IS NOT NULL;
CREATE INDEX sessions_name  ON sessions(formal_agent_name) WHERE formal_agent_name IS NOT NULL;
CREATE TABLE receipts (
    map  TEXT NOT NULL,
    key  TEXT NOT NULL,
    rev  INTEGER NOT NULL,
    json TEXT NOT NULL,
    PRIMARY KEY (map, key)
);
CREATE INDEX receipts_rev ON receipts(rev);
";

/// The database that backs the logical store path `json_path`.
pub fn database_path(json_path: &Path) -> PathBuf {
    json_path.with_extension("sqlite3")
}

// ---------------------------------------------------------------------------
// Receipt maps: one generic row shape, one marker type per store map.

/// One receipt map of the store (one `receipts.map` value). The marker types
/// below name the maps for the row-level API (`SessionRows`).
pub(crate) trait ReceiptMap {
    const NAME: &'static str;
    type Value: Clone + PartialEq + Serialize + DeserializeOwned;
    fn map(store: &CutexSessionStore) -> &BTreeMap<String, Self::Value>;
    fn map_mut(store: &mut CutexSessionStore) -> &mut BTreeMap<String, Self::Value>;
}

macro_rules! receipt_map {
    ($marker:ident, $name:literal, $field:ident, $value:ty) => {
        pub(crate) struct $marker;
        impl ReceiptMap for $marker {
            const NAME: &'static str = $name;
            type Value = $value;
            fn map(store: &CutexSessionStore) -> &BTreeMap<String, Self::Value> {
                &store.$field
            }
            fn map_mut(store: &mut CutexSessionStore) -> &mut BTreeMap<String, Self::Value> {
                &mut store.$field
            }
        }
    };
}

receipt_map!(
    NativeStop,
    "native_stop",
    native_stop_receipts,
    crate::session::model::NativeStopReceipt
);
receipt_map!(
    ExplicitLaunch,
    "explicit_launch",
    explicit_launch_receipts,
    crate::agent_management::ExplicitLaunchActionReceipt
);
receipt_map!(
    HumanAdoption,
    "human_adoption",
    human_adoption_receipts,
    crate::agent_management::HumanAdoptReceipt
);
receipt_map!(
    AgentArchive,
    "agent_archive",
    agent_archive_receipts,
    crate::agent_management::AgentArchiveReceipt
);
receipt_map!(
    FormalName,
    "formal_name",
    formal_name_receipts,
    crate::session::model::FormalAgentNameReceipt
);
receipt_map!(
    LaunchTombstone,
    "explicit_launch_tombstone",
    explicit_launch_tombstones,
    crate::session::receipt_retention::ExplicitLaunchTombstone
);

/// Runs `$call::<Map>($args)?` for every receipt map.
macro_rules! each_receipt_map {
    ($call:ident ( $($arg:expr),* )) => {{
        $call::<NativeStop>($($arg),*)?;
        $call::<ExplicitLaunch>($($arg),*)?;
        $call::<HumanAdoption>($($arg),*)?;
        $call::<AgentArchive>($($arg),*)?;
        $call::<FormalName>($($arg),*)?;
        $call::<LaunchTombstone>($($arg),*)?;
    }};
}

// ---------------------------------------------------------------------------
// Connections.

fn open_connection(db: &Path, create: bool) -> anyhow::Result<Connection> {
    let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    if create {
        flags |= OpenFlags::SQLITE_OPEN_CREATE;
    }
    let connection = Connection::open_with_flags(db, flags)
        .with_context(|| format!("failed to open cutex session database: {}", db.display()))?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    // WAL + NORMAL (owner decision 2026-10-09): a committed transaction
    // survives a process crash; an OS crash / power loss may drop the last
    // commits but never corrupts the database.
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(connection)
}

fn open_existing(db: &Path) -> anyhow::Result<Connection> {
    let connection = open_connection(db, false)?;
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != SCHEMA_VERSION {
        bail!(
            "cutex session database {} has schema version {version}, this cutex supports {SCHEMA_VERSION} (a newer cutex wrote it?)",
            db.display()
        );
    }
    Ok(connection)
}

static POOL: Mutex<Vec<(PathBuf, Vec<Connection>)>> = Mutex::new(Vec::new());

#[cfg(test)]
pub(super) static POOLED_TEST_PATHS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Connections stay open between calls in services (cheap revision checks).
/// Test runs open one per call instead: test stores live in temporary
/// directories that are removed afterwards, which Windows refuses while a
/// database file is still open.
fn pooled(db: &Path) -> bool {
    #[cfg(test)]
    {
        POOLED_TEST_PATHS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|path| path == db)
    }
    #[cfg(not(test))]
    {
        let _ = db;
        std::env::var_os("CUTEX_TEST_PRIVATE_HOME").is_none()
    }
}

fn with_connection<T>(
    db: &Path,
    action: impl FnOnce(&mut Connection) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let pool_this = pooled(db);
    let reused = if pool_this {
        let mut pool = POOL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pool.iter_mut()
            .find(|(path, _)| path == db)
            .and_then(|(_, connections)| connections.pop())
    } else {
        None
    };
    let mut connection = match reused {
        Some(connection) => connection,
        None => open_existing(db)?,
    };
    let result = action(&mut connection);
    if pool_this && result.is_ok() && connection.is_autocommit() {
        let mut pool = POOL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((_, connections)) = pool.iter_mut().find(|(path, _)| path == db) {
            if connections.len() < MAX_POOLED_PER_PATH {
                connections.push(connection);
            }
        } else {
            if pool.len() >= MAX_POOLED_PATHS {
                pool.remove(0);
            }
            pool.push((db.to_path_buf(), vec![connection]));
        }
    }
    result
}

/// Closes pooled connections to `db` (before files next to it are renamed).
fn drop_pooled(db: &Path) {
    let mut pool = POOL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    pool.retain(|(path, _)| path != db);
}

// ---------------------------------------------------------------------------
// Reading: cached snapshot per database, refreshed row-incrementally.

struct Snapshot {
    revision: u64,
    session_revs: HashMap<String, u64>,
    receipt_revs: HashMap<&'static str, HashMap<String, u64>>,
    store: Arc<CutexSessionStore>,
}

static CACHE: Mutex<Vec<(PathBuf, Snapshot)>> = Mutex::new(Vec::new());

#[cfg(test)]
thread_local! {
    /// Full reads / parsed rows on this thread, for tests (each test has
    /// its own thread).
    pub(super) static FULL_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static PARSED_ROWS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn parsed_row() {
    #[cfg(test)]
    PARSED_ROWS.with(|count| count.set(count.get() + 1));
}

fn read_revision(connection: &Connection) -> anyhow::Result<u64> {
    let value: String = connection
        .query_row(
            "SELECT value FROM meta WHERE key = 'store_revision'",
            [],
            |row| row.get(0),
        )
        .context("cutex session database has no store revision")?;
    value
        .parse()
        .with_context(|| format!("invalid cutex session store revision: {value}"))
}

/// A row on disk is older than the cached snapshot but missing from it: the
/// snapshot cannot be patched and is rebuilt from scratch.
#[derive(Debug)]
struct SnapshotGap;

impl std::fmt::Display for SnapshotGap {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("cached session snapshot is missing a row")
    }
}

impl std::error::Error for SnapshotGap {}

fn as_rev(value: i64) -> anyhow::Result<u64> {
    u64::try_from(value).context("negative cutex session row revision")
}

fn refresh_sessions(
    connection: &Connection,
    since: Option<u64>,
    revs: &mut HashMap<String, u64>,
    store: &mut CutexSessionStore,
) -> anyhow::Result<()> {
    let since = since.map_or(-1, |since| since as i64);
    let mut statement = connection
        .prepare_cached("SELECT key, rev, CASE WHEN rev > ?1 THEN record_json END FROM sessions")?;
    let mut rows = statement.query(params![since])?;
    let mut seen = HashSet::with_capacity(revs.len());
    while let Some(row) = rows.next()? {
        let key: String = row.get(0)?;
        let rev = as_rev(row.get(1)?)?;
        match row.get::<_, Option<String>>(2)? {
            Some(json) => {
                parsed_row();
                let record: CutexSessionRecord = serde_json::from_str(&json)
                    .with_context(|| format!("failed to parse cutex session row {key}"))?;
                store.sessions.insert(key.clone(), record);
                revs.insert(key.clone(), rev);
            }
            None if revs.get(&key) == Some(&rev) => {}
            None => return Err(SnapshotGap.into()),
        }
        seen.insert(key);
    }
    if seen.len() != revs.len() {
        revs.retain(|key, _| seen.contains(key));
        store.sessions.retain(|key, _| seen.contains(key));
    }
    Ok(())
}

fn refresh_map<M: ReceiptMap>(
    connection: &Connection,
    since: Option<u64>,
    all_revs: &mut HashMap<&'static str, HashMap<String, u64>>,
    store: &mut CutexSessionStore,
) -> anyhow::Result<()> {
    let revs = all_revs.entry(M::NAME).or_default();
    let since = since.map_or(-1, |since| since as i64);
    let mut statement = connection.prepare_cached(
        "SELECT key, rev, CASE WHEN rev > ?2 THEN json END FROM receipts WHERE map = ?1",
    )?;
    let mut rows = statement.query(params![M::NAME, since])?;
    let mut seen = HashSet::with_capacity(revs.len());
    let map = M::map_mut(store);
    while let Some(row) = rows.next()? {
        let key: String = row.get(0)?;
        let rev = as_rev(row.get(1)?)?;
        match row.get::<_, Option<String>>(2)? {
            Some(json) => {
                parsed_row();
                let value: M::Value = serde_json::from_str(&json)
                    .with_context(|| format!("failed to parse cutex {} receipt {key}", M::NAME))?;
                map.insert(key.clone(), value);
                revs.insert(key.clone(), rev);
            }
            None if revs.get(&key) == Some(&rev) => {}
            None => return Err(SnapshotGap.into()),
        }
        seen.insert(key);
    }
    if seen.len() != revs.len() {
        revs.retain(|key, _| seen.contains(key));
        map.retain(|key, _| seen.contains(key));
    }
    Ok(())
}

/// Brings `snapshot` to the revision visible to `connection` (callers hold
/// a transaction, so every table is read at the same revision).
fn refresh(connection: &Connection, snapshot: Option<Snapshot>) -> anyhow::Result<Snapshot> {
    let revision = read_revision(connection)?;
    if let Some(snapshot) = snapshot {
        if snapshot.revision == revision {
            return Ok(snapshot);
        }
        if snapshot.revision < revision {
            let Snapshot {
                revision: since,
                mut session_revs,
                mut receipt_revs,
                mut store,
            } = snapshot;
            let patched = (|| {
                let target = Arc::make_mut(&mut store);
                refresh_sessions(connection, Some(since), &mut session_revs, target)?;
                each_receipt_map!(refresh_map(
                    connection,
                    Some(since),
                    &mut receipt_revs,
                    target
                ));
                target.store_revision.set(revision);
                anyhow::Ok(())
            })();
            match patched {
                Ok(()) => {
                    return Ok(Snapshot {
                        revision,
                        session_revs,
                        receipt_revs,
                        store,
                    })
                }
                Err(error) if error.downcast_ref::<SnapshotGap>().is_some() => {}
                Err(error) => return Err(error),
            }
        }
        // Revision went backwards (database restored) or a row is missing:
        // rebuild.
    }
    #[cfg(test)]
    FULL_READS.with(|count| count.set(count.get() + 1));
    let mut store = CutexSessionStore::default();
    let mut session_revs = HashMap::new();
    let mut receipt_revs = HashMap::new();
    refresh_sessions(connection, None, &mut session_revs, &mut store)?;
    each_receipt_map!(refresh_map(connection, None, &mut receipt_revs, &mut store));
    store.store_revision.set(revision);
    Ok(Snapshot {
        revision,
        session_revs,
        receipt_revs,
        store: Arc::new(store),
    })
}

fn take_cached(db: &Path) -> Option<Snapshot> {
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let index = cache.iter().position(|(path, _)| path == db)?;
    Some(cache.swap_remove(index).1)
}

fn put_cached(db: &Path, snapshot: Snapshot) {
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cache.retain(|(path, _)| path != db);
    if cache.len() >= MAX_CACHED_PATHS {
        cache.remove(0);
    }
    cache.push((db.to_path_buf(), snapshot));
}

/// Serialises snapshot refreshes per process (one parse per revision even
/// with many concurrent readers). Never held while a caller reads a
/// snapshot.
static REFRESH: Mutex<()> = Mutex::new(());

/// The store at `json_path` as a shared snapshot of its latest revision.
pub(super) fn shared(json_path: &Path) -> anyhow::Result<Arc<CutexSessionStore>> {
    let Some(db) = ensure_ready(json_path, false, false)? else {
        return Ok(Arc::new(CutexSessionStore::default()));
    };
    let _refresh = REFRESH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    with_connection(&db, |connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let snapshot = refresh(&transaction, take_cached(&db))?;
        transaction.finish()?;
        let store = Arc::clone(&snapshot.store);
        put_cached(&db, snapshot);
        Ok(store)
    })
}

/// Forgets what this process knows about `db` (as if it had just started).
#[cfg(test)]
pub(super) fn forget_process_state(db: &Path) {
    CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|(path, _)| path != db);
    READY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|path| path != db);
    drop_pooled(db);
}

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// Writing: compare-and-swap on the store revision, write changed rows only.

#[derive(Default)]
struct Changes {
    sessions: Vec<String>,
    receipts: Vec<(&'static str, String)>,
    /// Row JSON bytes written (measurements).
    bytes: u64,
}

fn session_columns(record: &CutexSessionRecord) -> anyhow::Result<String> {
    Ok(match serde_json::to_value(record.archive_state)? {
        serde_json::Value::String(state) => state,
        other => other.to_string(),
    })
}

fn write_sessions(
    connection: &Connection,
    before: &CutexSessionStore,
    after: &CutexSessionStore,
    rev: i64,
    changes: &mut Changes,
) -> anyhow::Result<()> {
    let mut upsert = connection.prepare_cached(
        "INSERT INTO sessions (key, cutex_session_id, codex_session_id, formal_agent_name, archive_state, rev, record_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(key) DO UPDATE SET
           cutex_session_id = excluded.cutex_session_id,
           codex_session_id = excluded.codex_session_id,
           formal_agent_name = excluded.formal_agent_name,
           archive_state = excluded.archive_state,
           rev = excluded.rev,
           record_json = excluded.record_json",
    )?;
    for (key, record) in &after.sessions {
        if before.sessions.get(key) == Some(record) {
            continue;
        }
        let json = serde_json::to_string(record)
            .with_context(|| format!("failed to serialize cutex session {key}"))?;
        upsert.execute(params![
            key,
            record.cutex_session_id,
            record.codex_session_id,
            record.formal_agent_name,
            session_columns(record)?,
            rev,
            json
        ])?;
        changes.bytes += json.len() as u64;
        changes.sessions.push(key.clone());
    }
    let mut delete = connection.prepare_cached("DELETE FROM sessions WHERE key = ?1")?;
    for key in before.sessions.keys() {
        if !after.sessions.contains_key(key) {
            delete.execute(params![key])?;
            changes.sessions.push(key.clone());
        }
    }
    Ok(())
}

fn write_map<M: ReceiptMap>(
    connection: &Connection,
    before: &CutexSessionStore,
    after: &CutexSessionStore,
    rev: i64,
    changes: &mut Changes,
) -> anyhow::Result<()> {
    let old = M::map(before);
    let new = M::map(after);
    let mut upsert = connection.prepare_cached(
        "INSERT INTO receipts (map, key, rev, json) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(map, key) DO UPDATE SET rev = excluded.rev, json = excluded.json",
    )?;
    for (key, value) in new {
        if old.get(key) == Some(value) {
            continue;
        }
        let json = serde_json::to_string(value)
            .with_context(|| format!("failed to serialize cutex {} receipt {key}", M::NAME))?;
        upsert.execute(params![M::NAME, key, rev, json])?;
        changes.bytes += json.len() as u64;
        changes.receipts.push((M::NAME, key.clone()));
    }
    let mut delete =
        connection.prepare_cached("DELETE FROM receipts WHERE map = ?1 AND key = ?2")?;
    for key in old.keys() {
        if !new.contains_key(key) {
            delete.execute(params![M::NAME, key])?;
            changes.receipts.push((M::NAME, key.clone()));
        }
    }
    Ok(())
}

fn patch_map<M: ReceiptMap>(
    source: &CutexSessionStore,
    target: &mut CutexSessionStore,
    keys: &[(&'static str, String)],
    revs: &mut HashMap<&'static str, HashMap<String, u64>>,
    revision: u64,
) -> anyhow::Result<()> {
    let revs = revs.entry(M::NAME).or_default();
    for (_, key) in keys.iter().filter(|(map, _)| *map == M::NAME) {
        match M::map(source).get(key) {
            Some(value) => {
                M::map_mut(target).insert(key.clone(), value.clone());
                revs.insert(key.clone(), revision);
            }
            None => {
                M::map_mut(target).remove(key);
                revs.remove(key);
            }
        }
    }
    Ok(())
}

/// Applies a commit to the cached snapshot it was diffed against, copying
/// the committed values (no re-parse).
fn patch_snapshot(
    mut snapshot: Snapshot,
    source: &CutexSessionStore,
    changes: &Changes,
    revision: u64,
) -> anyhow::Result<Snapshot> {
    let target = Arc::make_mut(&mut snapshot.store);
    for key in &changes.sessions {
        match source.sessions.get(key) {
            Some(record) => {
                target.sessions.insert(key.clone(), record.clone());
                snapshot.session_revs.insert(key.clone(), revision);
            }
            None => {
                target.sessions.remove(key);
                snapshot.session_revs.remove(key);
            }
        }
    }
    each_receipt_map!(patch_map(
        source,
        target,
        &changes.receipts,
        &mut snapshot.receipt_revs,
        revision
    ));
    target.store_revision.set(revision);
    snapshot.revision = revision;
    Ok(snapshot)
}

/// Bytes of row JSON written by the last commits, for measurements.
pub static WRITTEN_ROW_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Source location of the writer that commits (for the commit trace).
pub(super) type Site = &'static std::panic::Location<'static>;

/// Commits `store` if the database is still at `store.store_revision`
/// (otherwise [`CutexSessionStoreRevisionConflict`]); on success the
/// store's revision is the new one. The caller holds the store file lock.
pub(super) fn commit_locked(
    json_path: &Path,
    store: &CutexSessionStore,
    site: Site,
) -> anyhow::Result<()> {
    let started = std::time::Instant::now();
    let db = ensure_ready(json_path, true, true)?.context("cutex session database missing")?;
    let expected = store.store_revision.get();
    let _refresh = REFRESH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let next = with_connection(&db, |connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = read_revision(&transaction)?;
        if current != expected {
            return Err(CutexSessionStoreRevisionConflict {
                expected,
                actual: current,
            }
            .into());
        }
        let next = current
            .checked_add(1)
            .context("cutex session store revision overflow")?;
        let rev = i64::try_from(next).context("cutex session store revision overflow")?;
        // The database is at `current`, so the snapshot at `current` is
        // exactly what is on disk: diff against it in memory.
        let snapshot = refresh(&transaction, take_cached(&db))?;
        let mut changes = Changes::default();
        write_sessions(&transaction, &snapshot.store, store, rev, &mut changes)?;
        each_receipt_map!(write_map(
            &transaction,
            &snapshot.store,
            store,
            rev,
            &mut changes
        ));
        transaction.execute(
            "UPDATE meta SET value = ?1 WHERE key = 'store_revision'",
            params![next.to_string()],
        )?;
        transaction.commit()?;
        WRITTEN_ROW_BYTES.fetch_add(changes.bytes, std::sync::atomic::Ordering::Relaxed);
        if let Ok(patched) = patch_snapshot(snapshot, store, &changes, next) {
            put_cached(&db, patched);
        }
        Ok(next)
    })?;
    store.store_revision.set(next);
    trace_commit("whole store", site, started);
    Ok(())
}

// ---------------------------------------------------------------------------
// Row-level writes (session store Phase 2b): only the touched rows are
// compared and written; nothing clones or diffs the whole store.

/// Rows a row-level writer touched: the new values live in `values` (a store
/// holding only those rows); a touched key missing from `values` is a delete.
#[derive(Default)]
pub(super) struct TouchedRows {
    pub values: CutexSessionStore,
    pub sessions: std::collections::BTreeSet<String>,
    pub receipts: std::collections::BTreeSet<(&'static str, String)>,
}

impl TouchedRows {
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty() && self.receipts.is_empty()
    }
}

fn write_touched_sessions(
    connection: &Connection,
    base: &CutexSessionStore,
    touched: &TouchedRows,
    rev: i64,
    changes: &mut Changes,
) -> anyhow::Result<()> {
    let mut upsert = connection.prepare_cached(
        "INSERT INTO sessions (key, cutex_session_id, codex_session_id, formal_agent_name, archive_state, rev, record_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(key) DO UPDATE SET
           cutex_session_id = excluded.cutex_session_id,
           codex_session_id = excluded.codex_session_id,
           formal_agent_name = excluded.formal_agent_name,
           archive_state = excluded.archive_state,
           rev = excluded.rev,
           record_json = excluded.record_json",
    )?;
    let mut delete = connection.prepare_cached("DELETE FROM sessions WHERE key = ?1")?;
    for key in &touched.sessions {
        let new = touched.values.sessions.get(key);
        if base.sessions.get(key) == new {
            continue;
        }
        match new {
            Some(record) => {
                let json = serde_json::to_string(record)
                    .with_context(|| format!("failed to serialize cutex session {key}"))?;
                upsert.execute(params![
                    key,
                    record.cutex_session_id,
                    record.codex_session_id,
                    record.formal_agent_name,
                    session_columns(record)?,
                    rev,
                    json
                ])?;
                changes.bytes += json.len() as u64;
            }
            None => {
                delete.execute(params![key])?;
            }
        }
        changes.sessions.push(key.clone());
    }
    Ok(())
}

fn write_touched_map<M: ReceiptMap>(
    connection: &Connection,
    base: &CutexSessionStore,
    touched: &TouchedRows,
    rev: i64,
    changes: &mut Changes,
) -> anyhow::Result<()> {
    let mut upsert = connection.prepare_cached(
        "INSERT INTO receipts (map, key, rev, json) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(map, key) DO UPDATE SET rev = excluded.rev, json = excluded.json",
    )?;
    let mut delete =
        connection.prepare_cached("DELETE FROM receipts WHERE map = ?1 AND key = ?2")?;
    for (_, key) in touched.receipts.iter().filter(|(map, _)| *map == M::NAME) {
        let new = M::map(&touched.values).get(key);
        if M::map(base).get(key) == new {
            continue;
        }
        match new {
            Some(value) => {
                let json = serde_json::to_string(value).with_context(|| {
                    format!("failed to serialize cutex {} receipt {key}", M::NAME)
                })?;
                upsert.execute(params![M::NAME, key, rev, json])?;
                changes.bytes += json.len() as u64;
            }
            None => {
                delete.execute(params![M::NAME, key])?;
            }
        }
        changes.receipts.push((M::NAME, key.clone()));
    }
    Ok(())
}

/// Commits `touched` if the database is still at `expected` (otherwise
/// [`CutexSessionStoreRevisionConflict`]). Rows equal to what is stored are
/// skipped; when nothing differs nothing is written and the revision stays
/// `expected`. Otherwise one `BEGIN IMMEDIATE` transaction writes the rows
/// stamped `rev = expected + 1` and bumps `store_revision`.
///
/// `base` is the caller's snapshot at `expected` (exactly what is on disk
/// then). It is released before the process cache is patched, so the patch
/// does not copy the whole store, and replaced by the snapshot at the
/// returned revision. The caller holds the store file lock.
pub(super) fn commit_rows_locked(
    json_path: &Path,
    expected: u64,
    base: &mut Option<Arc<CutexSessionStore>>,
    touched: &TouchedRows,
    site: Site,
) -> anyhow::Result<u64> {
    let db = ensure_ready(json_path, true, true)?.context("cutex session database missing")?;
    let started = std::time::Instant::now();
    let _refresh = REFRESH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let snapshot = base.as_ref().context("row snapshot missing")?;
    let outcome = with_connection(&db, |connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = read_revision(&transaction)?;
        if current != expected {
            return Err(CutexSessionStoreRevisionConflict {
                expected,
                actual: current,
            }
            .into());
        }
        let next = current
            .checked_add(1)
            .context("cutex session store revision overflow")?;
        let rev = i64::try_from(next).context("cutex session store revision overflow")?;
        let mut changes = Changes::default();
        write_touched_sessions(&transaction, snapshot, touched, rev, &mut changes)?;
        each_receipt_map!(write_touched_map(
            &transaction,
            snapshot,
            touched,
            rev,
            &mut changes
        ));
        if changes.sessions.is_empty() && changes.receipts.is_empty() {
            transaction.finish()?;
            return Ok(None);
        }
        transaction.execute(
            "UPDATE meta SET value = ?1 WHERE key = 'store_revision'",
            params![next.to_string()],
        )?;
        transaction.commit()?;
        WRITTEN_ROW_BYTES.fetch_add(changes.bytes, std::sync::atomic::Ordering::Relaxed);
        Ok(Some((next, changes)))
    })?;
    let Some((next, changes)) = outcome else {
        trace_commit("rows, unchanged", site, started);
        return Ok(expected);
    };
    // Patch the process cache in place: drop our reference first so the
    // cached `Arc` is unique (unless a reader still holds it).
    *base = None;
    match take_cached(&db) {
        Some(cached) if cached.revision == expected => {
            if let Ok(patched) = patch_snapshot(cached, &touched.values, &changes, next) {
                *base = Some(Arc::clone(&patched.store));
                put_cached(&db, patched);
            }
        }
        other => {
            // The cache moved on or was evicted (should not happen under the
            // store lock): read the committed revision the normal way.
            if let Some(other) = other {
                put_cached(&db, other);
            }
        }
    }
    drop(_refresh);
    if base.is_none() {
        *base = Some(shared(json_path)?);
    }
    trace_commit("rows", site, started);
    Ok(next)
}

/// One session row read straight from the database (no snapshot refresh):
/// `(key, record)` of the row with this key, or else of the row whose
/// `cutex_session_id` is `id`.
pub(super) fn read_session_row(
    json_path: &Path,
    key_or_id: &str,
) -> anyhow::Result<Option<(String, CutexSessionRecord)>> {
    let Some(db) = ensure_ready(json_path, false, false)? else {
        return Ok(None);
    };
    with_connection(&db, |connection| {
        let mut statement = connection.prepare_cached(
            "SELECT key, record_json FROM sessions WHERE key = ?1
             UNION ALL
             SELECT key, record_json FROM sessions WHERE cutex_session_id = ?1 AND key <> ?1
             LIMIT 1",
        )?;
        let mut rows = statement.query(params![key_or_id])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        let key: String = row.get(0)?;
        let json: String = row.get(1)?;
        parsed_row();
        let record = serde_json::from_str(&json)
            .with_context(|| format!("failed to parse cutex session row {key}"))?;
        Ok(Some((key, record)))
    })
}

/// `CUTEX_SESSION_STORE_TRACE=1`: one stderr line per commit (kind and
/// duration), to see which writers run on an idle service.
fn trace_commit(kind: &str, site: Site, started: std::time::Instant) {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ENABLED.get_or_init(|| std::env::var_os("CUTEX_SESSION_STORE_TRACE").is_some()) {
        eprintln!(
            "cutex: session store commit ({kind}) at {site}: {:.2} ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
    }
}

// ---------------------------------------------------------------------------
// Opening, import and sentinel.

static READY: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

fn is_ready(db: &Path) -> bool {
    READY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .any(|path| path == db)
}

fn mark_ready(db: &Path) {
    let mut ready = READY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !ready.iter().any(|path| path == db) {
        ready.push(db.to_path_buf());
    }
}

/// What lives at the old JSON path.
enum LegacyFile {
    Missing,
    Sentinel,
    Store(Vec<u8>),
}

fn read_legacy(json_path: &Path) -> anyhow::Result<LegacyFile> {
    let bytes = match fs::read(json_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(LegacyFile::Missing),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "Failed to read cutex session store: {}",
                    json_path.display()
                )
            })
        }
    };
    if is_sentinel(&bytes) {
        return Ok(LegacyFile::Sentinel);
    }
    Ok(LegacyFile::Store(bytes))
}

pub fn is_sentinel(bytes: &[u8]) -> bool {
    // A sentinel is small; never parse a whole store to find out.
    bytes.len() < 64 * 1024
        && serde_json::from_slice::<serde_json::Value>(bytes)
            .ok()
            .and_then(|value| {
                value
                    .get("cutexSessionStore")
                    .and_then(|marker| marker.as_str().map(|marker| marker == SENTINEL_MARKER))
            })
            .unwrap_or(false)
}

/// What [`ensure_ready`] did (tests and the import log line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenOutcome {
    /// Already open in this process or nothing to do.
    Ready,
    /// No store anywhere and nothing to create.
    Absent,
    /// A new, empty database.
    Created,
    /// The JSON store was imported.
    Imported,
    /// An import that crashed after its database was in place was finished
    /// (JSON renamed and / or sentinel written).
    Finished,
}

/// Makes sure the database for `json_path` exists and the old JSON store is
/// imported. Returns the database path, or `None` when there is no store and
/// `create` is false. `lock_held`: the caller already holds the store file
/// lock (taking it again from the same process would deadlock).
pub(super) fn ensure_ready(
    json_path: &Path,
    create: bool,
    lock_held: bool,
) -> anyhow::Result<Option<PathBuf>> {
    ensure_ready_outcome(json_path, create, lock_held).map(|(db, _)| db)
}

pub(super) fn ensure_ready_outcome(
    json_path: &Path,
    create: bool,
    lock_held: bool,
) -> anyhow::Result<(Option<PathBuf>, OpenOutcome)> {
    // Test processes never open (let alone import) the real `~/.cutex` store.
    crate::config::paths::ensure_not_real_state(json_path)?;
    let db = database_path(json_path);
    if is_ready(&db) {
        return Ok((Some(db), OpenOutcome::Ready));
    }
    // Fast path without the lock: an imported home with its sentinel.
    if db.is_file() && matches!(read_legacy(json_path)?, LegacyFile::Sentinel) {
        open_existing(&db)?;
        mark_ready(&db);
        return Ok((Some(db), OpenOutcome::Ready));
    }
    if !db.exists() && !create && matches!(read_legacy(json_path)?, LegacyFile::Missing) {
        return Ok((None, OpenOutcome::Absent));
    }
    let parent = json_path
        .parent()
        .context("cutex session store path has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "Failed to create cutex session store directory: {}",
            parent.display()
        )
    })?;
    let lock = if lock_held {
        None
    } else {
        let lock = super::open_store_lock(&parent.join(super::CUTEX_SESSIONS_LOCK_FILE))?;
        FileExt::lock_exclusive(&lock).context("Failed to lock cutex session store")?;
        Some(lock)
    };
    let result = ensure_ready_locked(json_path, &db, create);
    if let Some(lock) = lock {
        let _ = FileExt::unlock(&lock);
    }
    let outcome = result?;
    if outcome != OpenOutcome::Absent {
        mark_ready(&db);
        Ok((Some(db), outcome))
    } else {
        Ok((None, outcome))
    }
}

fn ensure_ready_locked(json_path: &Path, db: &Path, create: bool) -> anyhow::Result<OpenOutcome> {
    remove_stale_imports(db)?;
    let legacy = read_legacy(json_path)?;
    if db.is_file() {
        let connection = open_existing(db)?;
        let meta = read_meta(&connection)?;
        drop(connection);
        return match legacy {
            LegacyFile::Sentinel => Ok(OpenOutcome::Ready),
            LegacyFile::Missing => {
                // Crashed between renaming the JSON and writing the sentinel
                // (or a fresh home whose sentinel was removed).
                write_sentinel(json_path, db, meta.get("imported_from").map(String::as_str))?;
                Ok(OpenOutcome::Finished)
            }
            LegacyFile::Store(bytes) => {
                let sha = sha256_hex(&bytes);
                let imported_sha = meta.get("imported_sha256").map(String::as_str);
                if imported_sha == Some(sha.as_str()) {
                    // Crashed after the database was renamed into place.
                    finish_import(json_path, db, meta.get("imported_from").map(String::as_str))?;
                    return Ok(OpenOutcome::Finished);
                }
                let untouched = meta.get("imported_revision").is_some()
                    && meta.get("imported_revision") == meta.get("store_revision");
                if untouched {
                    // The database was never written after its import and the
                    // JSON changed since (an older cutex ran before the import
                    // was finished): the JSON is the newer truth. Keep the
                    // stale database aside and import again.
                    let aside = unique_sibling(db, &format!("superseded-{}", timestamp()))?;
                    drop_pooled(db);
                    rename_database(db, &aside)?;
                    eprintln!(
                        "cutex: session database {} was never written and {} changed since its import; moved it to {} and importing again",
                        db.display(),
                        json_path.display(),
                        aside.display()
                    );
                    import(json_path, db, &bytes)?;
                    return Ok(OpenOutcome::Imported);
                }
                bail!(
                    "cutex session store conflict: {} exists, but {} is a session store again (not the sentinel) and differs from what was imported. \
                     An older cutex probably ran on this home after the SQLite import. Stop all cutex processes and repair by hand \
                     (docs: session store operator notes); nothing was changed.",
                    db.display(),
                    json_path.display()
                )
            }
        };
    }
    match legacy {
        LegacyFile::Sentinel => bail!(
            "cutex session database {} is missing but {} says the store was migrated to it. \
             Restore the database from a backup (the pre-SQLite JSON {}.pre-sqlite-* is an older read-only reference); \
             refusing to start an empty store.",
            db.display(),
            json_path.display(),
            json_path.display()
        ),
        LegacyFile::Store(bytes) => {
            import(json_path, db, &bytes)?;
            Ok(OpenOutcome::Imported)
        }
        LegacyFile::Missing if create => {
            create_empty(db)?;
            write_sentinel(json_path, db, None)?;
            Ok(OpenOutcome::Created)
        }
        LegacyFile::Missing => Ok(OpenOutcome::Absent),
    }
}

fn read_meta(connection: &Connection) -> anyhow::Result<HashMap<String, String>> {
    let mut statement = connection.prepare("SELECT key, value FROM meta")?;
    let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    let mut meta = HashMap::new();
    for row in rows {
        let (key, value): (String, String) = row?;
        meta.insert(key, value);
    }
    Ok(meta)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = sha2::Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn timestamp() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

fn file_name(path: &Path) -> anyhow::Result<String> {
    Ok(path
        .file_name()
        .context("path has no file name")?
        .to_string_lossy()
        .into_owned())
}

/// `<path>.<suffix>`, or with `-2`, `-3`, … appended when that exists.
fn unique_sibling(path: &Path, suffix: &str) -> anyhow::Result<PathBuf> {
    let base = format!("{}.{suffix}", file_name(path)?);
    let parent = path.parent().context("path has no parent")?;
    let mut candidate = parent.join(&base);
    let mut counter = 2;
    while candidate.exists() {
        candidate = parent.join(format!("{base}-{counter}"));
        counter += 1;
    }
    Ok(candidate)
}

fn remove_stale_imports(db: &Path) -> anyhow::Result<()> {
    let prefix = format!("{}{IMPORTING_INFIX}", file_name(db)?);
    let parent = db.parent().context("database path has no parent")?;
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            // Only importers write these, and they hold the store lock we
            // hold now: whatever is left is from a crashed import.
            fs::remove_file(entry.path()).with_context(|| {
                format!(
                    "failed to remove stale session import {}",
                    entry.path().display()
                )
            })?;
        }
    }
    Ok(())
}

fn create_private_file(path: &Path) -> anyhow::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    Ok(())
}

/// Owner-only database and sidecars on Unix. On Windows they inherit the
/// owner-private ACL that cutex already requires of its home directory.
fn secure_database_files(db: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for suffix in ["", "-wal", "-shm"] {
            let mut name = db.as_os_str().to_owned();
            name.push(suffix);
            let path = PathBuf::from(name);
            if path.exists() {
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            }
        }
    }
    #[cfg(not(unix))]
    let _ = db;
    Ok(())
}

fn sync_parent(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn rename_database(from: &Path, to: &Path) -> anyhow::Result<()> {
    fs::rename(from, to)
        .with_context(|| format!("failed to rename {} to {}", from.display(), to.display()))?;
    for suffix in ["-wal", "-shm"] {
        let mut source = from.as_os_str().to_owned();
        source.push(suffix);
        let source = PathBuf::from(source);
        if source.exists() {
            let mut target = to.as_os_str().to_owned();
            target.push(suffix);
            fs::rename(&source, PathBuf::from(target))?;
        }
    }
    sync_parent(to)
}

fn initialise(
    connection: &Connection,
    revision: u64,
    extra_meta: &[(&str, String)],
) -> anyhow::Result<()> {
    connection.execute_batch(SCHEMA)?;
    connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    let mut insert = connection.prepare("INSERT INTO meta (key, value) VALUES (?1, ?2)")?;
    insert.execute(params!["schema_version", SCHEMA_VERSION.to_string()])?;
    insert.execute(params!["store_revision", revision.to_string()])?;
    insert.execute(params!["created_at", chrono::Utc::now().to_rfc3339()])?;
    for (key, value) in extra_meta {
        insert.execute(params![key, value])?;
    }
    Ok(())
}

/// Builds a database in a private temporary file next to `db` and renames it
/// into place only once complete (a crash leaves only the temporary file).
fn build_database(
    db: &Path,
    build: impl FnOnce(&mut Connection) -> anyhow::Result<()>,
    verify: impl FnOnce(&Connection) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let temp = db.with_file_name(format!(
        "{}{IMPORTING_INFIX}{}-{}",
        file_name(db)?,
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| {
        create_private_file(&temp)?;
        {
            let mut connection = open_connection(&temp, false)?;
            // Built in one transaction; WAL is switched on by every later
            // open (the mode is stored in the file header).
            connection.pragma_update(None, "journal_mode", "DELETE")?;
            build(&mut connection)?;
            verify(&connection)?;
            let integrity: String =
                connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
            anyhow::ensure!(
                integrity == "ok",
                "session database integrity check failed: {integrity}"
            );
            connection.pragma_update(None, "journal_mode", "WAL")?;
            connection
                .close()
                .map_err(|(_, error)| error)
                .context("failed to close new session database")?;
        }
        fault(db, "before-rename")?;
        fs::OpenOptions::new().write(true).open(&temp)?.sync_all()?;
        secure_database_files(&temp)?;
        fs::rename(&temp, db)
            .with_context(|| format!("failed to move new session database to {}", db.display()))?;
        sync_parent(db)?;
        Ok(())
    })();
    if result.is_err() {
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let mut name = temp.as_os_str().to_owned();
            name.push(suffix);
            let _ = fs::remove_file(PathBuf::from(name));
        }
    }
    result
}

fn create_empty(db: &Path) -> anyhow::Result<()> {
    build_database(
        db,
        |connection| {
            let transaction = connection.transaction()?;
            initialise(&transaction, 0, &[])?;
            transaction.commit()?;
            Ok(())
        },
        |_| Ok(()),
    )
}

fn insert_all(connection: &Connection, store: &CutexSessionStore, rev: i64) -> anyhow::Result<()> {
    let mut changes = Changes::default();
    let empty = CutexSessionStore::default();
    write_sessions(connection, &empty, store, rev, &mut changes)?;
    each_receipt_map!(write_map(connection, &empty, store, rev, &mut changes));
    Ok(())
}

fn verify_map<M: ReceiptMap>(
    connection: &Connection,
    original: &CutexSessionStore,
    imported: &CutexSessionStore,
) -> anyhow::Result<()> {
    let rows: i64 = connection.query_row(
        "SELECT COUNT(*) FROM receipts WHERE map = ?1",
        params![M::NAME],
        |row| row.get(0),
    )?;
    anyhow::ensure!(
        rows as usize == M::map(original).len() && M::map(imported) == M::map(original),
        "session import verification failed for {} receipts ({} rows, {} expected)",
        M::NAME,
        rows,
        M::map(original).len()
    );
    Ok(())
}

fn import(json_path: &Path, db: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    crate::config::paths::ensure_not_real_state(json_path)?;
    crate::config::paths::ensure_not_real_state(db)?;
    let started = std::time::Instant::now();
    let store: CutexSessionStore = serde_json::from_slice(bytes).with_context(|| {
        format!(
            "Failed to parse cutex session store for the SQLite import: {} (left unchanged)",
            json_path.display()
        )
    })?;
    let sha = sha256_hex(bytes);
    let revision = store.store_revision.get();
    let reference = pre_sqlite_name(json_path)?;
    let receipts = store.native_stop_receipts.len()
        + store.explicit_launch_receipts.len()
        + store.human_adoption_receipts.len()
        + store.agent_archive_receipts.len()
        + store.formal_name_receipts.len()
        + store.explicit_launch_tombstones.len();
    let meta = [
        ("imported_at", chrono::Utc::now().to_rfc3339()),
        ("imported_from", file_name(&reference)?),
        ("imported_sha256", sha.clone()),
        ("imported_bytes", bytes.len().to_string()),
        ("imported_revision", revision.to_string()),
        ("imported_sessions", store.sessions.len().to_string()),
        ("imported_receipts", receipts.to_string()),
    ];
    let rev = i64::try_from(revision).context("cutex session store revision overflow")?;
    build_database(
        db,
        |connection| {
            let transaction = connection.transaction()?;
            initialise(&transaction, revision, &meta)?;
            insert_all(&transaction, &store, rev)?;
            transaction.commit()?;
            fault(db, "after-insert")?;
            Ok(())
        },
        |connection| {
            // Read everything back the way the store is read and compare.
            let imported = refresh(connection, None)?.store;
            anyhow::ensure!(
                imported.store_revision.get() == revision,
                "session import verification failed: revision"
            );
            let rows: i64 =
                connection.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?;
            anyhow::ensure!(
                rows as usize == store.sessions.len() && imported.sessions == store.sessions,
                "session import verification failed for sessions ({} rows, {} expected)",
                rows,
                store.sessions.len()
            );
            each_receipt_map!(verify_map(connection, &store, &imported));
            fault(db, "verify")?;
            Ok(())
        },
    )?;
    finish_import(json_path, db, Some(&file_name(&reference)?))?;
    eprintln!(
        "cutex: imported session store {} into {} ({} bytes, {} sessions, {} receipts, revision {}, {} ms); original kept as {}",
        json_path.display(),
        db.display(),
        bytes.len(),
        store.sessions.len(),
        receipts,
        revision,
        started.elapsed().as_millis(),
        reference.display()
    );
    Ok(())
}

/// `<name>.pre-sqlite-<YYYYMMDD>` (UTC date), never an existing file.
fn pre_sqlite_name(json_path: &Path) -> anyhow::Result<PathBuf> {
    unique_sibling(
        json_path,
        &format!("pre-sqlite-{}", chrono::Utc::now().format("%Y%m%d")),
    )
}

/// Renames the imported JSON to its read-only reference name and writes the
/// sentinel in its place.
fn finish_import(json_path: &Path, db: &Path, reference: Option<&str>) -> anyhow::Result<()> {
    fault(db, "before-json-rename")?;
    let parent = json_path.parent().context("store path has no parent")?;
    let target = match reference.map(|name| parent.join(name)) {
        Some(target) if !target.exists() => target,
        _ => pre_sqlite_name(json_path)?,
    };
    fs::rename(json_path, &target).with_context(|| {
        format!(
            "failed to rename {} to {}",
            json_path.display(),
            target.display()
        )
    })?;
    sync_parent(&target)?;
    fault(db, "before-sentinel")?;
    write_sentinel(json_path, db, Some(&file_name(&target)?))
}

/// The sentinel left at the old JSON path. Older cutex parses
/// `storeRevision` as an unsigned number, fails on this string and reports
/// it (`Failed to parse cutex session store: …`) instead of treating the
/// home as empty; its writers parse the current file first, so they fail
/// too and never replace the sentinel.
pub fn sentinel_bytes(db: &Path, reference: Option<&str>) -> anyhow::Result<Vec<u8>> {
    let database = file_name(db)?;
    let mut message = format!(
        "MIGRATED TO SQLITE: this home's session store is {database}; this cutex is too old for it. Do not run it on this home."
    );
    if let Some(reference) = reference {
        message.push_str(&format!(
            " The pre-SQLite store is kept read-only as {reference}."
        ));
    }
    let value = serde_json::json!({
        "storeRevision": message,
        "sessions": message,
        "cutexSessionStore": SENTINEL_MARKER,
        "database": database,
        "preSqliteReference": reference,
        "migratedAt": chrono::Utc::now().to_rfc3339(),
    });
    let mut bytes = serde_json::to_vec_pretty(&value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn write_sentinel(json_path: &Path, db: &Path, reference: Option<&str>) -> anyhow::Result<()> {
    crate::config::atomic::write_private_bytes_atomic(json_path, &sentinel_bytes(db, reference)?)
}

// ---------------------------------------------------------------------------
// Fault injection for crash tests.

#[cfg(test)]
pub(super) static FAULTS: Mutex<Vec<(PathBuf, &'static str)>> = Mutex::new(Vec::new());

#[cfg(test)]
fn fault(db: &Path, point: &'static str) -> anyhow::Result<()> {
    let faults = FAULTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if faults
        .iter()
        .any(|(path, armed)| path == db && *armed == point)
    {
        bail!("injected fault at {point}");
    }
    Ok(())
}

#[cfg(not(test))]
fn fault(_db: &Path, _point: &'static str) -> anyhow::Result<()> {
    Ok(())
}
