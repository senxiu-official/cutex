//! Row-oriented SQLite backing for the large management stores.
//!
//! Business modules retain their own cross-process lock.  This layer adds an
//! SQLite generation CAS and an immediate transaction, so independent store
//! locks may safely share one WAL database.

#[path = "bus_changes.rs"]
pub(crate) mod bus_changes;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::Context;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use sha2::{Digest, Sha256};

pub const DATABASE_FILE: &str = "management-state.sqlite3";
const FORMAT_VERSION: i64 = 1;
static CACHED_READ_CONNECTION: OnceLock<Mutex<Option<(PathBuf, Connection)>>> = OnceLock::new();

#[cfg(test)]
thread_local! {
    // Thread-local and consumed once: only the test's selected read can pause.
    pub(crate) static READ_SNAPSHOT_TEST_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    pub(crate) static PRESENTATION_SEARCHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub type RowKey = (String, String);
pub type Rows = BTreeMap<RowKey, Vec<u8>>;

#[derive(Debug)]
pub struct LoadedRows {
    pub generation: u64,
    pub rows: Rows,
    pub exists: bool,
}

#[derive(Debug)]
pub enum RowChange {
    Put {
        collection: String,
        key: String,
        value: Vec<u8>,
    },
    Delete {
        collection: String,
        key: String,
    },
}

fn open_connection(path: &Path) -> anyhow::Result<Connection> {
    let db = Connection::open(path)
        .with_context(|| format!("open hot management database: {}", path.display()))?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.pragma_update(None, "foreign_keys", true)?;
    Ok(db)
}

fn read_connection(path: &Path) -> anyhow::Result<Connection> {
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open hot management database read-only: {}", path.display()))?;
    db.busy_timeout(Duration::from_secs(5))?;
    Ok(db)
}

fn connection(path: &Path) -> anyhow::Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let db = open_connection(path)?;
    db.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=FULL;
         CREATE TABLE IF NOT EXISTS hot_store_meta (
           store TEXT PRIMARY KEY,
           format_version INTEGER NOT NULL,
           generation INTEGER NOT NULL,
           migrated_at TEXT,
           legacy_sha256 TEXT,
           verified INTEGER NOT NULL CHECK (verified IN (0, 1))
         );
         CREATE TABLE IF NOT EXISTS hot_store_rows (
           store TEXT NOT NULL,
           collection TEXT NOT NULL,
           row_key TEXT NOT NULL,
           value_json BLOB NOT NULL,
           PRIMARY KEY (store, collection, row_key),
           FOREIGN KEY (store) REFERENCES hot_store_meta(store) ON DELETE CASCADE
         ) WITHOUT ROWID;",
    )?;
    // The expression is maintained by SQLite in the same transaction as each
    // message write, including writes by older binaries. No second projection
    // or invalidation protocol can fall behind the authoritative row.
    db.execute_batch(
        "CREATE INDEX IF NOT EXISTS agent_bus_message_state ON hot_store_rows
         (json_extract(CAST(value_json AS TEXT), '$.snapshot.state'), row_key)
         WHERE store = 'agent_bus' AND collection = 'message';",
    )?;
    // Open presentation obligations by recipient and next attempt (058 C1).
    // Independent of the message state: an ACKed input keeps its display work.
    db.execute_batch(&format!(
        "CREATE INDEX IF NOT EXISTS agent_bus_presentation_due ON hot_store_rows
         ({PRESENTATION_RECIPIENT}, {PRESENTATION_NEXT_ATTEMPT}, row_key)
         WHERE {PRESENTATION_OPEN};"
    ))?;
    db.execute_batch(&format!("CREATE INDEX IF NOT EXISTS agent_bus_admission_candidates
        ON hot_store_rows(row_key) WHERE {ADMISSION_CANDIDATE};"))?;
    db.execute_batch("CREATE INDEX IF NOT EXISTS agent_bus_owner_carry ON hot_store_rows
        (json_extract(CAST(value_json AS TEXT),'$.snapshot.toCutexSessionId'),
         json_extract(CAST(value_json AS TEXT),'$.ownerCarry'),row_key)
        WHERE store='agent_bus' AND collection='message' AND json_extract(CAST(value_json AS TEXT),'$.ownerCarry') IS NOT NULL;")?;
    db.execute_batch("CREATE INDEX IF NOT EXISTS agent_bus_close_request ON hot_store_rows
        (json_extract(CAST(value_json AS TEXT), '$.settlement.request.requestId'), row_key)
        WHERE store='agent_bus' AND collection='message';")?;
    db.execute_batch(&format!("CREATE INDEX IF NOT EXISTS agent_bus_management_starts ON hot_store_rows
        (json_extract(CAST(value_json AS TEXT),'$.canonicalEnvelope.controlType'), json_extract(CAST(value_json AS TEXT),'$.canonicalEnvelope.from'),row_key) WHERE {MANAGEMENT_START_ROWS};
        CREATE INDEX IF NOT EXISTS agent_bus_settlement_outbox ON hot_store_rows(row_key) WHERE {SETTLEMENT_OUTBOX_ROWS};
        CREATE INDEX IF NOT EXISTS agent_bus_finished_time ON hot_store_rows({FINISHED_TIME},row_key) WHERE {FINISHED_ROWS};"))?;
    Ok(db)
}

/// Selected owner's frozen IDs, including settled carries, without retaining history.
pub(crate) fn owner_carry_keys(db: &Connection, session: &str, owner: &str) -> anyhow::Result<Vec<String>> {
    let mut statement=db.prepare("SELECT row_key FROM hot_store_rows WHERE store='agent_bus' AND collection='message'
        AND json_extract(CAST(value_json AS TEXT),'$.ownerCarry') IS NOT NULL
        AND json_extract(CAST(value_json AS TEXT),'$.snapshot.toCutexSessionId')=?1
        AND json_extract(CAST(value_json AS TEXT),'$.ownerCarry')=?2 ORDER BY row_key")?;
    let ids=statement.query_map([session,owner],|row|row.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
    crate::store::metrics::note("bus-messages",crate::store::OpCounts {sql_statements:1,rows_visited:ids.len() as u64,..Default::default()});
    Ok(ids)
}

pub(crate) fn close_request_keys(db: &Connection, request: &str) -> anyhow::Result<Vec<String>> {
    let mut statement = db.prepare("SELECT row_key FROM hot_store_rows WHERE store='agent_bus' AND collection='message' AND json_extract(CAST(value_json AS TEXT), '$.settlement.request.requestId')=?1 ORDER BY row_key")?;
    let keys = statement
        .query_map([request], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(keys)
}

const PRESENTATION_RECIPIENT: &str =
    "json_extract(CAST(value_json AS TEXT), '$.snapshot.toCutexSessionId')";
// `nextAttemptAt` is serde-defaulted to 0 when absent.
const PRESENTATION_NEXT_ATTEMPT: &str =
    "IFNULL(json_extract(CAST(value_json AS TEXT), '$.snapshot.presentation.nextAttemptAt'), 0)";
const PRESENTATION_OPEN: &str = "store = 'agent_bus' AND collection = 'message'
         AND json_extract(CAST(value_json AS TEXT), '$.snapshot.presentation.version') IS NOT NULL
         AND json_extract(CAST(value_json AS TEXT), '$.snapshot.presentation.receipt') IS NULL";

/// The recipient's first `limit` open presentation obligations as
/// `(message id, next attempt)`, earliest first. Rows are not decoded.
pub(crate) fn presentation_due_rows(
    db: &Connection,
    recipient: &str,
    limit: usize,
) -> anyhow::Result<Vec<(String, i64)>> {
    #[cfg(test)]
    PRESENTATION_SEARCHES.with(|n| n.set(n.get() + 1));
    let indexed: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='index' AND name='agent_bus_presentation_due')",
        [], |row| row.get(0),
    )?;
    // As for bus_state_query: pin the index once a writer installed it; a
    // home no writer has opened since upgrade stays readable by a scan.
    let index = if indexed {
        " INDEXED BY agent_bus_presentation_due"
    } else {
        ""
    };
    let mut query = db.prepare(&format!(
        "SELECT row_key, {PRESENTATION_NEXT_ATTEMPT} FROM hot_store_rows{index}
         WHERE {PRESENTATION_OPEN} AND {PRESENTATION_RECIPIENT} = ?1
         ORDER BY {PRESENTATION_NEXT_ATTEMPT}, row_key LIMIT ?2"
    ))?;
    let rows = query
        .query_map(params![recipient, i64::try_from(limit)?], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

fn meta(connection: &Connection, store: &str) -> anyhow::Result<Option<(u64, bool)>> {
    connection
        .query_row(
            "SELECT format_version, generation, verified FROM hot_store_meta WHERE store = ?1",
            [store],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)? != 0,
                ))
            },
        )
        .optional()?
        .map(|(format_version, generation, verified)| {
            let archive_format = store == "asks" && format_version == crate::asks::archive::FORMAT_VERSION;
            anyhow::ensure!(
                format_version == FORMAT_VERSION || archive_format,
                "unsupported {store} hot-store format version {format_version}"
            );
            if archive_format {
                anyhow::ensure!(table_exists(connection,"asks_archive_rows")?,"asks archive rows table missing; keep services stopped and reconcile before rollback");
                let active: bool = connection.query_row(
                    "SELECT schema_version=1 AND active=1 FROM asks_archive_meta WHERE singleton=1", [], |row| row.get(0)
                ).context("asks cold archive metadata missing; keep services stopped and reconcile before rollback")?;
                anyhow::ensure!(active, "asks cold archive is not authoritative");
            }
            let generation = u64::try_from(generation).context("negative hot-store generation")?;
            Ok((generation, verified))
        })
        .transpose()
}

pub(crate) fn prepare(path: &Path) -> anyhow::Result<()> { drop(connection(path)?); Ok(()) }

pub(crate) const ADMISSION_CANDIDATE: &str = "store='agent_bus' AND collection='message'
    AND (json_extract(CAST(value_json AS TEXT),'$.snapshot.state') IN ('pending','inbox_accepted','stale_held')
         OR json_extract(CAST(value_json AS TEXT),'$.admissionStartedIdle')=1)
    AND json_extract(CAST(value_json AS TEXT),'$.settlement.closure') IS NULL";

/// Keyset page over scheduling candidates. At most the budget plus one
/// probe is visited; no completed-history payload is decoded or retained.
pub(crate) fn admission_page(
    db: &Connection,
    after: Option<&str>,
    budget: crate::store::PageBudget,
) -> anyhow::Result<(Rows, Option<String>)> {
    let mut stmt = db.prepare(&format!("SELECT collection,row_key,value_json FROM hot_store_rows INDEXED BY agent_bus_admission_candidates WHERE {ADMISSION_CANDIDATE} {} ORDER BY row_key LIMIT ?2",
        if after.is_some() { "AND row_key > ?1" } else { "AND (?1 IS NULL)" }))?;
    let mut query = stmt.query(params![
        after,
        budget.max_rows.max(1).saturating_add(1) as i64
    ])?;
    let mut rows = Rows::new();
    let mut bytes = 0;
    let mut more = false;
    while let Some(row) = query.next()? {
        let collection: String = row.get(0)?;
        let key: String = row.get(1)?;
        let value: Vec<u8> = row.get(2)?;
        crate::store::metrics::note(
            "bus-messages",
            crate::store::OpCounts {
                rows_visited: 1,
                bytes_read: value.len() as u64,
                ..Default::default()
            },
        );
        if !budget.admits(rows.len(), bytes, value.len()) {
            more = true;
            break;
        }
        bytes += value.len();
        rows.insert((collection, key), value);
    }
    let next = more.then(|| rows.keys().last().expect("page makes progress").1.clone());
    Ok((rows, next))
}

fn rows(connection: &Connection, store: &str) -> anyhow::Result<Rows> {
    let mut statement = connection.prepare("SELECT collection,row_key,value_json FROM hot_store_rows WHERE store=?1 ORDER BY collection,row_key")?;
    let rows = statement
        .query_map([store], |row| Ok(((row.get(0)?, row.get(1)?), row.get(2)?)))?
        .collect::<rusqlite::Result<Rows>>()?;
    note_bus_rows(store, rows.len(), rows.values().map(Vec::len).sum());
    Ok(rows)
}

fn note_bus_rows(logical: &str, rows: usize, bytes: usize) {
    let id = match logical {
        "agent_bus" => "bus-messages",
        "agent_bus_settlement_evidence" => "bus-settlement-evidence",
        _ => return,
    };
    crate::store::metrics::note(
        id,
        crate::store::OpCounts {
            bytes_read: bytes as u64,
            rows_visited: rows as u64,
            rows_materialized: rows as u64,
            sql_statements: 1,
            ..Default::default()
        },
    );
}

pub fn generation(path: &Path, store: &str) -> anyhow::Result<Option<u64>> {
    if !path.exists() {
        return Ok(None);
    }
    let db = read_connection(path)?;
    if !table_exists(&db, "hot_store_meta")? {
        return Ok(None);
    }
    Ok(meta(&db, store)?
        .filter(|(_, verified)| *verified)
        .map(|(generation, _)| generation))
}

/// Generation probe for the process's one default management database.
/// Reusing the read-only connection keeps warm presentation/list reads at a
/// small indexed SELECT while SQLite's WAL snapshot still observes commits
/// made by independent cutex processes.
pub fn cached_generation(path: &Path, store: &str) -> anyhow::Result<Option<u64>> {
    let mut cached = CACHED_READ_CONNECTION
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| anyhow::anyhow!("hot-store read connection lock was poisoned"))?;
    if cached
        .as_ref()
        .is_none_or(|(cached_path, _)| cached_path != path)
    {
        if !path.exists() {
            return Ok(None);
        }
        *cached = Some((path.to_path_buf(), read_connection(path)?));
    }
    let connection = &cached.as_ref().expect("connection installed above").1;
    Ok(meta(connection, store)?
        .filter(|(_, verified)| *verified)
        .map(|(generation, _)| generation))
}

/// [`verified_snapshot`] on the cached read-only connection, for probes that
/// run on every idle poll. Never creates, migrates or repairs data.
pub(crate) fn cached_snapshot<T>(
    path: &Path,
    store: &str,
    read: impl FnOnce(&Connection, u64) -> anyhow::Result<T>,
) -> anyhow::Result<Option<T>> {
    let mut cached = CACHED_READ_CONNECTION
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| anyhow::anyhow!("hot-store read connection lock was poisoned"))?;
    if cached
        .as_ref()
        .is_none_or(|(cached_path, _)| cached_path != path)
    {
        if !path.exists() {
            return Ok(None);
        }
        *cached = Some((path.to_path_buf(), read_connection(path)?));
    }
    let db = &mut cached.as_mut().expect("connection installed above").1;
    if !table_exists(db, "hot_store_meta")? {
        return Ok(None);
    }
    let tx = db.transaction_with_behavior(TransactionBehavior::Deferred)?;
    let Some((generation, true)) = meta(&tx, store)? else {
        return Ok(None);
    };
    let result = read(&tx, generation)?;
    tx.commit()?;
    Ok(Some(result))
}

#[cfg(test)]
pub(crate) fn clear_cached_generation() {
    if let Some(connection) = CACHED_READ_CONNECTION.get() {
        *connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

fn table_exists(connection: &Connection, table: &str) -> anyhow::Result<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1)",
        [table],
        |row| row.get(0),
    )?)
}

