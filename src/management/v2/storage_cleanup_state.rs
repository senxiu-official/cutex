//! Additive cleanup receipts. No session, configuration or native history format changes.
use anyhow::Context;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::{
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    time::Duration,
};

pub(super) fn database(home: &Path) -> anyhow::Result<Connection> {
    let directory = home.join("runtime/storage");
    fs::create_dir_all(&directory)?;
    let db = Connection::open_with_flags(
        directory.join("storage.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
            | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
            | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.execute_batch("PRAGMA journal_mode=WAL;
        CREATE TABLE IF NOT EXISTS cleanup_previews(id TEXT PRIMARY KEY, expires INTEGER NOT NULL, value TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS cleanups(id TEXT PRIMARY KEY, request_id TEXT NOT NULL UNIQUE, preview_id TEXT NOT NULL UNIQUE, state TEXT NOT NULL, created TEXT NOT NULL, value TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS cleanup_items(run_id TEXT NOT NULL, id TEXT NOT NULL, state TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY(run_id,id));
        CREATE TABLE IF NOT EXISTS cleanup_audit(id TEXT PRIMARY KEY, value TEXT NOT NULL, exported INTEGER NOT NULL DEFAULT 0);")?;
    Ok(db)
}

pub(super) fn save_preview(home: &Path, value: &Value, expires: i64) -> anyhow::Result<()> {
    let mut db = database(home)?;
    let tx = db.transaction()?;
    // Expired consent metadata is disposable; audit and run receipts remain.
    tx.execute(
        "DELETE FROM cleanup_previews WHERE expires<?1",
        [chrono::Utc::now().timestamp()],
    )?;
    tx.execute(
        "INSERT INTO cleanup_previews(id,expires,value) VALUES(?1,?2,?3)",
        params![
            value["previewId"].as_str().context("preview ID")?,
            expires,
            value.to_string()
        ],
    )?;
    tx.commit()?;
    Ok(())
}

pub(super) fn preview(home: &Path, id: &str) -> anyhow::Result<Option<Value>> {
    read_value(
        &database(home)?,
        "SELECT value FROM cleanup_previews WHERE id=?1",
        id,
    )
}

fn read_value(db: &Connection, query: &str, id: &str) -> anyhow::Result<Option<Value>> {
    let text: Option<String> = db.query_row(query, [id], |row| row.get(0)).optional()?;
    text.map(|text| serde_json::from_str(&text).map_err(Into::into))
        .transpose()
}

pub(super) fn run(home: &Path, id: &str) -> anyhow::Result<Option<Value>> {
    read_value(
        &database(home)?,
        "SELECT value FROM cleanups WHERE id=?1",
        id,
    )
}

pub(super) fn by_request(home: &Path, id: &str) -> anyhow::Result<Option<Value>> {
    read_value(
        &database(home)?,
        "SELECT value FROM cleanups WHERE request_id=?1",
        id,
    )
}
pub(super) fn by_preview(home: &Path, id: &str) -> anyhow::Result<Option<Value>> {
    read_value(
        &database(home)?,
        "SELECT value FROM cleanups WHERE preview_id=?1",
        id,
    )
}

pub(super) fn history(home: &Path, unfinished_only: bool) -> anyhow::Result<Vec<Value>> {
    let db = database(home)?;
    let mut statement = db.prepare(if unfinished_only {
        "SELECT value FROM cleanups WHERE state='running' OR (state='failed' AND EXISTS (SELECT 1 FROM cleanup_items i WHERE i.run_id=cleanups.id AND i.state='failed' AND json_extract(i.value,'$.retryPhase')='deleting')) ORDER BY created,id"
    } else {
        "SELECT value FROM cleanups ORDER BY created DESC,id DESC LIMIT 50"
    })?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
}

/// The durable receipt and audit outbox are committed before any destructive work.
/// A unique preview prevents a second request from consuming the same consent.
pub(super) fn begin(home: &Path, value: &Value) -> anyhow::Result<()> {
    let mut db = database(home)?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute("INSERT INTO cleanups(id,request_id,preview_id,state,created,value) VALUES(?1,?2,?3,'running',?4,?5)", params![value["runId"].as_str(), value["requestId"].as_str(), value["previewId"].as_str(),value["createdAt"].as_str(),value.to_string()])?;
    save_items(&tx, value)?;
    enqueue_audit(&tx, value, "start")?;
    tx.commit()?;
    flush_audit(home)
}

fn save_items(db: &Connection, run: &Value) -> anyhow::Result<()> {
    for item in run["items"].as_array().context("cleanup items")? {
        db.execute("INSERT INTO cleanup_items(run_id,id,state,value) VALUES(?1,?2,?3,?4) ON CONFLICT(run_id,id) DO UPDATE SET state=excluded.state,value=excluded.value", params![run["runId"].as_str(), item["id"].as_str(), item["state"].as_str(),item.to_string()])?;
    }
    Ok(())
}

pub(super) fn save_run(home: &Path, value: &Value, terminal: bool) -> anyhow::Result<()> {
    persist_run(home, value, terminal.then_some("end"))
}
pub(super) fn start_attempt(home: &Path, value: &Value) -> anyhow::Result<()> {
    persist_run(home, value, Some("start"))
}
/// Error visibility must not depend on the JSONL projection being writable.
pub(super) fn record_pause(home: &Path, message: &str) -> anyhow::Result<Vec<Value>> {
    let canonical = home.canonicalize()?;
    let home = canonical.as_path();
    let _guard = super::storage_cleanup_fs::pin_storage_state(home)?;
    let db = database(home)?;
    // Update only the current JSON field, atomically with the unfinished-state
    // predicate. A queued worker may have completed after our worker lock fell.
    let mut statement = db.prepare("UPDATE cleanups SET value=json_set(value,'$.error',?1) WHERE state='running' OR (state='failed' AND EXISTS (SELECT 1 FROM cleanup_items i WHERE i.run_id=cleanups.id AND i.state='failed' AND json_extract(i.value,'$.retryPhase')='deleting')) RETURNING value")?;
    let rows = statement.query_map([message], |row| row.get::<_, String>(0))?;
    rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
}
fn persist_run(home: &Path, value: &Value, audit_phase: Option<&str>) -> anyhow::Result<()> {
    let mut db = database(home)?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let updated = tx.execute(
        "UPDATE cleanups SET state=?2,value=?3 WHERE id=?1",
        params![
            value["runId"].as_str(),
            value["state"].as_str(),
            value.to_string()
        ],
    )?;
    anyhow::ensure!(updated == 1, "cleanup receipt disappeared");
    save_items(&tx, value)?;
    if let Some(phase) = audit_phase {
        enqueue_audit(&tx, value, phase)?;
    }
    tx.commit()?;
    flush_audit(home)
}

fn enqueue_audit(db: &Connection, run: &Value, phase: &str) -> anyhow::Result<()> {
    let attempt = run["attempt"].as_u64().unwrap_or(1);
    let run_id = run["runId"].as_str().context("run ID")?;
    let id = if attempt == 1 {
        format!("{run_id}:{phase}")
    } else {
        format!("{run_id}:attempt-{attempt}:{phase}")
    };
    let items:Vec<Value>=run["items"].as_array().context("audit items")?.iter().map(|item|serde_json::json!({"id":item["id"],"path":item["path"],"state":item["state"],"outcome":item["outcome"],"reason":item["reason"],"code":item["code"],"freedBytes":item["freedBytes"],"retainedPath":item["retainedPath"]})).collect();
    let value = serde_json::json!({"eventId":id,"phase":phase,"attempt":attempt,"at":chrono::Utc::now().to_rfc3339(),"runId":run["runId"],"requestId":run["requestId"],"action":run["action"],"actor":run["actor"],"state":run["state"],"freedBytes":run["freedBytes"],"items":items});
    db.execute(
        "INSERT OR IGNORE INTO cleanup_audit(id,value) VALUES(?1,?2)",
        params![id, value.to_string()],
    )?;
    Ok(())
}

/// At-least-once JSONL projection of the durable audit table. Stable event IDs
/// make a crash after fsync/before marking exported harmless to audit readers.
pub(super) fn flush_audit(home: &Path) -> anyhow::Result<()> {
    use fs2::FileExt;
    let db = database(home)?;
    let mut statement =
        db.prepare("SELECT id,value FROM cleanup_audit WHERE exported=0 ORDER BY rowid")?;
    let rows: Vec<(String, String)> = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    if rows.is_empty() {
        return Ok(());
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(home.join("runtime/storage/cleanups.jsonl"))?;
    file.lock_exclusive()?;
    if file.metadata()?.len() > 0 {
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0u8; 1];
        file.read_exact(&mut last)?;
        // Preserve a torn final record and start the replay on its own line.
        if last[0] != b'\n' {
            file.write_all(b"\n")?;
        }
    }
    for (id, value) in rows {
        writeln!(file, "{value}")?;
        file.sync_all()?;
        db.execute("UPDATE cleanup_audit SET exported=1 WHERE id=?1", [id])?;
    }
    FileExt::unlock(&file)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (std::path::PathBuf, Value) {
        let home = std::env::temp_dir().join(format!("cleanup-state-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&home).unwrap();
        let run = serde_json::json!({"runId":"run","requestId":"request","previewId":"preview","action":"old-logs","createdAt":"2026-10-24T00:00:00Z","state":"running","freedBytes":0,"actor":{"principal":"owner"},"items":[{"id":"item","state":"queued"}]});
        (home, run)
    }
    #[test]
    fn receipt_and_projection_survive_reopen_and_do_not_consume_preview_twice() {
        let (home, mut value) = fixture();
        begin(&home, &value).unwrap();
        let mut duplicate = value.clone();
        duplicate["runId"] = "other".into();
        duplicate["requestId"] = "other".into();
        assert!(begin(&home, &duplicate).is_err());
        assert_eq!(by_request(&home, "request").unwrap(), Some(value.clone()));
        assert_eq!(history(&home, true).unwrap().len(), 1);
        value["state"] = "completed".into();
        value["items"][0]["state"] = "deleted".into();
        save_run(&home, &value, true).unwrap();
        save_run(&home, &value, true).unwrap();
        assert!(history(&home, true).unwrap().is_empty());
        assert!(record_pause(&home, "late worker error").unwrap().is_empty());
        assert_eq!(run(&home, "run").unwrap(), Some(value.clone()));
        let audit = fs::read_to_string(home.join("runtime/storage/cleanups.jsonl")).unwrap();
        let events: Vec<Value> = audit
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(!audit.starts_with('\n') && !audit.contains("\n\n"));
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["phase"], "start");
        assert_eq!(events[1]["phase"], "end");
        fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn pause_reason_is_durable_even_when_the_audit_projection_is_unavailable() {
        let (home, value) = fixture();
        begin(&home, &value).unwrap();
        let audit = home.join("runtime/storage/cleanups.jsonl");
        let saved = audit.with_extension("saved");
        fs::rename(&audit, &saved).unwrap();
        fs::create_dir(&audit).unwrap();
        assert_eq!(
            record_pause(&home, "audit output unavailable").unwrap()[0]["error"],
            "audit output unavailable"
        );
        assert_eq!(
            run(&home, "run").unwrap().unwrap()["error"],
            "audit output unavailable"
        );
        fs::remove_dir(&audit).unwrap();
        fs::rename(saved, audit).unwrap();
        fs::remove_dir_all(home).unwrap();
    }
}
