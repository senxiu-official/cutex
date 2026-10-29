//! Manual compaction of named, briefly opened cutex databases.
//! Never targets native databases or databases with daemon-held connections.
use anyhow::Context;
use rusqlite::{config::DbConfig, Connection, OpenFlags};
use std::{path::Path, time::Duration};

pub(super) const DATABASES: &[(&str, &str)] = &[
    // These three stores contain only a PathBuf and open/drop a connection per
    // operation (documents::DocumentStore, artifacts::ArtifactStore and
    // documents::archive::RecordArchive). Their writers wait up to ten seconds.
    ("documents", "runtime/documents/documents-v1.sqlite3"),
    ("artifacts", "runtime/artifacts/artifacts-v1.sqlite3"),
    ("archive", "archive/index-v1.sqlite3"),
];
// Excluded even when no agent runtime is online: session/store/sqlite.rs pools
// connections, workflows::WorkflowStore retains Mutex<Connection> in the service,
// and the project-events projector retains Journal::db. A separate VACUUM must
// never compete with those owners; they need subsystem-owned maintenance first.
const COMPACTION_BUDGET: Duration = Duration::from_secs(1);

fn allowed(relative: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(
        DATABASES
            .iter()
            .any(|(_, path)| relative == Path::new(path)),
        "database is not compactable"
    );
    Ok(())
}

fn exclusive(path: &Path) -> anyhow::Result<Connection> {
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    db.busy_timeout(Duration::ZERO)?;
    // Preview's availability probe must not checkpoint a pending WAL merely
    // because its temporary connection was the last one to close. Execution
    // checkpoints explicitly only after it has acquired exclusive ownership.
    db.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)?;
    // Set EXCLUSIVE before the first database access. In WAL this also needs
    // exclusive shared-memory access: an idle but live WAL connection refuses
    // the preflight, even without a write transaction. COMMIT retains the lock
    // until this connection closes, so VACUUM cannot race a newly active writer.
    db.execute_batch("PRAGMA locking_mode=EXCLUSIVE; BEGIN EXCLUSIVE; COMMIT;")
        .context("database has another connection or transaction; retry when idle")?;
    Ok(db)
}

pub(super) fn reclaimable(home: &Path, relative: &Path) -> anyhow::Result<u64> {
    allowed(relative)?;
    let _path_guard = super::storage_cleanup_fs::pin_regular(home, relative)?;
    // Preview must test SQLite availability too, not just agent runtime state.
    // Drop immediately after metadata queries; execute repeats this preflight.
    let db = exclusive(&home.join(relative))?;
    let pages: i64 = db.pragma_query_value(None, "freelist_count", |row| row.get(0))?;
    let page_size: i64 = db.pragma_query_value(None, "page_size", |row| row.get(0))?;
    Ok(u64::try_from(pages)?.saturating_mul(u64::try_from(page_size)?))
}

pub(super) fn compact(home: &Path, relative: &Path) -> anyhow::Result<u64> {
    allowed(relative)?;
    let _path_guard = super::storage_cleanup_fs::pin_regular(home, relative)?;
    let path = home.join(relative);
    let before = size(&path);
    let db = exclusive(&path)?;
    let started = std::time::Instant::now();
    db.progress_handler(
        1_000,
        // A cooperative deadline, well below the allowed stores' ten-second
        // writer timeout. SQLITE_INTERRUPT rolls VACUUM back; closing this
        // connection releases the exclusive lock on every error path too.
        Some(move || started.elapsed() > COMPACTION_BUDGET),
    )?;
    let check: String = db.query_row("PRAGMA quick_check(1)", [], |row| row.get(0))?;
    anyhow::ensure!(check == "ok", "database health check failed");
    checkpoint(&db)?;
    db.execute_batch("VACUUM;")?;
    // In WAL mode VACUUM writes the compacted image to WAL, leaving the old
    // large main file in place. NO_CKPT_ON_CLOSE is intentional for previews,
    // so execution must explicitly apply and truncate this final WAL before
    // releasing ownership or measuring reclaimed bytes.
    checkpoint(&db)?;
    db.close()
        .map_err(|(_, error)| error)
        .context("close compacted database")?;
    Ok(before.saturating_sub(size(&path)))
}

fn checkpoint(db: &Connection) -> anyhow::Result<()> {
    // SQLite reports an incomplete/busy checkpoint as a result row, not an SQL
    // error. Never report completion based merely on execute_batch succeeding.
    let busy: i64 = db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
    anyhow::ensure!(busy == 0, "database checkpoint was busy");
    Ok(())
}