/// Load a verified store, or migrate the legacy JSON in one transaction.
/// `decode` must fully deserialize and validate the legacy bytes and return
/// exactly the rows used by the normal store decoder.
pub fn load_or_migrate(
    path: &Path,
    store: &str,
    legacy_path: &Path,
    decode: impl Fn(&[u8]) -> anyhow::Result<Rows>,
    validate_rows: impl Fn(&Rows) -> anyhow::Result<()>,
) -> anyhow::Result<LoadedRows> {
    let mut db = connection(path)?;
    {
        // Metadata and rows must belong to the same committed WAL snapshot,
        // including readers which deliberately do not hold the store OS lock.
        let tx = db.transaction_with_behavior(TransactionBehavior::Deferred)?;
        if let Some((generation, true)) = meta(&tx, store)? {
            #[cfg(test)]
            READ_SNAPSHOT_TEST_HOOK.with(|hook| {
                let action = hook.borrow_mut().take();
                if let Some(action) = action { action(); }
            });
            let rows = rows(&tx, store)?;
            tx.commit()?;
            return Ok(LoadedRows { generation, rows, exists: true });
        }
    }

    let legacy = match std::fs::read(legacy_path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read legacy store: {}", legacy_path.display()))
        }
    };
    let Some(legacy) = legacy else {
        return Ok(LoadedRows {
            generation: 0,
            rows: Rows::new(),
            exists: false,
        });
    };
    // Decode before taking SQLite's writer slot. Corrupt/truncated JSON must
    // leave both the database and the legacy backup untouched.
    let decoded = decode(&legacy)?;
    let digest = format!("{:x}", Sha256::digest(&legacy));
    bus_changes::invalidate(path, store)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some((generation, true)) = meta(&tx, store)? {
        let loaded = rows(&tx, store)?;
        tx.commit()?;
        return Ok(LoadedRows {
            generation,
            rows: loaded,
            exists: true,
        });
    }
    // A prior uncommitted design/prototype may have left an explicitly
    // unverified row set. It is never trusted or incrementally completed.
    tx.execute("DELETE FROM hot_store_meta WHERE store = ?1", [store])?;
    tx.execute(
        "INSERT INTO hot_store_meta(store, format_version, generation, migrated_at, legacy_sha256, verified)
         VALUES (?1, ?2, 1, ?3, ?4, 0)",
        params![store, FORMAT_VERSION, chrono::Utc::now().to_rfc3339(), digest],
    )?;
    {
        let mut insert = tx.prepare(
            "INSERT INTO hot_store_rows(store, collection, row_key, value_json) VALUES (?1, ?2, ?3, ?4)",
        )?;
        for ((collection, key), value) in &decoded {
            insert.execute(params![store, collection, key, value])?;
        }
    }
    let verified = rows(&tx, store)?;
    anyhow::ensure!(
        verified == decoded,
        "hot-store migration verification failed"
    );
    validate_rows(&verified).context("hot-store migration decoder verification failed")?;
    tx.execute(
        "UPDATE hot_store_meta SET verified = 1 WHERE store = ?1",
        [store],
    )?;
    tx.commit()?;
    Ok(LoadedRows {
        generation: 1,
        rows: decoded,
        exists: true,
    })
}

