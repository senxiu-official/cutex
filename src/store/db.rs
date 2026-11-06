//! SQLite engine of the store layer: one `StoreDb` per domain file, a single
//! writer connection, pooled read connections, and the common tables
//! (instance id, per-store head, change log, outbox, migrations).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::Duration;

use anyhow::Context;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use super::domain::{DomainKind, TxDomain};
use super::keyed::{ChangeKind, PageBudget, Scope, MAX_CHANGE_SUMMARY_BYTES};
use super::manifest::StoreSpec;
use super::metrics::{self, OpCounts};
use super::outbox::OutboxItem;
use super::{notify, Epoch, Revision, Seq};

/// Longest row key the layer accepts (keys are not bodies).
pub const MAX_KEY_BYTES: usize = 1024;
const READER_POOL: usize = 4;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS store_instance (
  singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
  instance_id TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS store_head (
  store TEXT PRIMARY KEY,
  epoch INTEGER NOT NULL,
  seq INTEGER NOT NULL,
  low_water INTEGER NOT NULL,
  change_bytes INTEGER NOT NULL,
  schema_version INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS store_changes (
  store TEXT NOT NULL,
  seq INTEGER NOT NULL,
  scope TEXT NOT NULL,
  row_key TEXT NOT NULL,
  revision INTEGER NOT NULL,
  kind INTEGER NOT NULL,
  summary BLOB,
  PRIMARY KEY (store, seq)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS store_changes_scope ON store_changes (store, scope, seq);
CREATE TABLE IF NOT EXISTS store_outbox (
  id INTEGER PRIMARY KEY,
  store TEXT NOT NULL,
  topic TEXT NOT NULL,
  idem_key TEXT NOT NULL,
  payload BLOB NOT NULL,
  commit_seq INTEGER NOT NULL,
  claimed_until INTEGER,
  UNIQUE (topic, idem_key)
);
CREATE INDEX IF NOT EXISTS store_outbox_topic ON store_outbox (topic, id);
CREATE TABLE IF NOT EXISTS store_migrations (
  store TEXT NOT NULL,
  version INTEGER NOT NULL,
  applied_at TEXT NOT NULL,
  source_fingerprint TEXT,
  PRIMARY KEY (store, version)
);
CREATE TABLE IF NOT EXISTS store_kv (
  store TEXT NOT NULL,
  scope TEXT NOT NULL,
  row_key TEXT NOT NULL,
  revision INTEGER NOT NULL,
  seq INTEGER NOT NULL,
  value BLOB NOT NULL,
  PRIMARY KEY (store, scope, row_key)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS store_kv_index (
  store TEXT NOT NULL,
  index_name TEXT NOT NULL,
  scope TEXT NOT NULL,
  index_key TEXT NOT NULL,
  row_key TEXT NOT NULL,
  PRIMARY KEY (store, index_name, scope, index_key, row_key)
) WITHOUT ROWID;
";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoreHead {
    pub epoch: Epoch,
    pub seq: Seq,
    /// Changes with `seq <= low_water` were pruned from the transport log.
    pub low_water: Seq,
    pub schema_version: u32,
}

pub struct StoreDb {
    domain: &'static TxDomain,
    path: PathBuf,
    instance: String,
    writer: Mutex<Connection>,
    readers: Mutex<Vec<Connection>>,
}

static OPEN: OnceLock<Mutex<HashMap<PathBuf, Weak<StoreDb>>>> = OnceLock::new();

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn open_connection(path: &Path) -> anyhow::Result<Connection> {
    let db = Connection::open(path)
        .with_context(|| format!("open store database: {}", path.display()))?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.pragma_update(None, "foreign_keys", true)?;
    Ok(db)
}

fn enable_wal(db: &Connection) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match db.pragma_update(None, "journal_mode", "WAL") {
            Ok(()) => return Ok(()),
            Err(error) => {
                let contention = matches!(&error, rusqlite::Error::SqliteFailure(code, _)
                    if matches!(code.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked));
                // WAL promotion can return BUSY immediately despite busy_timeout.
                // Retry this idempotent pragma only; transaction semantics stay intact.
                if !contention || std::time::Instant::now() >= deadline {
                    return Err(error).context("promote store database to WAL");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

fn to_i64(value: u64, what: &str) -> anyhow::Result<i64> {
    i64::try_from(value).with_context(|| format!("{what} overflow"))
}

fn to_u64(value: i64, what: &str) -> anyhow::Result<u64> {
    u64::try_from(value).with_context(|| format!("negative {what}"))
}

fn fresh_epoch() -> Epoch {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let value = u64::from_le_bytes(bytes[..8].try_into().expect("8 bytes")) & (i64::MAX as u64);
    Epoch(value.max(1))
}

impl StoreDb {
    /// Open (or reuse) the domain database under the cutex home.
    pub fn open(domain: &'static TxDomain, home: &Path) -> anyhow::Result<Arc<StoreDb>> {
        anyhow::ensure!(
            domain.kind == DomainKind::Sqlite,
            "store domain {} is not a SQLite domain",
            domain.id
        );
        Self::open_at(domain, &home.join(domain.path))
    }

    /// Open (or reuse) a domain database at an explicit path (tests, kit tools).
    pub fn open_at(domain: &'static TxDomain, path: &Path) -> anyhow::Result<Arc<StoreDb>> {
        let mut open = lock(OPEN.get_or_init(|| Mutex::new(HashMap::new())));
        if let Some(existing) = open.get(path).and_then(Weak::upgrade) {
            anyhow::ensure!(
                existing.domain.id == domain.id,
                "{} is already open as domain {}",
                path.display(),
                existing.domain.id
            );
            return Ok(existing);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create store directory: {}", parent.display()))?;
        }
        let writer = open_connection(path)?;
        enable_wal(&writer)?;
        writer.pragma_update(None, "synchronous", "FULL")?;
        writer.execute_batch(SCHEMA)?;
        writer.execute(
            "INSERT OR IGNORE INTO store_instance(singleton, instance_id) VALUES (1, ?1)",
            [uuid::Uuid::new_v4().to_string()],
        )?;
        let instance: String = writer.query_row(
            "SELECT instance_id FROM store_instance WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        let db = Arc::new(StoreDb {
            domain,
            path: path.to_path_buf(),
            instance,
            writer: Mutex::new(writer),
            readers: Mutex::new(Vec::new()),
        });
        open.retain(|_, weak| weak.strong_count() > 0);
        open.insert(path.to_path_buf(), Arc::downgrade(&db));
        Ok(db)
    }

    pub fn domain(&self) -> &'static TxDomain {
        self.domain
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Host instance identity of this database; pinned in every cursor.
    pub fn instance_id(&self) -> &str {
        &self.instance
    }

    /// Make sure `spec` has a head row (fresh epoch on first use) and record
    /// its layer schema version. Idempotent.
    pub fn register(&self, spec: &'static StoreSpec) -> anyhow::Result<StoreHead> {
        self.write(|tx| tx.ensure_head(spec))
    }

    /// One deferred transaction: every read inside sees one WAL snapshot.
    pub fn read<T>(&self, f: impl FnOnce(&ReadTx<'_>) -> anyhow::Result<T>) -> anyhow::Result<T> {
        let pooled = lock(&self.readers).pop();
        let mut connection = match pooled {
            Some(connection) => connection,
            None => open_connection(&self.path)?,
        };
        let result = (|| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
            let value = f(&ReadTx { sql: &tx })?;
            tx.commit()?;
            Ok(value)
        })();
        let mut readers = lock(&self.readers);
        if readers.len() < READER_POOL && connection.is_autocommit() {
            readers.push(connection);
        }
        result
    }

    /// `BEGIN IMMEDIATE` on the single writer. Never wait on runtimes or the
    /// network inside `f`. Subscribers are woken after the commit.
    pub fn write<T>(&self, f: impl FnOnce(&mut WriteTx<'_>) -> anyhow::Result<T>) -> anyhow::Result<T> {
        let mut connection = lock(&self.writer);
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut write = WriteTx {
            sql: &tx,
            touched: Vec::new(),
        };
        let value = f(&mut write)?;
        let touched = std::mem::take(&mut write.touched);
        let mut stores: Vec<&'static StoreSpec> = Vec::new();
        for (spec, _, _) in &touched {
            if !stores.iter().any(|seen| seen.id == spec.id) {
                stores.push(spec);
            }
        }
        for spec in &stores {
            prune_changes(&tx, spec)?;
        }
        drop(write);
        tx.commit()?;
        drop(connection);
        for spec in stores {
            notify::publish(
                spec.id,
                touched
                    .iter()
                    .filter(|(touched, _, _)| touched.id == spec.id)
                    .map(|(_, scope, seq)| (scope.clone(), *seq)),
            );
        }
        Ok(value)
    }
}

/// Read side of a transaction.
pub struct ReadTx<'a> {
    sql: &'a Connection,
}

/// Write side of an immediate transaction.
pub struct WriteTx<'a> {
    sql: &'a Connection,
    touched: Vec<(&'static StoreSpec, Scope, Seq)>,
}

#[derive(Clone, Debug)]
pub struct RawChange {
    pub seq: Seq,
    pub scope: String,
    pub key: String,
    pub revision: Revision,
    pub kind: ChangeKind,
    pub summary: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct RawChanges {
    pub rows: Vec<RawChange>,
    pub more: bool,
}

fn head(sql: &Connection, spec: &StoreSpec) -> anyhow::Result<Option<StoreHead>> {
    note_sql(spec, 1, 0);
    sql.query_row(
        "SELECT epoch, seq, low_water, schema_version FROM store_head WHERE store = ?1",
        [spec.id],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        },
    )
    .optional()?
    .map(|(epoch, seq, low_water, schema_version)| {
        Ok(StoreHead {
            epoch: Epoch(to_u64(epoch, "store epoch")?),
            seq: to_u64(seq, "store seq")?,
            low_water: to_u64(low_water, "store low water")?,
            schema_version: u32::try_from(schema_version).context("store schema version")?,
        })
    })
    .transpose()
}

/// Head of a store that has never been registered: nothing to read yet.
const EMPTY_HEAD: StoreHead = StoreHead {
    epoch: Epoch(0),
    seq: 0,
    low_water: 0,
    schema_version: 0,
};

pub(crate) fn read_head(sql: &Connection, spec: &StoreSpec) -> anyhow::Result<StoreHead> {
    Ok(head(sql, spec)?.unwrap_or(EMPTY_HEAD))
}

pub(crate) fn note_sql(spec: &StoreSpec, statements: u64, rows: u64) {
    metrics::note(
        spec.id,
        OpCounts {
            sql_statements: statements,
            rows_visited: rows,
            ..OpCounts::default()
        },
    );
}

fn read_changes(
    sql: &Connection,
    spec: &StoreSpec,
    scope: &Scope,
    after: Seq,
    budget: PageBudget,
) -> anyhow::Result<RawChanges> {
    let limit = to_i64(budget.max_rows.saturating_add(1) as u64, "page rows")?;
    let after = to_i64(after, "change cursor")?;
    let mut statement;
    let mut rows = if *scope == Scope::Host {
        statement = sql.prepare_cached(
            "SELECT seq, scope, row_key, revision, kind, summary FROM store_changes
             WHERE store = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
        )?;
        statement.query(params![spec.id, after, limit])?
    } else {
        statement = sql.prepare_cached(
            "SELECT seq, scope, row_key, revision, kind, summary FROM store_changes
             WHERE store = ?1 AND scope = ?2 AND seq > ?3 ORDER BY seq LIMIT ?4",
        )?;
        statement.query(params![spec.id, scope.encode(), after, limit])?
    };
    let mut out = Vec::new();
    let mut used = 0usize;
    let mut visited = 0u64;
    let mut more = false;
    while let Some(row) = rows.next()? {
        visited += 1;
        let key: String = row.get(2)?;
        let summary: Option<Vec<u8>> = row.get(5)?;
        let bytes = key.len() + summary.as_ref().map_or(0, Vec::len) + 32;
        if !budget.admits(out.len(), used, bytes) {
            more = true;
            break;
        }
        used += bytes;
        out.push(RawChange {
            seq: to_u64(row.get(0)?, "change seq")?,
            scope: row.get(1)?,
            key,
            revision: to_u64(row.get(3)?, "change revision")?,
            kind: ChangeKind::from_code(row.get(4)?)?,
            summary,
        });
    }
    metrics::note(
        spec.id,
        OpCounts {
            sql_statements: 1,
            rows_visited: visited,
            rows_materialized: out.len() as u64,
            bytes_read: used as u64,
            ..OpCounts::default()
        },
    );
    Ok(RawChanges { rows: out, more })
}

/// Drop the oldest transport changes beyond the store's retention budget.
/// Work is proportional to the rows removed; business rows are untouched.
fn prune_changes(sql: &Connection, spec: &StoreSpec) -> anyhow::Result<()> {
    let Some(current) = head(sql, spec)? else {
        return Ok(());
    };
    let bytes: i64 = sql.query_row(
        "SELECT change_bytes FROM store_head WHERE store = ?1",
        [spec.id],
        |row| row.get(0),
    )?;
    let mut bytes = to_u64(bytes, "change bytes")?;
    let mut low_water = current.low_water;
    let retention = spec.change_retention;
    let mut freed_rows = 0u64;
    if current.seq - low_water > retention.max_rows {
        let cutoff = current.seq - retention.max_rows;
        let freed: i64 = sql.query_row(
            "SELECT COALESCE(SUM(length(row_key) + COALESCE(length(summary), 0) + 32), 0)
             FROM store_changes WHERE store = ?1 AND seq <= ?2",
            params![spec.id, to_i64(cutoff, "cutoff")?],
            |row| row.get(0),
        )?;
        sql.execute(
            "DELETE FROM store_changes WHERE store = ?1 AND seq <= ?2",
            params![spec.id, to_i64(cutoff, "cutoff")?],
        )?;
        bytes = bytes.saturating_sub(to_u64(freed, "freed bytes")?);
        freed_rows += cutoff - low_water;
        low_water = cutoff;
    }
    while bytes > retention.max_bytes && low_water < current.seq {
        // Oldest first, in small batches, until under the byte budget.
        let mut statement = sql.prepare_cached(
            "SELECT seq, length(row_key) + COALESCE(length(summary), 0) + 32 FROM store_changes
             WHERE store = ?1 AND seq > ?2 ORDER BY seq LIMIT 256",
        )?;
        let batch: Vec<(i64, i64)> = statement
            .query_map(params![spec.id, to_i64(low_water, "low water")?], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        if batch.is_empty() {
            break;
        }
        let mut cutoff = low_water;
        for (seq, size) in batch {
            cutoff = to_u64(seq, "change seq")?;
            bytes = bytes.saturating_sub(to_u64(size, "change size")?);
            freed_rows += 1;
            if bytes <= retention.max_bytes {
                break;
            }
        }
        sql.execute(
            "DELETE FROM store_changes WHERE store = ?1 AND seq <= ?2",
            params![spec.id, to_i64(cutoff, "cutoff")?],
        )?;
        low_water = cutoff;
    }
    if low_water != current.low_water {
        sql.execute(
            "UPDATE store_head SET low_water = ?2, change_bytes = ?3 WHERE store = ?1",
            params![spec.id, to_i64(low_water, "low water")?, to_i64(bytes, "change bytes")?],
        )?;
        note_sql(spec, 2, freed_rows);
    }
    Ok(())
}

impl ReadTx<'_> {
    /// SQL access for repository modules only (enforced by the ratchet test).
    pub fn sql(&self) -> &Connection {
        self.sql
    }

    /// Head of `spec`; an unregistered store reads as empty (epoch 0).
    pub fn head(&self, spec: &StoreSpec) -> anyhow::Result<StoreHead> {
        Ok(head(self.sql, spec)?.unwrap_or(EMPTY_HEAD))
    }

    pub fn changes(
        &self,
        spec: &StoreSpec,
        scope: &Scope,
        after: Seq,
        budget: PageBudget,
    ) -> anyhow::Result<RawChanges> {
        read_changes(self.sql, spec, scope, after, budget)
    }
}

impl WriteTx<'_> {
    pub fn sql(&self) -> &Connection {
        self.sql
    }

    pub fn head(&self, spec: &StoreSpec) -> anyhow::Result<StoreHead> {
        Ok(head(self.sql, spec)?.unwrap_or(EMPTY_HEAD))
    }

    pub(crate) fn ensure_head(&mut self, spec: &'static StoreSpec) -> anyhow::Result<StoreHead> {
        if let Some(current) = head(self.sql, spec)? {
            if current.schema_version != spec.schema_version {
                self.sql.execute(
                    "UPDATE store_head SET schema_version = ?2 WHERE store = ?1",
                    params![spec.id, i64::from(spec.schema_version)],
                )?;
            }
            return Ok(StoreHead {
                schema_version: spec.schema_version,
                ..current
            });
        }
        let epoch = fresh_epoch();
        self.sql.execute(
            "INSERT INTO store_head(store, epoch, seq, low_water, change_bytes, schema_version)
             VALUES (?1, ?2, 0, 0, 0, ?3)",
            params![spec.id, to_i64(epoch.0, "epoch")?, i64::from(spec.schema_version)],
        )?;
        note_sql(spec, 1, 0);
        Ok(StoreHead {
            epoch,
            seq: 0,
            low_water: 0,
            schema_version: spec.schema_version,
        })
    }

    /// Allocate the next commit seq of `spec` and append one change row
    /// (a tombstone for `Delete`). Returns the seq.
    pub fn record_change(
        &mut self,
        spec: &'static StoreSpec,
        scope: &Scope,
        key: &str,
        revision: Revision,
        kind: ChangeKind,
        summary: Option<&[u8]>,
    ) -> anyhow::Result<Seq> {
        if key.len() > MAX_KEY_BYTES {
            return Err(super::StoreError::Invalid(format!("row key longer than {MAX_KEY_BYTES} bytes")).into());
        }
        if summary.is_some_and(|summary| summary.len() > MAX_CHANGE_SUMMARY_BYTES) {
            return Err(super::StoreError::Invalid(format!(
                "change summary longer than {MAX_CHANGE_SUMMARY_BYTES} bytes"
            ))
            .into());
        }
        let current = self.ensure_head(spec)?;
        let seq = current.seq.checked_add(1).context("store seq overflow")?;
        let size = key.len() + summary.map_or(0, <[u8]>::len) + 32;
        self.sql
            .prepare_cached(
                "INSERT INTO store_changes(store, seq, scope, row_key, revision, kind, summary)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?
            .execute(params![
                spec.id,
                to_i64(seq, "seq")?,
                scope.encode(),
                key,
                to_i64(revision, "revision")?,
                kind.code(),
                summary
            ])?;
        self.sql
            .prepare_cached(
                "UPDATE store_head SET seq = ?2, change_bytes = change_bytes + ?3 WHERE store = ?1",
            )?
            .execute(params![spec.id, to_i64(seq, "seq")?, size as i64])?;
        metrics::note(
            spec.id,
            OpCounts {
                sql_statements: 2,
                bytes_written: size as u64,
                ..OpCounts::default()
            },
        );
        self.touched.push((spec, scope.clone(), seq));
        Ok(seq)
    }

    /// Enqueue in this transaction; a repeated `(topic, idem_key)` is a no-op.
    pub fn outbox(&mut self, spec: &'static StoreSpec, item: &OutboxItem) -> anyhow::Result<()> {
        let current = self.ensure_head(spec)?;
        self.sql
            .prepare_cached(
                "INSERT INTO store_outbox(store, topic, idem_key, payload, commit_seq)
                 VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(topic, idem_key) DO NOTHING",
            )?
            .execute(params![
                spec.id,
                item.topic,
                item.idem_key,
                item.payload,
                to_i64(current.seq, "seq")?
            ])?;
        metrics::note(
            spec.id,
            OpCounts {
                sql_statements: 1,
                bytes_written: item.payload.len() as u64,
                ..OpCounts::default()
            },
        );
        Ok(())
    }

    /// Record that a layer migration of `spec` ran from `source_fingerprint`.
    pub fn record_migration(
        &mut self,
        spec: &'static StoreSpec,
        version: u32,
        source_fingerprint: Option<&str>,
    ) -> anyhow::Result<()> {
        self.sql.execute(
            "INSERT INTO store_migrations(store, version, applied_at, source_fingerprint)
             VALUES (?1, ?2, ?3, ?4)",
            params![spec.id, i64::from(version), chrono::Utc::now().to_rfc3339(), source_fingerprint],
        )?;
        Ok(())
    }

    /// Start a new change history for `spec` (migration / restore): fresh
    /// epoch, transport log cleared. Every existing cursor must resync.
    pub fn reset_epoch(&mut self, spec: &'static StoreSpec) -> anyhow::Result<StoreHead> {
        let current = self.ensure_head(spec)?;
        let epoch = fresh_epoch();
        self.sql
            .execute("DELETE FROM store_changes WHERE store = ?1", [spec.id])?;
        self.sql.execute(
            "UPDATE store_head SET epoch = ?2, low_water = seq, change_bytes = 0 WHERE store = ?1",
            params![spec.id, to_i64(epoch.0, "epoch")?],
        )?;
        Ok(StoreHead {
            epoch,
            low_water: current.seq,
            ..current
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::store::domain::TxDomain;

    pub(crate) static TEST_DOMAIN: TxDomain = TxDomain {
        id: "test",
        kind: DomainKind::Sqlite,
        path: "store-test.sqlite3",
        note: "tests",
    };

    pub(crate) fn temp_db(name: &str) -> (PathBuf, Arc<StoreDb>) {
        let root = std::env::temp_dir().join(format!("cutex-store-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let db = StoreDb::open_at(&TEST_DOMAIN, &root.join("store.sqlite3")).unwrap();
        (root, db)
    }

    static SPEC: StoreSpec = crate::store::kv::tests::spec_for_tests("db-fixture", 3, 1_000_000);

    fn init_child(root: &Path, slot: usize) -> std::process::Child {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "store::db::tests::private_wal_opener", "--ignored", "--nocapture", "--test-threads=1"])
            .env("CUTEX_STORE_INIT_FIXTURE", root)
            .env("CUTEX_STORE_INIT_SLOT", slot.to_string())
            .stdout(std::process::Stdio::null()).spawn().unwrap()
    }
    fn wait_child(child: &mut std::process::Child) {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait().unwrap() { assert!(status.success(), "owned opener {}: {status}", child.id()); return }
            if std::time::Instant::now() >= deadline {
                // This exact Child was spawned above; never traverse a process tree.
                child.kill().unwrap(); let _ = child.wait();
                panic!("owned opener timed out");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn init_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!("wal-init-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap(); root
    }
    fn ready(root: &Path, count: usize) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while (0..count).any(|slot| !root.join(format!("ready-{slot}")).exists()) {
            assert!(std::time::Instant::now() < deadline, "owned opener did not start");
            std::thread::sleep(Duration::from_millis(10));
        }
        std::fs::write(root.join("go"), []).unwrap();
    }
    #[test]
    #[ignore = "owned cross-process SQLite opener fixture"]
    fn private_wal_opener() {
        let root = PathBuf::from(std::env::var_os("CUTEX_STORE_INIT_FIXTURE").unwrap());
        assert!(root.canonicalize().unwrap().starts_with(std::env::temp_dir().canonicalize().unwrap()));
        let slot: usize = std::env::var("CUTEX_STORE_INIT_SLOT").unwrap().parse().unwrap();
        std::fs::write(root.join(format!("ready-{slot}")), []).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !root.join("go").exists() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        let db = StoreDb::open_at(&TEST_DOMAIN, &root.join("store.sqlite3")).unwrap();
        db.write(|tx| {
            tx.sql().execute_batch("CREATE TABLE IF NOT EXISTS init_writes(slot INTEGER PRIMARY KEY)")?;
            tx.sql().execute("INSERT INTO init_writes(slot) VALUES (?1)", [slot as i64])?;
            Ok(())
        }).unwrap();
    }
    #[test]
    fn wal_promotion_waits_for_a_real_reader_and_preserves_existing_rows() {
        let _env = crate::config::paths::test_home_env_lock();
        let root = init_root();
        let reader = Connection::open(root.join("store.sqlite3")).unwrap();
        reader.execute_batch("CREATE TABLE retained(value TEXT); INSERT INTO retained VALUES ('keep'); BEGIN; SELECT * FROM retained;").unwrap();
        let mut child = init_child(&root, 0); ready(&root, 1);
        std::thread::sleep(Duration::from_millis(200));
        assert!(child.try_wait().unwrap().is_none(), "WAL opener must wait for the reader");
        reader.execute_batch("COMMIT").unwrap(); wait_child(&mut child);
        assert_eq!(reader.query_row("SELECT value FROM retained", [], |r|r.get::<_,String>(0)).unwrap(), "keep");
        drop(reader); std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn fresh_store_processes_initialize_and_commit_without_losing_writes() {
        let _env = crate::config::paths::test_home_env_lock();
        for _ in 0..8 {
            let root = init_root();
            let mut first = init_child(&root, 0); let mut second = init_child(&root, 1);
            ready(&root, 2); wait_child(&mut first); wait_child(&mut second);
            let db = Connection::open(root.join("store.sqlite3")).unwrap();
            assert_eq!(db.query_row("SELECT count(*) FROM init_writes", [], |r|r.get::<_,i64>(0)).unwrap(), 2);
            assert_eq!(db.query_row("PRAGMA integrity_check", [], |r|r.get::<_,String>(0)).unwrap(), "ok");
            drop(db); std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn changes_retention_advances_low_water_and_reopen_keeps_instance() {
        let (root, db) = temp_db("retention");
        let instance = db.instance_id().to_owned();
        for n in 0..5 {
            db.write(|tx| {
                tx.record_change(&SPEC, &Scope::Host, &format!("k{n}"), n + 1, ChangeKind::Put, None)
            })
            .unwrap();
        }
        let head = db.read(|tx| tx.head(&SPEC)).unwrap();
        assert_eq!((head.seq, head.low_water), (5, 2));
        let changes = db
            .read(|tx| tx.changes(&SPEC, &Scope::Host, 0, PageBudget::DELTA_DEFAULT))
            .unwrap();
        assert_eq!(changes.rows.iter().map(|row| row.seq).collect::<Vec<_>>(), [3, 4, 5]);
        drop(db);
        let reopened = StoreDb::open_at(&TEST_DOMAIN, &root.join("store.sqlite3")).unwrap();
        assert_eq!(reopened.instance_id(), instance);
        assert_eq!(reopened.read(|tx| tx.head(&SPEC)).unwrap().epoch, head.epoch);
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn byte_retention_prunes_oldest_until_under_budget() {
        static SMALL: StoreSpec = crate::store::kv::tests::spec_for_tests("db-bytes", 1000, 200);
        let (root, db) = temp_db("bytes");
        for n in 0..10u64 {
            db.write(|tx| {
                tx.record_change(&SMALL, &Scope::Host, &format!("key-{n:04}"), n + 1, ChangeKind::Put, Some(&[0; 20]))
            })
            .unwrap();
        }
        let head = db.read(|tx| tx.head(&SMALL)).unwrap();
        // 8 + 20 + 32 = 60 bytes per change: at most 3 fit in 200.
        assert_eq!(head.seq, 10);
        assert_eq!(head.low_water, 7);
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn oversized_summary_and_key_are_rejected_without_writing() {
        let (root, db) = temp_db("summary");
        let error = db
            .write(|tx| {
                tx.record_change(&SPEC, &Scope::Host, "k", 1, ChangeKind::Put, Some(&[0; MAX_CHANGE_SUMMARY_BYTES + 1]))
            })
            .unwrap_err();
        assert!(error.to_string().contains("summary"));
        assert!(db
            .write(|tx| tx.record_change(&SPEC, &Scope::Host, &"k".repeat(MAX_KEY_BYTES + 1), 1, ChangeKind::Put, None))
            .is_err());
        assert_eq!(db.read(|tx| tx.head(&SPEC)).unwrap().seq, 0);
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }
}