fn size(path: &Path) -> u64 {
    [
        path.to_path_buf(),
        std::path::PathBuf::from(format!("{}-wal", path.display())),
    ]
    .iter()
    .filter_map(|path| std::fs::metadata(path).ok())
    .map(|meta| meta.len())
    .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compaction_keeps_records_and_refuses_busy_or_unlisted_databases() {
        let home = std::env::temp_dir().join(format!("cleanup-vacuum-{}", uuid::Uuid::new_v4()));
        let relative = Path::new("archive/index-v1.sqlite3");
        std::fs::create_dir_all(home.join(relative).parent().unwrap()).unwrap();
        let db = Connection::open(home.join(relative)).unwrap();
        db.execute_batch("CREATE TABLE records(value BLOB); INSERT INTO records VALUES(zeroblob(1000000)); INSERT INTO records VALUES('keep'); DELETE FROM records WHERE rowid=1;").unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert!(compact(&home, relative).is_err());
        db.execute_batch("ROLLBACK").unwrap();
        drop(db);
        assert!(compact(&home, relative).unwrap() > 0);
        let db = Connection::open(home.join(relative)).unwrap();
        assert_eq!(
            db.query_row("SELECT value FROM records", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            "keep"
        );
        assert!(compact(&home, Path::new("codex-home/state_5.sqlite")).is_err());
        drop(db);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn idle_live_wal_connection_blocks_preview_and_compaction_and_keeps_writing() {
        for (_, name) in DATABASES {
            let home =
                std::env::temp_dir().join(format!("cleanup-vacuum-wal-{}", uuid::Uuid::new_v4()));
            let relative = Path::new(name);
            let path = home.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            {
                let seed = Connection::open(&path).unwrap();
                seed.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE records(value BLOB); INSERT INTO records VALUES(zeroblob(1000000)); INSERT INTO records VALUES('keep'); DELETE FROM records WHERE rowid=1;").unwrap();
            }
            // A live daemon-like second connection, with no active transaction
            // and no write lock. Reading once attaches it to WAL shared memory.
            let writer = Connection::open(&path).unwrap();
            writer.busy_timeout(Duration::ZERO).unwrap();
            let journal: String = writer
                .pragma_query_value(None, "journal_mode", |row| row.get(0))
                .unwrap();
            assert_eq!(journal, "wal");
            assert_eq!(
                writer
                    .query_row("SELECT count(*) FROM records", [], |row| row
                        .get::<_, i64>(0))
                    .unwrap(),
                1
            );
            assert!(writer.is_autocommit());
            for result in [reclaimable(&home, relative), compact(&home, relative)] {
                let error = result.expect_err("idle WAL owner must block separate compaction");
                let sqlite = error
                    .downcast_ref::<rusqlite::Error>()
                    .expect("SQLite availability failure");
                assert!(
                    matches!(
                        sqlite.sqlite_error_code(),
                        Some(
                            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                        )
                    ),
                    "{error:#}"
                );
                // No timeout/retry: the same owning connection must still be
                // able to commit immediately after the refused maintenance.
                writer
                    .execute("INSERT INTO records VALUES('writer survived')", [])
                    .unwrap();
                assert!(writer.is_autocommit());
            }
            assert_eq!(
                writer
                    .query_row("SELECT count(*) FROM records", [], |row| row
                        .get::<_, i64>(0))
                    .unwrap(),
                3
            );
            drop(writer);
            assert!(reclaimable(&home, relative).unwrap() > 0);
            let before = size(&path);
            let freed = compact(&home, relative).unwrap();
            let after = size(&path);
            assert!(
                after < before,
                "physical main + WAL bytes must shrink: {before} -> {after}"
            );
            assert_eq!(
                freed,
                before - after,
                "receipt must match physical file sizes"
            );
            let wal = std::path::PathBuf::from(format!("{}-wal", path.display()));
            assert_eq!(
                std::fs::metadata(wal).map(|m| m.len()).unwrap_or(0),
                0,
                "compacted WAL must be applied and truncated before returning"
            );
            let reader = Connection::open(&path).unwrap();
            assert_eq!(
                reader
                    .query_row("SELECT count(*) FROM records", [], |row| row
                        .get::<_, i64>(0))
                    .unwrap(),
                3
            );
            drop(reader);
            std::fs::remove_dir_all(home).unwrap();
        }
    }

    #[test]
    fn daemon_owned_and_native_databases_are_excluded_even_when_idle() {
        let home =
            std::env::temp_dir().join(format!("cleanup-vacuum-excluded-{}", uuid::Uuid::new_v4()));
        for name in [
            "cutex-sessions.sqlite3",
            "runtime/workflows/runs.sqlite3",
            "runtime/project-events/journal.sqlite3",
            "runtime/storage/cleanup.sqlite3",
            "codex-home/state_5.sqlite",
        ] {
            let relative = Path::new(name);
            let path = home.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let writer = Connection::open(&path).unwrap();
            writer.busy_timeout(Duration::ZERO).unwrap();
            writer.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE records(value); INSERT INTO records VALUES('keep');").unwrap();
            assert!(writer.is_autocommit());
            for result in [reclaimable(&home, relative), compact(&home, relative)] {
                assert_eq!(
                    result.unwrap_err().to_string(),
                    "database is not compactable"
                );
                writer
                    .execute("INSERT INTO records VALUES('writer survived')", [])
                    .unwrap();
            }
            assert_eq!(
                writer
                    .query_row("SELECT count(*) FROM records", [], |row| row
                        .get::<_, i64>(0))
                    .unwrap(),
                3
            );
        }
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn preview_does_not_checkpoint_pending_wal_into_database() {
        let home =
            std::env::temp_dir().join(format!("cleanup-vacuum-preview-{}", uuid::Uuid::new_v4()));
        let relative = Path::new("archive/index-v1.sqlite3");
        let path = home.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        {
            let seed = Connection::open(&path).unwrap();
            seed.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)
                .unwrap();
            seed.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE records(value); INSERT INTO records VALUES('pending WAL record');").unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        assert!(
            std::fs::metadata(format!("{}-wal", path.display()))
                .unwrap()
                .len()
                > 0
        );
        reclaimable(&home, relative).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let reader = Connection::open(&path).unwrap();
        assert_eq!(
            reader
                .query_row("SELECT value FROM records", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "pending WAL record"
        );
        drop(reader);
        std::fs::remove_dir_all(home).unwrap();
    }
}
