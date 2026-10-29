//! Row-oriented SQLite backing for the large management stores.
//!
//! Business modules retain their own cross-process lock.  This layer adds an
//! SQLite generation CAS and an immediate transaction, so independent store
//! locks may safely share one WAL database.

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
    Ok(db)
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

fn rows(connection: &Connection, store: &str) -> anyhow::Result<Rows> {
    let mut statement = connection.prepare(
        "SELECT collection, row_key, value_json FROM hot_store_rows WHERE store = ?1 ORDER BY collection, row_key",
    )?;
    let values =
        statement.query_map([store], |row| Ok(((row.get(0)?, row.get(1)?), row.get(2)?)))?;
    values
        .collect::<rusqlite::Result<Rows>>()
        .map_err(Into::into)
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

/// Commit only the supplied changed rows and advance the store generation.
pub fn commit(
    path: &Path,
    store: &str,
    expected_generation: u64,
    changes: Vec<RowChange>,
) -> anyhow::Result<u64> {
    if changes.is_empty() {
        return Ok(expected_generation);
    }
    let mut db = connection(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current = meta(&tx, store)?;
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
    tx.commit()?;
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
    if !path.exists() {
        return Ok(None);
    }
    let db = read_connection(path)?;
    if !table_exists(&db, "hot_store_meta")? {
        return Ok(None);
    }
    let Some((generation, true)) = meta(&db, store)? else {
        return Ok(None);
    };
    Ok(Some(LoadedRows {
        generation,
        rows: rows(&db, store)?,
        exists: true,
    }))
}

/// Whether this store has any migration metadata, including an unverified
/// legacy handoff. Callers use this to distinguish a pre-SQLite JSON store
/// from JSON that must be re-imported before another offline conversion.
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
