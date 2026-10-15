//! Record archive (`<cutex home>/archive/`, Management `record-archive`):
//! closed delegations and finished agent messages, with the text of their
//! documents, after `documents.closedRetentionDays`.
//!
//! - `records-YYYY-MM.jsonl.gz` (month of the record's close): a multi-member
//!   gzip file, one member per record, so appending never rewrites it and
//!   one record is read by decompressing only its member.
//! - `index-v1.sqlite3`: record id → (file, offset, length) and the facts
//!   the list shows (kind, title, participants, closedAt, archivedAt).
//! - Whole months are deleted once the newest record in them is older than
//!   `documents.archiveRetentionDays` (0 keeps them).

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// One archived record (index row).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveEntry {
    pub record_id: String,
    /// `ask` or `message`.
    pub kind: String,
    pub month: String,
    pub title: String,
    pub participants: Vec<String>,
    pub closed_at: String,
    pub archived_at: String,
    pub bytes: u64,
}

/// What [`RecordArchive::append`] stores.
pub struct NewArchiveRecord {
    pub record_id: String,
    pub kind: String,
    pub title: String,
    pub participants: Vec<String>,
    pub closed_at: DateTime<Utc>,
    /// The record itself (ask with answers…, or the bus message).
    pub record: Value,
    /// `[{documentId, title, bytes, text}]` of its documents.
    pub documents: Vec<Value>,
}

/// List filter (`GET /v2/record-archive`).
#[derive(Clone, Debug, Default)]
pub struct ArchiveFilter {
    pub kind: Option<String>,
    pub month: Option<String>,
    pub involving: Option<String>,
    pub query: Option<String>,
}

pub struct RecordArchive {
    dir: PathBuf,
}