/// Commit changed rows and their transport tombstones atomically.
pub fn commit(path: &Path, store: &str, expected_generation: u64, changes: Vec<RowChange>) -> anyhow::Result<u64> {
    if changes.is_empty() { return Ok(expected_generation); }
    if bus_changes::spec(store).is_some() {
        drop(connection(path)?);
        return bus_changes::commit(path, store, expected_generation, changes);
    }
    let mut db = connection(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let next = commit_rows(&tx, store, expected_generation, &changes)?;
    tx.commit()?;
    Ok(next)
}

pub(super) fn commit_rows(tx: &Connection, store: &str, expected_generation: u64, changes: &[RowChange]) -> anyhow::Result<u64> {
    let current = meta(tx, store)?;
    match current {
        Some((generation, true)) => anyhow::ensure!(
            generation == expected_generation,
            "hot-store generation changed from {expected_generation} to {generation}"
        ),
        None if expected_generation == 0 => {
            tx.execute(
                "INSERT INTO hot_store_meta(store, format_version, generation, migrated_at, legacy_sha256, verified)
                 VALUES (?1, ?2, 0, NULL, NULL, 1)",
                params![store, FORMAT_VERSION],
            )?;
        }
        Some((_, false)) => anyhow::bail!("hot-store migration is not verified"),
        None => anyhow::bail!("hot-store generation disappeared"),
    }
    for change in changes {
        match change {
            RowChange::Put {
                collection,
                key,
                value,
            } => {
                tx.execute(
                    "INSERT INTO hot_store_rows(store, collection, row_key, value_json) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(store, collection, row_key) DO UPDATE SET value_json = excluded.value_json",
                    params![store, collection, key, value],
                )?;
            }
            RowChange::Delete { collection, key } => {
                tx.execute(
                    "DELETE FROM hot_store_rows WHERE store = ?1 AND collection = ?2 AND row_key = ?3",
                    params![store, collection, key],
                )?;
            }
        }
    }
    let next = expected_generation
        .checked_add(1)
        .context("hot-store generation overflow")?;
    tx.execute(
        "UPDATE hot_store_meta SET generation = ?2 WHERE store = ?1",
        params![
            store,
            i64::try_from(next).context("hot-store generation overflow")?
        ],
    )?;
    Ok(next)
}

/// After a verified legacy export, make the JSON authoritative for the next
/// r257 start. Rows stay in the database as a recovery copy, but the normal
/// migration path will atomically replace them with any changes made by the
/// older binary before marking the store verified again.
pub fn handoff_to_legacy(path: &Path, store: &str, expected_generation: u64) -> anyhow::Result<()> {
    let mut db = connection(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (generation, verified) =
        meta(&tx, store)?.context("hot store disappeared before legacy handoff")?;
    anyhow::ensure!(verified, "hot store is already handed off to legacy JSON");
    anyhow::ensure!(
        generation == expected_generation,
        "hot-store generation changed from {expected_generation} to {generation} before legacy handoff"
    );
    tx.execute(
        "UPDATE hot_store_meta SET verified = 0 WHERE store = ?1",
        [store],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn verified_rows(path: &Path, store: &str) -> anyhow::Result<Option<LoadedRows>> {
    verified_snapshot(path, store, |db, generation| {
        Ok(LoadedRows {
            generation,
            rows: rows(db, store)?,
            exists: true,
        })
    })
}

/// A coherent read-only WAL snapshot. Never creates, migrates or repairs data.
pub(crate) fn verified_snapshot<T>(
    path: &Path,
    store: &str,
    read: impl FnOnce(&Connection, u64) -> anyhow::Result<T>,
) -> anyhow::Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    let mut db = read_connection(path)?;
    if !table_exists(&db, "hot_store_meta")? {
        return Ok(None);
    }
    let tx = db.transaction_with_behavior(TransactionBehavior::Deferred)?;
    let Some((generation, true)) = meta(&tx, store)? else {
        return Ok(None);
    };
    let result = read(&tx, generation)?;
    tx.commit()?;
    Ok(Some(result))
}

pub(crate) fn verified_row(
    db: &Connection,
    store: &str,
    collection: &str,
    key: &str,
) -> anyhow::Result<Option<Vec<u8>>> {
    if !meta(db, store)?.is_some_and(|(_, verified)| verified) {
        return Ok(None);
    }
    let value: Option<Vec<u8>> = db
        .query_row(
            "SELECT value_json FROM hot_store_rows WHERE store=?1 AND collection=?2 AND row_key=?3",
            params![store, collection, key],
            |row| row.get(0),
        )
        .optional()?;
    note_bus_rows(store,usize::from(value.is_some()),value.as_ref().map_or(0,Vec::len));
    Ok(value)
}

pub(crate) fn bus_state_query(db: &Connection) -> anyhow::Result<String> {
    let indexed: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='index' AND name='agent_bus_message_state')",
        [], |row| row.get(0),
    )?;
    // Without ANALYZE, SQLite can prefer the (store,collection) primary-key
    // prefix and scan every retained payload. Pin the state index once the
    // normal writer has installed it; old read-only homes remain readable.
    let index = if indexed {
        " INDEXED BY agent_bus_message_state"
    } else {
        ""
    };
    Ok(format!(
        "SELECT collection,row_key,value_json FROM hot_store_rows{index}
         WHERE store='agent_bus' AND collection='message'
         AND json_extract(CAST(value_json AS TEXT),'$.snapshot.state')
             IN ('pending','inbox_accepted','stale_held')"
    ))
}

pub(crate) fn bus_state_rows(db: &Connection) -> anyhow::Result<Rows> {
    let mut query = db.prepare(&bus_state_query(db)?)?;
    let rows = query
        .query_map([], |row| Ok(((row.get(0)?, row.get(1)?), row.get(2)?)))?
        .collect::<rusqlite::Result<Rows>>()?;
    Ok(rows)
}

/// Indexed reader-fence probe; never creates or migrates a store.
pub(crate) fn has_row(path:&Path,store:&str,collection:&str,key:&str)->anyhow::Result<bool>{
    if !path.exists(){return Ok(false);}
    let db=read_connection(path)?;
    if !table_exists(&db,"hot_store_rows")?{return Ok(false);}
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM hot_store_rows WHERE store=?1 AND collection=?2 AND row_key=?3)",
        params![store,collection,key],|row|row.get(0))?)
}

