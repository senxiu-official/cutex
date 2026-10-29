//! Narrow observer read for receipt presentation, not the session writer opener.
//! Existing schema v1 only; absent/legacy/incomplete stores remain unknown.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use super::super::read_views::{CurrentObserver, ReceiptReadContext};

pub(super) fn attach(mut context: ReceiptReadContext) -> ReceiptReadContext {
    if context.is_empty() {
        return context;
    }
    let ids = context
        .values()
        .map(|input| input.snapshot.to_cutex_session_id.clone())
        .collect();
    let observers = crate::session::store::cutex_sessions_path()
        .and_then(|path| read_at(&path, &ids))
        .unwrap_or_default();
    for input in context.values_mut() {
        // Never accept an observer copied from retained message evidence.
        input.observer = observers.get(&input.snapshot.to_cutex_session_id).cloned();
    }
    context
}

fn read_at(
    logical_path: &Path,
    ids: &BTreeSet<String>,
) -> anyhow::Result<BTreeMap<String, CurrentObserver>> {
    use crate::session::store::{is_session_store_sentinel, session_database_path};
    crate::config::paths::ensure_not_real_state(logical_path)?;
    let mut result = BTreeMap::new();
    let database = session_database_path(logical_path);
    if ids.is_empty() || !database.is_file() {
        return Ok(result);
    }
    // A missing/non-sentinel JSON file requires session-store reconciliation;
    // presentation must not import it, repair it or use a possibly stale DB.
    let sentinel = match std::fs::metadata(logical_path) {
        Ok(meta) if meta.len() < 64 * 1024 => std::fs::read(logical_path)?,
        _ => return Ok(result),
    };
    if !is_session_store_sentinel(&sentinel) {
        return Ok(result);
    }
    let mut db = Connection::open_with_flags(
        &database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.busy_timeout(Duration::from_millis(250))?;
    let tx = db.transaction()?;
    let version: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let schema: Option<String> = tx
        .query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    anyhow::ensure!(
        version == 1 && schema.as_deref() == Some("1"),
        "unsupported observer schema"
    );
    // Extract only identity fields in SQLite. Do not materialize launch specs,
    // deployment paths, profiles, receipt maps, or other session payload data.
    let mut query = tx.prepare("SELECT json_extract(record_json,'$.cutex_session_id'),
        json_extract(record_json,'$.runtime_backend'), json_extract(record_json,'$.lifecycle'),
        json_extract(record_json,'$.runtime_generation'), json_extract(record_json,'$.current_runtime_agent_id'),
        json_extract(record_json,'$.claude_session.thread_id') FROM sessions WHERE key=?1")?;
    for id in ids.iter().take(super::MAX_CONTEXT_MESSAGES) {
        let row = query
            .query_row([id], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            })
            .optional()?;
        let Some((
            Some(cutex_id),
            Some(backend),
            Some(lifecycle),
            Some(generation),
            Some(runtime),
            Some(thread),
        )) = row
        else {
            continue;
        };
        if cutex_id != *id
            || backend != "claude_subscription"
            || lifecycle != "active"
            || generation < 0
            || generation > 9_007_199_254_740_991
            || runtime.is_empty()
            || thread.is_empty()
        {
            continue;
        }
        result.insert(
            id.clone(),
            CurrentObserver {
                cutex_session_id: cutex_id,
                thread_id: thread,
                runtime_generation: generation as u64,
            },
        );
    }
    drop(query);
    tx.commit()?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::store::tests::TARGET;

    #[test]
    fn receipt_observer_reads_canonical_private_store_without_migration_or_state_writes() {
        let root = std::env::temp_dir().join(format!(
            "receipt-observer-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("cutex-sessions.json");
        let ids = BTreeSet::from([TARGET.into()]);
        assert!(read_at(&path, &ids).unwrap().is_empty());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        std::fs::write(&path, b"{\"legacy\":true}").unwrap();
        assert!(read_at(&path, &ids).unwrap().is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"legacy\":true}");
        std::fs::remove_file(&path).unwrap();
        let sessions = super::super::tests::sessions();
        crate::session::store::save_cutex_session_store_to_path(&path, &sessions).unwrap();
        let sentinel = std::fs::read(&path).unwrap();
        let db_path = crate::session::store::session_database_path(&path);
        let db = Connection::open(&db_path).unwrap();
        let before: String = db
            .query_row(
                "SELECT value FROM meta WHERE key='store_revision'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let json_before: String = db
            .query_row(
                "SELECT record_json FROM sessions WHERE key=?1",
                [TARGET],
                |row| row.get(0),
            )
            .unwrap();
        let observers = read_at(&path, &ids).unwrap();
        assert_eq!(observers[TARGET].runtime_generation, 9);
        assert_eq!(observers[TARGET].thread_id, "thread");
        assert_eq!(std::fs::read(&path).unwrap(), sentinel);
        assert_eq!(
            db.query_row::<String, _, _>(
                "SELECT value FROM meta WHERE key='store_revision'",
                [],
                |row| row.get(0)
            )
            .unwrap(),
            before
        );
        assert_eq!(
            db.query_row::<String, _, _>(
                "SELECT record_json FROM sessions WHERE key=?1",
                [TARGET],
                |row| row.get(0)
            )
            .unwrap(),
            json_before
        );
        for field in [
            "cutex_session_id",
            "runtime_backend",
            "lifecycle",
            "runtime_generation",
            "current_runtime_agent_id",
            "claude_session",
        ] {
            let mut incomplete: serde_json::Value = serde_json::from_str(&json_before).unwrap();
            incomplete.as_object_mut().unwrap().remove(field);
            db.execute(
                "UPDATE sessions SET record_json=?1 WHERE key=?2",
                rusqlite::params![incomplete.to_string(), TARGET],
            )
            .unwrap();
            assert!(
                read_at(&path, &ids).unwrap().is_empty(),
                "missing {field} must not be guessed"
            );
        }
        let mut newer: serde_json::Value = serde_json::from_str(&json_before).unwrap();
        newer["runtime_generation"] = serde_json::json!(10);
        db.execute(
            "UPDATE sessions SET record_json=?1 WHERE key=?2",
            rusqlite::params![newer.to_string(), TARGET],
        )
        .unwrap();
        assert_eq!(read_at(&path,&ids).unwrap()[TARGET].runtime_generation,10,
            "new observation must read current DB identity instead of retained message evidence/cache");
        // Existing DB without a valid marker must not repair the marker or
        // promote its possibly stale identity to trusted current observation.
        std::fs::write(&path, b"{\"legacy\":true}").unwrap();
        assert!(read_at(&path, &ids).unwrap().is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"legacy\":true}");
    }
}