impl RecordArchive {
    pub fn open_default() -> anyhow::Result<Self> {
        Ok(Self::at(crate::config::paths::config_dir()?.join("archive")))
    }

    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn connection(&self) -> anyhow::Result<Connection> {
        crate::management::client_tokens::ensure_private_dir(&self.dir)?;
        let path = self.dir.join("index-v1.sqlite3");
        let connection = Connection::open(&path).with_context(|| format!("open {}", path.display()))?;
        connection.busy_timeout(std::time::Duration::from_secs(10))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS records (
                record_id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                month TEXT NOT NULL,
                file TEXT NOT NULL,
                offset INTEGER NOT NULL,
                length INTEGER NOT NULL,
                title TEXT NOT NULL,
                participants TEXT NOT NULL,
                closed_at TEXT NOT NULL,
                archived_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS records_month ON records (month);",
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for suffix in ["", "-wal", "-shm"] {
                let file = PathBuf::from(format!("{}{suffix}", path.display()));
                if file.exists() {
                    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))?;
                }
            }
        }
        Ok(connection)
    }

    /// Append one record (idempotent by record id).
    pub fn append(&self, record: NewArchiveRecord, now: DateTime<Utc>) -> anyhow::Result<()> {
        let connection = self.connection()?;
        let exists: bool =
            connection.query_row("SELECT EXISTS(SELECT 1 FROM records WHERE record_id = ?1)", [&record.record_id], |row| row.get(0))?;
        if exists {
            return Ok(());
        }
        let month = record.closed_at.format("%Y-%m").to_string();
        let file = format!("records-{month}.jsonl.gz");
        let line = serde_json::to_vec(&json!({
            "recordId": record.record_id,
            "kind": record.kind,
            "title": record.title,
            "participants": record.participants,
            "closedAt": crate::asks::now_rfc3339(record.closed_at),
            "archivedAt": crate::asks::now_rfc3339(now),
            "record": record.record,
            "documents": record.documents,
        }))?;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&line)?;
        encoder.write_all(b"\n")?;
        let member = encoder.finish()?;
        let path = self.dir.join(&file);
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut handle = options.open(&path).with_context(|| format!("open {}", path.display()))?;
        let offset = handle.seek(SeekFrom::End(0))?;
        handle.write_all(&member)?;
        handle.sync_data()?;
        connection.execute(
            "INSERT INTO records (record_id, kind, month, file, offset, length, title, participants, closed_at, archived_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                record.record_id,
                record.kind,
                month,
                file,
                offset as i64,
                member.len() as i64,
                crate::asks::truncate_chars(&record.title, 200),
                serde_json::to_string(&record.participants)?,
                crate::asks::now_rfc3339(record.closed_at),
                crate::asks::now_rfc3339(now),
            ],
        )?;
        Ok(())
    }

    /// One archived record with its documents' text.
    pub fn get(&self, record_id: &str) -> anyhow::Result<Option<Value>> {
        let connection = self.connection()?;
        let Some((file, offset, length)) = connection
            .query_row("SELECT file, offset, length FROM records WHERE record_id = ?1", [record_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?))
            })
            .optional()?
        else {
            return Ok(None);
        };
        let mut handle = std::fs::File::open(self.dir.join(&file))?;
        handle.seek(SeekFrom::Start(offset as u64))?;
        let mut member = vec![0; length as usize];
        handle.read_exact(&mut member)?;
        let mut text = Vec::new();
        flate2::read::GzDecoder::new(member.as_slice()).read_to_end(&mut text)?;
        Ok(Some(serde_json::from_slice(&text)?))
    }

    pub fn list(&self, filter: &ArchiveFilter, limit: usize, offset: usize) -> anyhow::Result<(Vec<ArchiveEntry>, bool)> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT record_id, kind, month, title, participants, closed_at, archived_at, length FROM records ORDER BY closed_at DESC",
        )?;
        let rows = statement.query_map([], |row| {
            let participants: String = row.get(4)?;
            Ok(ArchiveEntry {
                record_id: row.get(0)?,
                kind: row.get(1)?,
                month: row.get(2)?,
                title: row.get(3)?,
                participants: serde_json::from_str(&participants).unwrap_or_default(),
                closed_at: row.get(5)?,
                archived_at: row.get(6)?,
                bytes: row.get::<_, i64>(7)? as u64,
            })
        })?;
        let query = filter.query.as_deref().map(str::to_lowercase);
        let mut matched = Vec::new();
        for row in rows {
            let entry = row?;
            if filter.kind.as_deref().is_some_and(|kind| kind != entry.kind)
                || filter.month.as_deref().is_some_and(|month| month != entry.month)
                || filter.involving.as_deref().is_some_and(|who| !entry.participants.iter().any(|p| p == who))
                || query.as_deref().is_some_and(|query| {
                    !entry.title.to_lowercase().contains(query) && !entry.record_id.to_lowercase().contains(query)
                })
            {
                continue;
            }
            matched.push(entry);
        }
        let has_more = matched.len() > offset + limit;
        Ok((matched.into_iter().skip(offset).take(limit).collect(), has_more))
    }

    /// Delete months past retention. Returns (months, records) removed.
    pub fn prune(&self, retention_days: u32, now: DateTime<Utc>) -> anyhow::Result<(usize, usize)> {
        if retention_days == 0 {
            return Ok((0, 0));
        }
        let cutoff = crate::asks::now_rfc3339(now - chrono::Duration::days(i64::from(retention_days)));
        let connection = self.connection()?;
        let mut statement = connection.prepare("SELECT month, file, MAX(closed_at) FROM records GROUP BY month, file")?;
        let months: Vec<(String, String, String)> =
            statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?.collect::<Result<_, _>>()?;
        let (mut gone_months, mut gone_records) = (0, 0);
        for (month, file, newest) in months {
            if newest >= cutoff {
                continue;
            }
            gone_records += connection.execute("DELETE FROM records WHERE month = ?1", [&month])?;
            let _ = std::fs::remove_file(self.dir.join(file));
            gone_months += 1;
        }
        Ok((gone_months, gone_records))
    }

    /// `{bytes, months, records, oldest}`.
    pub fn usage(&self) -> anyhow::Result<Value> {
        let connection = self.connection()?;
        let (records, oldest): (i64, Option<String>) =
            connection.query_row("SELECT COUNT(*), MIN(closed_at) FROM records", [], |row| Ok((row.get(0)?, row.get(1)?)))?;
        let mut bytes = 0;
        let mut months = 0;
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with("records-") && name.ends_with(".jsonl.gz") {
                    months += 1;
                    bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
                }
            }
        }
        Ok(json!({"bytes": bytes, "months": months, "records": records, "oldest": oldest}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, at: DateTime<Utc>) -> NewArchiveRecord {
        NewArchiveRecord {
            record_id: id.into(),
            kind: "ask".into(),
            title: format!("title {id}"),
            participants: vec!["cutex.a".into()],
            closed_at: at,
            record: json!({"askId": id}),
            documents: vec![json!({"documentId": "doc-1", "text": "body"})],
        }
    }

    #[test]
    fn append_read_list_and_prune_by_month() {
        let dir = std::env::temp_dir().join(format!("cutex-archive-{}", uuid::Uuid::new_v4().simple()));
        let archive = RecordArchive::at(&dir);
        let now = Utc::now();
        let old = now - chrono::Duration::days(400);
        archive.append(record("ask-old", old), now).unwrap();
        archive.append(record("ask-1", now), now).unwrap();
        archive.append(record("ask-2", now), now).unwrap();
        archive.append(record("ask-2", now), now).unwrap(); // idempotent
        let got = archive.get("ask-2").unwrap().unwrap();
        assert_eq!(got["record"]["askId"], "ask-2");
        assert_eq!(got["documents"][0]["text"], "body");
        assert_eq!(archive.get("ask-1").unwrap().unwrap()["record"]["askId"], "ask-1");
        let (all, more) = archive.list(&ArchiveFilter::default(), 10, 0).unwrap();
        assert_eq!((all.len(), more), (3, false));
        let (page, more) = archive.list(&ArchiveFilter::default(), 1, 0).unwrap();
        assert_eq!((page.len(), more), (1, true));
        let filter = ArchiveFilter { query: Some("TITLE ask-1".into()), ..Default::default() };
        assert_eq!(archive.list(&filter, 10, 0).unwrap().0.len(), 1);
        assert_eq!(archive.prune(365, now).unwrap(), (1, 1));
        assert!(archive.get("ask-old").unwrap().is_none());
        assert!(archive.get("ask-1").unwrap().is_some());
        assert_eq!(archive.prune(0, now + chrono::Duration::days(4000)).unwrap(), (0, 0), "0 keeps");
        assert_eq!(archive.usage().unwrap()["records"], 2);
        let _ = std::fs::remove_dir_all(dir);
    }
}