/// Whether this store has any migration metadata, including an unverified
/// legacy handoff. Callers distinguish pre-SQLite JSON from an offline
/// conversion that requires re-import.
pub fn has_store(path: &Path, store: &str) -> anyhow::Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let db = read_connection(path)?;
    if !table_exists(&db, "hot_store_meta")? {
        return Ok(false);
    }
    Ok(meta(&db, store)?.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_rows_and_generation_share_one_wal_snapshot() {
        let root =
            std::env::temp_dir().join(format!("cutex-selected-snapshot-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(DATABASE_FILE);
        let put = |value: &[u8]| RowChange::Put {
            collection: "row".into(),
            key: "key".into(),
            value: value.to_vec(),
        };
        commit(&path, "fixture", 0, vec![put(b"old")]).unwrap();
        let observed = verified_snapshot(&path, "fixture", |db, generation| {
            commit(&path, "fixture", 1, vec![put(b"new")])?;
            Ok((generation, verified_row(db, "fixture", "row", "key")?))
        })
        .unwrap()
        .unwrap();
        assert_eq!(observed, (1, Some(b"old".to_vec())));
        assert_eq!(generation(&path, "fixture").unwrap(), Some(2));
        assert_eq!(
            verified_snapshot(&path, "fixture", |db, _| verified_row(
                db, "fixture", "row", "key"
            ))
            .unwrap()
            .unwrap(),
            Some(b"new".to_vec())
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corrupt_legacy_and_unverified_partial_rows_are_never_accepted() {
        let root = std::env::temp_dir().join(format!("cutex-hot-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let db_path = root.join(DATABASE_FILE);
        let legacy = root.join("legacy.json");
        std::fs::write(&legacy, b"{truncated").unwrap();
        assert!(load_or_migrate(
            &db_path,
            "fixture",
            &legacy,
            |bytes| serde_json::from_slice::<Rows>(bytes).map_err(Into::into),
            |_| Ok(()),
        )
        .is_err());
        assert_eq!(generation(&db_path, "fixture").unwrap(), None);

        let db = connection(&db_path).unwrap();
        db.execute(
            "INSERT INTO hot_store_meta(store, format_version, generation, verified) VALUES ('fixture', 1, 9, 0)",
            [],
        ).unwrap();
        db.execute(
            "INSERT INTO hot_store_rows(store, collection, row_key, value_json) VALUES ('fixture', 'old', 'partial', x'31')",
            [],
        ).unwrap();
        drop(db);
        let expected = Rows::from([(("new".into(), "complete".into()), b"1".to_vec())]);
        std::fs::write(&legacy, b"{\"complete\":true}").unwrap();
        let loaded = load_or_migrate(
            &db_path,
            "fixture",
            &legacy,
            |bytes| {
                let value: serde_json::Value = serde_json::from_slice(bytes)?;
                anyhow::ensure!(value["complete"] == true, "fixture incomplete");
                Ok(expected.clone())
            },
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(loaded.rows, expected);
        assert_eq!(loaded.generation, 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn generation_cas_prevents_cross_process_lost_update() {
        let root = std::env::temp_dir().join(format!("cutex-hot-cas-{}", uuid::Uuid::new_v4()));
        let db = root.join(DATABASE_FILE);
        let put = |key: &str| RowChange::Put {
            collection: "c".into(),
            key: key.into(),
            value: b"1".to_vec(),
        };
        assert_eq!(commit(&db, "fixture", 0, vec![put("a")]).unwrap(), 1);
        assert_eq!(cached_generation(&db, "fixture").unwrap(), Some(1));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let writers = ["b", "c"].map(|key| {
            let db = db.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                commit(
                    &db,
                    "fixture",
                    1,
                    vec![RowChange::Put {
                        collection: "c".into(),
                        key: key.into(),
                        value: b"1".to_vec(),
                    }],
                )
                .map(|_| key)
            })
        });
        let results = writers.map(|writer| writer.join().unwrap());
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let loser = results.into_iter().find_map(Result::err).unwrap();
        assert!(loser.to_string().contains("generation changed"));
        assert_eq!(
            cached_generation(&db, "fixture").unwrap(),
            Some(2),
            "the reused reader observes another connection's WAL commit",
        );
        let missing = if verified_rows(&db, "fixture")
            .unwrap()
            .unwrap()
            .rows
            .contains_key(&("c".into(), "b".into()))
        {
            "c"
        } else {
            "b"
        };
        assert_eq!(commit(&db, "fixture", 2, vec![put(missing)]).unwrap(), 3);
        let rows = verified_rows(&db, "fixture").unwrap().unwrap().rows;
        assert!(rows.contains_key(&("c".into(), "a".into())));
        assert!(rows.contains_key(&("c".into(), "b".into())));
        assert!(rows.contains_key(&("c".into(), "c".into())));
        assert_eq!(cached_generation(&db, "fixture").unwrap(), Some(3));
        clear_cached_generation();
        std::fs::remove_dir_all(root).unwrap();
    }
}

const MANAGEMENT_START_ROWS: &str = "store='agent_bus' AND collection='message' AND json_extract(CAST(value_json AS TEXT),'$.canonicalEnvelope.controlType') IS NOT NULL";
const SETTLEMENT_OUTBOX_ROWS: &str = "store='agent_bus' AND collection='message' AND json_array_length(CAST(value_json AS TEXT),'$.settlement.pendingSessions')>0 AND json_extract(CAST(value_json AS TEXT),'$.settlement.closure') IS NOT NULL";

fn existing_index(db: &Connection, name: &str) -> anyhow::Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name=?1)",[name],|row|row.get(0))?)
}

pub(crate) fn management_start_rows(db: &Connection, sender: &str, control: &str) -> anyhow::Result<Rows> {
    let index = if existing_index(db,"agent_bus_management_starts")? {" INDEXED BY agent_bus_management_starts"} else {""};
    let sql = format!("SELECT collection,row_key,value_json FROM hot_store_rows{index} WHERE {MANAGEMENT_START_ROWS}
        AND json_extract(CAST(value_json AS TEXT),'$.canonicalEnvelope.controlType')=?1
        AND json_extract(CAST(value_json AS TEXT),'$.canonicalEnvelope.from')=?2 ORDER BY row_key");
    let mut query = db.prepare(&sql)?;
    let rows = query.query_map(params![control,sender],|row|Ok(((row.get(0)?,row.get(1)?),row.get(2)?)))?.collect::<rusqlite::Result<Rows>>()?;
    note_bus_rows("agent_bus",rows.len(),rows.values().map(Vec::len).sum());
    Ok(rows)
}

pub(crate) fn settlement_outbox_rows(db: &Connection) -> anyhow::Result<Rows> {
    let index = if existing_index(db,"agent_bus_settlement_outbox")? {" INDEXED BY agent_bus_settlement_outbox"} else {""};
    let sql = format!("SELECT collection,row_key,value_json FROM hot_store_rows{index} WHERE {SETTLEMENT_OUTBOX_ROWS} ORDER BY row_key");
    let mut query = db.prepare(&sql)?;
    let rows = query.query_map([],|row|Ok(((row.get(0)?,row.get(1)?),row.get(2)?)))?.collect::<rusqlite::Result<Rows>>()?;
    note_bus_rows("agent_bus",rows.len(),rows.values().map(Vec::len).sum());
    Ok(rows)
}

// julianday is only a conservative prefilter. Equal millisecond buckets and
// unsupported/invalid RFC3339 forms are retained for the exact Rust predicate.
// The shape guard also prevents SQLite dynamic words (now/subsec) from making
// an expression index reject legacy timestamps that Rust previously skipped.
const FINISHED_TIME: &str = "IFNULL(CASE WHEN json_extract(CAST(value_json AS TEXT),'$.updatedAt') GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9][Tt]*' THEN julianday(json_extract(CAST(value_json AS TEXT),'$.updatedAt')) ELSE NULL END,-1.0e100)";
const FINISHED_ROWS: &str = "store='agent_bus' AND collection='message' AND json_extract(CAST(value_json AS TEXT),'$.snapshot.state') IN ('delivered','inbox_accepted','quarantined','discarded','failed') AND json_extract(CAST(value_json AS TEXT),'$.settlement') IS NULL AND (json_extract(CAST(value_json AS TEXT),'$.snapshot.presentation') IS NULL OR json_extract(CAST(value_json AS TEXT),'$.snapshot.presentation.receipt') IS NOT NULL)";

pub(crate) fn finished_message_rows(db: &Connection, cutoff: chrono::DateTime<chrono::Utc>) -> anyhow::Result<Rows> {
    let index = if existing_index(db,"agent_bus_finished_time")? {" INDEXED BY agent_bus_finished_time"} else {""};
    let sql = format!("SELECT collection,row_key,value_json FROM hot_store_rows{index} WHERE {FINISHED_ROWS} AND {FINISHED_TIME}<=IFNULL(julianday(?1),1.0e100) ORDER BY row_key");
    let mut query = db.prepare(&sql)?;
    let rows = query.query_map([cutoff.to_rfc3339()],|row|Ok(((row.get(0)?,row.get(1)?),row.get(2)?)))?.collect::<rusqlite::Result<Rows>>()?;
    note_bus_rows("agent_bus",rows.len(),rows.values().map(Vec::len).sum());
    Ok(rows)
}
