//! Additive v1 indexes and a transactional, bounded cross-process change feed.
//! Triggers deliberately cover previous-build writers too (R0).
use super::*;
use crate::store::metrics::{note, OpCounts};
use crate::store::{ChangeCursor, Epoch, PageBudget, Scope, StoreHead};
use rusqlite::OptionalExtension;

const STORE: &str = "session-store";
const CACHE_BYTES: usize = 8 * 1024 * 1024;
const CACHE_ROWS: usize = 1024;

#[path = "indexed_repository.rs"]
mod repository;
pub use repository::{SessionFilter, SessionRepository};

pub(super) fn install(connection: &mut Connection) -> anyhow::Result<()> {
    let installed: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM meta WHERE key='session_query_v1')",
        [],
        |r| r.get(0),
    )?;
    if installed {
        return Ok(());
    }
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // Another process may have finished while we waited for the writer slot.
    // Recheck after acquiring it so cold concurrent readers build once.
    let installed: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM meta WHERE key='session_query_v1')",
        [],
        |r| r.get(0),
    )?;
    if installed {
        tx.finish()?;
        return Ok(());
    }
    let retention = crate::store::manifest::spec(STORE).change_retention;
    let retained_rows = i64::try_from(retention.max_rows)?;
    let retained_bytes = i64::try_from(retention.max_bytes)?;
    tx.execute_batch("
        CREATE TABLE IF NOT EXISTS store_instance (
            singleton INTEGER PRIMARY KEY CHECK(singleton=1), instance_id TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS session_query_head (
            singleton INTEGER PRIMARY KEY CHECK(singleton=1), instance TEXT NOT NULL,
            seq INTEGER NOT NULL, low_water INTEGER NOT NULL, total_bytes INTEGER NOT NULL,
            low_revision INTEGER NOT NULL, session_low_water INTEGER NOT NULL,
            low_bytes INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS session_changes (
            seq INTEGER PRIMARY KEY, map TEXT NOT NULL, key TEXT NOT NULL,
            deleted INTEGER NOT NULL, rev INTEGER NOT NULL, end_bytes INTEGER NOT NULL,
            commit_revision INTEGER NOT NULL, session_id TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS session_catalog_index (
            key TEXT PRIMARY KEY, session_id TEXT NOT NULL, archive_state TEXT NOT NULL,
            host_id TEXT NOT NULL, eligible INTEGER NOT NULL,
            display_override TEXT, base_name TEXT NOT NULL, change_seq INTEGER NOT NULL);
        CREATE INDEX IF NOT EXISTS session_catalog_active ON session_catalog_index(session_id,key)
            WHERE archive_state='active' AND eligible=1;
        CREATE INDEX IF NOT EXISTS session_changes_map ON session_changes(map, seq);
        CREATE INDEX IF NOT EXISTS session_changes_bytes ON session_changes(end_bytes);
        CREATE INDEX IF NOT EXISTS session_changes_revision ON session_changes(map,commit_revision);
        CREATE INDEX IF NOT EXISTS session_changes_subject ON session_changes(map,session_id,seq);
        CREATE INDEX IF NOT EXISTS sessions_id ON sessions(cutex_session_id, key);
        CREATE INDEX IF NOT EXISTS sessions_lifecycle_page ON sessions(archive_state, cutex_session_id, key);
        CREATE INDEX IF NOT EXISTS sessions_lifecycle_key ON sessions(archive_state, key);
        CREATE INDEX IF NOT EXISTS sessions_active_key ON sessions(key) WHERE archive_state='active';
        CREATE INDEX IF NOT EXISTS sessions_active_id ON sessions(cutex_session_id,key) WHERE archive_state='active';
        CREATE INDEX IF NOT EXISTS receipts_map_rev ON receipts(map, rev);
        CREATE INDEX IF NOT EXISTS receipts_prepared_session
            ON receipts(json_extract(json,'$.receipt.review.subject.cutex_session_id'))
            WHERE map='explicit_launch' AND json_extract(json,'$.kind')='runtime'
                AND json_extract(json,'$.receipt.stage')='prepared';
    ")?;
    tx.execute(
        "INSERT OR IGNORE INTO store_instance VALUES(1,?1)",
        [uuid::Uuid::new_v4().to_string()],
    )?;
    tx.execute("INSERT OR IGNORE INTO session_query_head SELECT 1,instance_id,0,0,0,0,0,0 FROM store_instance WHERE singleton=1",[])?;
    for (table, map, key, rev) in [
        ("sessions", "''", "key", "rev"),
        ("receipts", "NEW.map", "key", "rev"),
    ] {
        for (operation, row, deleted) in [
            ("INSERT", "NEW", 0),
            ("UPDATE", "NEW", 0),
            ("DELETE", "OLD", 1),
        ] {
            let map = if table == "receipts" {
                format!("{row}.map")
            } else {
                map.to_owned()
            };
            let session_id = if table == "sessions" {
                format!("{row}.cutex_session_id")
            } else {
                "''".to_owned()
            };
            let index_sql = if table != "sessions" {
                String::new()
            } else if deleted == 1 {
                "DELETE FROM session_catalog_index WHERE key=OLD.key;".to_owned()
            } else {
                format!("INSERT INTO session_catalog_index {} ON CONFLICT(key) DO UPDATE SET
                    session_id=excluded.session_id,archive_state=excluded.archive_state,host_id=excluded.host_id,
                    eligible=excluded.eligible,display_override=excluded.display_override,base_name=excluded.base_name,change_seq=excluded.change_seq;",
                    catalog_projection("NEW",""))
            };
            let event_rev = if deleted == 1 {
                "CAST((SELECT value FROM meta WHERE key='store_revision') AS INTEGER)+1".to_owned()
            } else {
                format!("{row}.{rev}")
            };
            // Cumulative bytes make both retention cuts indexed. No COUNT or
            // full-log scan on a commit; only physically expired feed rows go.
            tx.execute_batch(&format!("
                CREATE TRIGGER IF NOT EXISTS s3_{table}_{operation} AFTER {operation} ON {table} BEGIN
                    {index_sql}
                    UPDATE session_query_head SET seq=seq+1,
                        total_bytes=total_bytes+length(CAST({row}.{key} AS BLOB))+length(CAST({map} AS BLOB))+length(CAST({session_id} AS BLOB))+64
                        WHERE singleton=1;
                    INSERT INTO session_changes
                        SELECT seq,{map},{row}.{key},{deleted},{event_rev},total_bytes,
                            CAST((SELECT value FROM meta WHERE key='store_revision') AS INTEGER)+1,{session_id}
                        FROM session_query_head WHERE singleton=1;
                    UPDATE session_query_head SET low_water=MAX(low_water,seq-{retained_rows},
                        COALESCE((SELECT seq FROM session_changes WHERE total_bytes-low_bytes>{retained_bytes} AND end_bytes>=total_bytes-{retained_bytes} ORDER BY end_bytes ASC LIMIT 1),0))
                        WHERE singleton=1;
                    UPDATE session_query_head SET low_revision=MAX(low_revision,
                        COALESCE((SELECT MAX(commit_revision) FROM session_changes WHERE seq<=low_water),0)),
                        session_low_water=MAX(session_low_water,
                        COALESCE((SELECT MAX(seq) FROM session_changes WHERE map='' AND seq<=low_water),0)),
                        low_bytes=COALESCE((SELECT end_bytes FROM session_changes WHERE seq=low_water),low_bytes)
                        WHERE singleton=1;
                    DELETE FROM session_changes WHERE seq<=(SELECT low_water FROM session_query_head WHERE singleton=1);
                END;
            "))?;
        }
    }
    // One bounded-memory SQL index build on the existing canonical rows;
    // no receipt rows or business bodies are copied into the read model.
    tx.execute_batch(&format!(
        "INSERT OR REPLACE INTO session_catalog_index {}",
        catalog_projection("s", "FROM sessions AS s")
    ))?;
    tx.execute(
        "INSERT OR IGNORE INTO meta VALUES('session_query_v1','1')",
        [],
    )?;
    tx.commit()?;
    Ok(())
}

fn catalog_projection(row: &str, from: &str) -> String {
    let sequence = if row == "NEW" {
        "(SELECT seq+1 FROM session_query_head WHERE singleton=1)"
    } else {
        "0"
    };
    format!("SELECT {row}.key,{row}.cutex_session_id,{row}.archive_state,
        coalesce(json_extract({row}.record_json,'$.host_id'),''),
        coalesce(coalesce(json_extract({row}.record_json,'$.agent_enabled'),0)=1 OR
            (coalesce(json_extract({row}.record_json,'$.registration_class'),'local_only')='local_only'
             AND {row}.formal_agent_name IS NULL AND
             (json_type({row}.record_json,'$.explicit_launch')='object' OR
              json_extract({row}.record_json,'$.runtime_backend') IN ('claude_subscription','dsh','kimi'))),0),
        json_extract({row}.record_json,'$.display_name_override'),
        coalesce({row}.formal_agent_name,json_extract({row}.record_json,'$.display_name_hint'),
            json_extract({row}.record_json,'$.thread_name'),{row}.codex_session_id,{row}.cutex_session_id),{sequence}
        {from}")
}

fn head(connection: &Connection) -> anyhow::Result<(String, StoreHead)> {
    note(
        STORE,
        OpCounts {
            sql_statements: 1,
            ..Default::default()
        },
    );
    Ok(connection.query_row(
        "SELECT instance,seq,session_low_water FROM session_query_head WHERE singleton=1",
        [],
        |r| {
            Ok((
                r.get(0)?,
                StoreHead {
                    epoch: Epoch(1),
                    seq: r.get::<_, i64>(1)? as u64,
                    low_water: r.get::<_, i64>(2)? as u64,
                    schema_version: 1,
                },
            ))
        },
    )?)
}

pub fn has_prepared_launch_at(path: &Path, session: &str) -> anyhow::Result<bool> {
    let Some(db) = ensure_ready(path, false, false)? else {
        return Ok(false);
    };
    with_connection(&db, |connection| {
        let exists:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM receipts INDEXED BY receipts_prepared_session WHERE map='explicit_launch'
            AND json_extract(json,'$.kind')='runtime' AND json_extract(json,'$.receipt.stage')='prepared'
            AND json_extract(json,'$.receipt.review.subject.cutex_session_id')=?1)",[session],|r|r.get(0))?;
        note(
            STORE,
            OpCounts {
                sql_statements: 1,
                rows_visited: u64::from(exists),
                ..Default::default()
            },
        );
        Ok(exists)
    })
}

/// Startup recovery reads only unfinished rows in the promotion receipt map;
/// it does not initialize the compatibility snapshot or decode session rows.
pub(crate) fn pending_quick_chat_promotions_at(
    path: &Path,
) -> anyhow::Result<Vec<crate::agent_management::QuickChatPromoteReceipt>> {
    let Some(db) = ensure_ready(path, false, false)? else {
        return Ok(Vec::new());
    };
    with_connection(&db, |connection| {
        let mut query = connection.prepare_cached(
            "SELECT json FROM receipts WHERE map=?1 AND json_extract(json,'$.result.complete')=0 ORDER BY key",
        )?;
        let rows = query.query_map([QuickChatPromotion::NAME], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    })
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct CacheKey {
    path: PathBuf,
    instance: String,
    key: String,
}
struct CachedRow {
    rev: u64,
    bytes: usize,
    value: Arc<CutexSessionRecord>,
}
#[derive(Default)]
struct RowCache {
    rows: HashMap<CacheKey, CachedRow>,
    fifo: std::collections::VecDeque<CacheKey>,
    bytes: usize,
}
static ROW_CACHE: std::sync::LazyLock<Mutex<RowCache>> =
    std::sync::LazyLock::new(|| Mutex::new(RowCache::default()));

fn decode(
    path: &Path,
    instance: &str,
    key: String,
    rev: u64,
    json: String,
) -> anyhow::Result<Arc<CutexSessionRecord>> {
    note(
        STORE,
        OpCounts {
            bytes_read: json.len() as u64,
            rows_visited: 1,
            ..Default::default()
        },
    );
    // Cache is bounded by encoded bytes and row count; old readers keep only
    // their row Arc. Holding one row never detaches a whole collection.
    let mut cache = ROW_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let cache_key = CacheKey {
        path: path.into(),
        instance: instance.into(),
        key,
    };
    let mut publish = true;
    if let Some(row) = cache.rows.get(&cache_key) {
        if row.rev == rev {
            return Ok(Arc::clone(&row.value));
        }
        // A slow WAL reader may finish after a newer generation. It gets its
        // own snapshot value without replacing the newer shared cache row.
        publish = row.rev < rev;
    }
    let value = Arc::new(serde_json::from_str(&json)?);
    parsed_row();
    note(
        STORE,
        OpCounts {
            bytes_decoded: json.len() as u64,
            rows_materialized: 1,
            ..Default::default()
        },
    );
    if publish && json.len() <= CACHE_BYTES {
        let old = cache.rows.insert(
            cache_key.clone(),
            CachedRow {
                rev,
                bytes: json.len(),
                value: Arc::clone(&value),
            },
        );
        if let Some(old) = old {
            cache.bytes -= old.bytes;
        } else {
            cache.fifo.push_back(cache_key);
        }
        cache.bytes += json.len();
        while cache.rows.len() > CACHE_ROWS || cache.bytes > CACHE_BYTES {
            let oldest = cache.fifo.pop_front().expect("cached row has a FIFO key");
            if let Some(old) = cache.rows.remove(&oldest) {
                cache.bytes -= old.bytes;
            }
        }
    }
    Ok(value)
}

pub fn get_at(path: &Path, id: &str) -> anyhow::Result<Option<Arc<CutexSessionRecord>>> {
    let Some(db) = ensure_ready(path, false, false)? else {
        return Ok(None);
    };
    with_connection(&db, |connection| {
        let tx = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let (instance, _) = head(&tx)?;
        let row: Option<(String, i64, String)> = tx.query_row(
            "SELECT key,rev,record_json FROM sessions WHERE key=?1 UNION ALL
             SELECT key,rev,record_json FROM sessions WHERE cutex_session_id=?1 AND key<>?1 LIMIT 1",
            [id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        note(
            STORE,
            OpCounts {
                sql_statements: 1,
                ..Default::default()
            },
        );
        row.map(|(key, rev, json)| decode(&db, &instance, key, as_rev(rev)?, json))
            .transpose()
    })
}

/// A public host-catalog ID, never a map-key alias or substring search.
pub fn host_row_at(path: &Path, host: &str, id: &str) -> anyhow::Result<Option<Arc<CutexSessionRecord>>> {
    let Some(db) = ensure_ready(path, false, false)? else { return Ok(None); };
    with_connection(&db, |connection| {
        let tx=connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let (instance,_)=head(&tx)?;
        let row:Option<(String,i64,String)>=tx.query_row(
            "SELECT key,rev,record_json FROM sessions INDEXED BY sessions_active_id WHERE cutex_session_id=?1 AND archive_state='active' ORDER BY key LIMIT 1",
            [id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        note(STORE,OpCounts {sql_statements:1,..Default::default()});
        let record=row.map(|(key,rev,json)|decode(&db,&instance,key,as_rev(rev)?,json)).transpose()?;
        Ok(record.filter(|r|r.cutex_session_id==id && r.is_active() && !r.is_retired()
            && (r.agent_enabled || r.is_owned_session())
            && crate::runtime::lifecycle::cutex_session_host_is_local(&r.host_id,host)))
    })
}

pub(crate) fn owned_by_native_at(
    path: &Path,
    native: &str,
) -> anyhow::Result<Option<Arc<CutexSessionRecord>>> {
    let Some(db) = ensure_ready(path, false, false)? else {
        return Ok(None);
    };
    with_connection(&db, |connection| {
        let tx = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let (instance, _) = head(&tx)?;
        let mut stmt=tx.prepare_cached("SELECT key,rev,record_json FROM sessions INDEXED BY sessions_codex WHERE codex_session_id=?1")?;
        let mut rows = stmt.query([native])?;
        note(
            STORE,
            OpCounts {
                sql_statements: 1,
                ..Default::default()
            },
        );
        while let Some(row) = rows.next()? {
            let record = decode(
                &db,
                &instance,
                row.get(0)?,
                as_rev(row.get(1)?)?,
                row.get(2)?,
            )?;
            if record.is_owned_session() {
                return Ok(Some(record));
            }
        }
        Ok(None)
    })
}

/// Point writer with the established file-lock -> SQLite-CAS order. The
/// caller's preparation closure runs after the read transaction is released.
/// No process-wide session or receipt snapshot is loaded or published.
pub(crate) fn mutate_at<T>(
    path: &Path,
    key: &str,
    edit: impl FnOnce(&mut Option<CutexSessionRecord>) -> anyhow::Result<T>,
) -> anyhow::Result<(T, u64)> {
    let db = ensure_ready(path, true, false)?.context("session database missing")?;
    let parent = path.parent().context("session store has no parent")?;
    let lock = super::super::open_store_lock(&parent.join(super::super::CUTEX_SESSIONS_LOCK_FILE))?;
    FileExt::lock_exclusive(&lock)?;
    let (expected, original) = with_connection(&db, |connection| {
        let tx = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let revision = read_revision(&tx)?;
        let (instance, _) = head(&tx)?;
        let row: Option<(i64, String)> = tx
            .query_row(
                "SELECT rev,record_json FROM sessions WHERE key=?1",
                [key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let row = row
            .map(|(rev, json)| {
                decode(&db, &instance, key.into(), as_rev(rev)?, json).map(|r| (*r).clone())
            })
            .transpose()?;
        note(
            STORE,
            OpCounts {
                sql_statements: 1,
                ..Default::default()
            },
        );
        Ok((revision, row))
    })?;
    let mut replacement = original.clone();
    let result = edit(&mut replacement)?;
    let revision = with_connection(&db, |connection| {
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = read_revision(&tx)?;
        if current != expected {
            return Err(CutexSessionStoreRevisionConflict {
                expected,
                actual: current,
            }
            .into());
        }
        if replacement == original {
            tx.finish()?;
            return Ok(current);
        }
        let next = current
            .checked_add(1)
            .context("session revision overflow")?;
        let rev = i64::try_from(next)?;
        let mut base = CutexSessionStore::default();
        if let Some(record) = original {
            base.sessions.insert(key.into(), record);
        }
        let mut touched = TouchedRows::default();
        touched.sessions.insert(key.into());
        if let Some(record) = replacement {
            touched.values.sessions.insert(key.into(), record);
        }
        let mut changes = Changes::default();
        write_touched_sessions(&tx, &base, &touched, rev, &mut changes)?;
        tx.execute(
            "UPDATE meta SET value=?1 WHERE key='store_revision'",
            [next.to_string()],
        )?;
        note(
            STORE,
            OpCounts {
                sql_statements: 1,
                ..Default::default()
            },
        );
        tx.commit()?;
        WRITTEN_ROW_BYTES.fetch_add(changes.bytes, std::sync::atomic::Ordering::Relaxed);
        Ok(next)
    })?;
    Ok((result, revision))
}

pub(crate) fn put_receipt_at<M: ReceiptMap>(
    path: &Path,
    key: &str,
    value: Option<M::Value>,
) -> anyhow::Result<u64> {
    let json = value.as_ref().map(serde_json::to_string).transpose()?;
    let encoded_bytes = json.as_ref().map_or(0, |json| json.len() as u64);
    note(
        STORE,
        OpCounts {
            bytes_encoded: encoded_bytes,
            ..Default::default()
        },
    );
    let db = ensure_ready(path, true, false)?.context("session database missing")?;
    let parent = path.parent().context("session store has no parent")?;
    let lock = super::super::open_store_lock(&parent.join(super::super::CUTEX_SESSIONS_LOCK_FILE))?;
    FileExt::lock_exclusive(&lock)?;
    let (expected, old) = with_connection(&db, |connection| {
        let tx = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let revision = read_revision(&tx)?;
        let old: Option<String> = tx
            .query_row(
                "SELECT json FROM receipts WHERE map=?1 AND key=?2",
                params![M::NAME, key],
                |r| r.get(0),
            )
            .optional()?;
        note(
            STORE,
            OpCounts {
                sql_statements: 1,
                ..Default::default()
            },
        );
        Ok((revision, old))
    })?;
    let old = old
        .map(|json| {
            note(
                STORE,
                OpCounts {
                    bytes_read: json.len() as u64,
                    bytes_decoded: json.len() as u64,
                    rows_visited: 1,
                    rows_materialized: 1,
                    ..Default::default()
                },
            );
            serde_json::from_str::<M::Value>(&json)
        })
        .transpose()?;
    with_connection(&db, |connection| {
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = read_revision(&tx)?;
        if expected != current {
            return Err(CutexSessionStoreRevisionConflict {
                expected,
                actual: current,
            }
            .into());
        }
        if old == value {
            tx.finish()?;
            return Ok(current);
        }
        let next = current
            .checked_add(1)
            .context("session revision overflow")?;
        if let Some(json) = json {
            tx.execute("INSERT INTO receipts(map,key,rev,json) VALUES(?1,?2,?3,?4) ON CONFLICT(map,key) DO UPDATE SET rev=excluded.rev,json=excluded.json",params![M::NAME,key,i64::try_from(next)?,json])?;
            note(
                STORE,
                OpCounts {
                    sql_statements: 1,
                    ..Default::default()
                },
            );
        } else {
            tx.execute(
                "DELETE FROM receipts WHERE map=?1 AND key=?2",
                params![M::NAME, key],
            )?;
        }
        tx.execute(
            "UPDATE meta SET value=?1 WHERE key='store_revision'",
            [next.to_string()],
        )?;
        tx.commit()?;
        note(
            STORE,
            OpCounts {
                bytes_written: encoded_bytes,
                sql_statements: 1,
                ..Default::default()
            },
        );
        WRITTEN_ROW_BYTES.fetch_add(encoded_bytes, std::sync::atomic::Ordering::Relaxed);
        Ok(next)
    })
}

/// Host catalog candidates: SQL excludes retired, foreign and unmanaged rows
/// before materializing. Substring matching preserves Rust's Unicode behavior.
pub fn host_page_at(
    path: &Path,
    host: &str,
    after: Option<&str>,
    needle: &str,
    limit: usize,
) -> anyhow::Result<(Vec<Arc<CutexSessionRecord>>, Option<String>)> {
    let Some(db) = ensure_ready(path, false, false)? else {
        return Ok((Vec::new(), None));
    };
    with_connection(&db, |connection| {
        let tx = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let (instance, _) = head(&tx)?;
        let mut stmt=tx.prepare_cached("SELECT key,session_id,host_id,display_override,base_name FROM session_catalog_index INDEXED BY session_catalog_active WHERE archive_state='active'
            AND eligible=1 AND session_id>?1 ORDER BY session_id,key")?;
        let mut query = stmt.query([after.unwrap_or("")])?;
        let needle = needle.to_lowercase();
        let mut records = Vec::new();
        let mut bytes = 0;
        let mut next = None;
        while let Some(row) = query.next()? {
            let key: String = row.get(0)?;
            let id: String = row.get(1)?;
            let row_host: String = row.get(2)?;
            let override_name: Option<String> = row.get(3)?;
            let base_name: String = row.get(4)?;
            let name = override_name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(&base_name);
            note(
                STORE,
                OpCounts {
                    bytes_read: (key.len() + id.len() + row_host.len() + name.len()) as u64,
                    rows_visited: 1,
                    rows_materialized: 1,
                    ..Default::default()
                },
            );
            if !crate::runtime::lifecycle::cutex_session_host_is_local(&row_host, host)
                || (!id.to_lowercase().contains(&needle) && !name.to_lowercase().contains(&needle))
            {
                continue;
            }
            if records.len() >= limit.clamp(1, 200) {
                next = records
                    .last()
                    .map(|r: &Arc<CutexSessionRecord>| r.cutex_session_id.clone());
                break;
            }
            let (rev, json): (i64, String) = tx.query_row(
                "SELECT rev,record_json FROM sessions WHERE key=?1",
                [&key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            note(
                STORE,
                OpCounts {
                    sql_statements: 1,
                    ..Default::default()
                },
            );
            let row_bytes = json.len();
            if !records.is_empty() && bytes + row_bytes > 256 * 1024 {
                note(
                    STORE,
                    OpCounts {
                        bytes_read: row_bytes as u64,
                        rows_visited: 1,
                        ..Default::default()
                    },
                );
                next = records
                    .last()
                    .map(|r: &Arc<CutexSessionRecord>| r.cutex_session_id.clone());
                break;
            }
            bytes += row_bytes;
            records.push(decode(&db, &instance, key, as_rev(rev)?, json)?);
        }
        note(
            STORE,
            OpCounts {
                sql_statements: 1,
                ..Default::default()
            },
        );
        Ok((records, next))
    })
}

/// Page budget applies before decoding. The first oversize row is explicit;
/// callers must use a separately bounded body route rather than truncate it.
pub struct SessionPage {
    pub rows: Vec<Arc<CutexSessionRecord>>,
    pub revisions: Vec<u64>,
    pub next: Option<String>,
    pub change_cursor: ChangeCursor,
    pub oversize_first: bool,
}

pub fn page_at(
    path: &Path,
    retired: bool,
    after: Option<&str>,
    budget: PageBudget,
) -> anyhow::Result<Option<SessionPage>> {
    anyhow::ensure!(
        budget.max_rows > 0
            && budget.max_rows <= 1000
            && budget.max_bytes > 0
            && budget.max_bytes <= 1024 * 1024,
        "invalid session page budget"
    );
    let Some(db) = ensure_ready(path, false, false)? else {
        return Ok(None);
    };
    with_connection(&db, |connection| {
        let tx = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let (instance, head) = head(&tx)?;
        let spec = crate::store::manifest::spec(STORE);
        let cursor = after
            .map(|text| crate::store::ListCursor::decode(spec, text))
            .transpose()?;
        if let Some(cursor) = &cursor {
            cursor.check(&instance, spec, &head, &Scope::Host, retired as u64)?;
        }
        let (state, index) = if retired {
            ("retired", "sessions_lifecycle_key")
        } else {
            ("active", "sessions_active_key")
        };
        let mut stmt = tx.prepare_cached(&format!(
            "SELECT key,rev,record_json,cutex_session_id FROM sessions INDEXED BY {index} WHERE archive_state='{state}'
             AND key>?1 ORDER BY key LIMIT ?2"
        ))?;
        let mut query = stmt.query(params![
            cursor.as_ref().map(|c| c.after_key()).unwrap_or(""),
            (budget.max_rows + 1) as i64
        ])?;
        let mut rows = Vec::new();
        let mut revisions = Vec::new();
        let mut bytes = 0;
        let mut next = None;
        let mut last = String::new();
        let mut oversize_first = false;
        while let Some(row) = query.next()? {
            let json: String = row.get(2)?;
            if rows.len() >= budget.max_rows
                || (!rows.is_empty() && bytes + json.len() > budget.max_bytes)
            {
                note(
                    STORE,
                    OpCounts {
                        bytes_read: json.len() as u64,
                        rows_visited: 1,
                        ..Default::default()
                    },
                );
                next = Some(last);
                break;
            }
            oversize_first |= rows.is_empty() && json.len() > budget.max_bytes;
            bytes += json.len();
            last = row.get(0)?;
            let rev = as_rev(row.get(1)?)?;
            revisions.push(rev);
            rows.push(decode(&db, &instance, row.get(0)?, rev, json)?);
        }
        note(
            STORE,
            OpCounts {
                sql_statements: 1,
                ..Default::default()
            },
        );
        let next = next.map(|key| {
            crate::store::ListCursor::new(&instance, spec, &head, &Scope::Host, retired as u64, key)
                .encode()
        });
        Ok(Some(SessionPage {
            rows,
            revisions,
            next,
            oversize_first,
            change_cursor: ChangeCursor::new(&instance, spec, head.epoch, &Scope::Host, head.seq),
        }))
    })
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionChange {
    pub seq: u64,
    pub key: String,
    pub deleted: bool,
    pub revision: u64,
}
pub enum SessionChanges {
    Page {
        changes: Vec<SessionChange>,
        cursor: ChangeCursor,
        watermark: u64,
        more: bool,
        oversize_first: bool,
    },
    Resync {
        reason: crate::store::ResyncReason,
        watermark: u64,
    },
}

pub fn changes_at(
    path: &Path,
    cursor: &ChangeCursor,
    budget: PageBudget,
) -> anyhow::Result<Option<SessionChanges>> {
    anyhow::ensure!(
        budget.max_rows > 0
            && budget.max_rows <= 1000
            && budget.max_bytes > 0
            && budget.max_bytes <= 1024 * 1024,
        "invalid session delta budget"
    );
    let Some(db) = ensure_ready(path, false, false)? else {
        return Ok(None);
    };
    with_connection(&db, |connection| {
        let tx = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let (instance, head) = head(&tx)?;
        let spec = crate::store::manifest::spec(STORE);
        if let Some(reason) = cursor.resync_reason(&instance, spec, &head, &Scope::Host) {
            return Ok(Some(SessionChanges::Resync {
                reason,
                watermark: head.seq,
            }));
        }
        let mut stmt=tx.prepare_cached("SELECT seq,session_id,deleted,rev FROM session_changes WHERE map='' AND seq>?1 ORDER BY seq LIMIT ?2")?;
        let mut query = stmt.query(params![
            cursor.after_seq() as i64,
            (budget.max_rows + 1) as i64
        ])?;
        let mut changes = Vec::new();
        let mut bytes = 2;
        let mut visited = 0;
        let mut read_bytes = 0;
        let mut more = false;
        let mut oversize_first = false;
        let mut after = head.seq;
        while let Some(row) = query.next()? {
            let key: String = row.get(1)?;
            visited += 1;
            read_bytes += key.len() + 24;
            let change = SessionChange {
                seq: as_rev(row.get(0)?)?,
                key,
                deleted: row.get(2)?,
                revision: as_rev(row.get(3)?)?,
            };
            let encoded = serde_json::to_vec(&change)?;
            note(
                STORE,
                OpCounts {
                    bytes_encoded: encoded.len() as u64,
                    ..Default::default()
                },
            );
            let row_bytes = encoded.len() + usize::from(!changes.is_empty());
            if changes.len() >= budget.max_rows
                || (!changes.is_empty() && bytes + row_bytes > budget.max_bytes)
            {
                more = true;
                break;
            }
            oversize_first |= changes.is_empty() && bytes + row_bytes > budget.max_bytes;
            bytes += row_bytes;
            after = change.seq;
            changes.push(change);
        }
        if !more {
            after = head.seq;
        }
        note(
            STORE,
            OpCounts {
                sql_statements: 1,
                bytes_read: read_bytes as u64,
                rows_visited: visited,
                rows_materialized: changes.len() as u64,
                ..Default::default()
            },
        );
        Ok(Some(SessionChanges::Page {
            changes,
            cursor: ChangeCursor::new(&instance, spec, head.epoch, &Scope::Host, after),
            watermark: head.seq,
            more,
            oversize_first,
        }))
    })
}

#[cfg(test)]
#[path = "indexed_tests.rs"]
mod tests;
