//! Input overflow → documents (cutex-desktop
//! `docs/research/input-overflow-documents.md` §B).
//!
//! When a message for an agent is larger than its receiver accepts, the
//! large part moves into a document and the message becomes a structured
//! overlength notice (MCP tool `read_overlength_content`, id `doc-…`, §0.4).
//! Delegation parts point at the records the asks store already has
//! (answers, briefs); this store holds everything else (agent messages,
//! alarms, task notices, management starts).
//!
//! - `<cutex home>/runtime/documents/blobs/<sha[0..2]>/<sha256>`: immutable,
//!   content-addressed, owner-private.
//! - `documents-v1.sqlite3`: one record per document (`doc-<uuid>`): bytes,
//!   media type, source `{kind, id, producer}`, readers.
//! - Agents read only documents they are readers of; the owner reads all.
//! - One document holds at most [`MAX_DOCUMENT_BYTES`]; the store at most
//!   `documents.quotaBytes` ([`DocumentError::QuotaFull`]).
//! - Lifetime: a document lives as long as its source record (a document of
//!   a delegation or agent message is archived with it, see [`sweep`]);
//!   documents of a source that no longer exists go after the upload grace.
//!
//! r211 ignores these directories; nothing here is needed to read the asks
//! store or the Agent Bus state, so a rollback stays safe.

pub mod archive;
pub mod sweep;

use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Largest single document (the delegation brief cap).
pub const MAX_DOCUMENT_BYTES: usize = 256 * 1024;
/// Page size of `read_overlength_content` (same as delegation answers).
pub const PAGE_BYTES: usize = crate::asks::INLINE_ANSWER_MAX_BYTES;
/// Owner capability names (`GET /v2/capabilities`).
pub const CAPABILITY: &str = "input-overflow-documents";
pub const ARCHIVE_CAPABILITY: &str = "record-archive";

/// Where a document came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentSource {
    /// `message` (Agent Bus message), `ask`, `upload`.
    pub kind: String,
    pub id: String,
    /// What spilled it: `bus.message`, `bus.alarm`, `bus.task`,
    /// `bus.managementStart`, `asks.brief`, ….
    pub producer: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentRecord {
    pub document_id: String,
    pub sha256: String,
    pub bytes: usize,
    pub media_type: String,
    pub created_at: String,
    pub source: DocumentSource,
    /// Sessions that may read it (agents); the owner reads every document.
    pub readers: Vec<String>,
    /// Short human title ("Message from Planner", "Alarm …").
    pub title: String,
}

/// What [`DocumentStore::put`] stores.
#[derive(Clone, Debug)]
pub struct NewDocument {
    pub bytes: Vec<u8>,
    pub media_type: String,
    pub source: DocumentSource,
    pub readers: Vec<String>,
    pub title: String,
}

#[derive(Debug)]
pub enum DocumentError {
    TooLarge { bytes: usize },
    QuotaFull { used: u64, quota: u64 },
    Store(anyhow::Error),
}

impl std::fmt::Display for DocumentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { bytes } => write!(f, "document_too_large: {bytes} bytes (max {MAX_DOCUMENT_BYTES})"),
            Self::QuotaFull { used, quota } => write!(f, "documents_quota_full: {used} of {quota} bytes used"),
            Self::Store(error) => write!(f, "documents_unavailable: {error:#}"),
        }
    }
}

impl std::error::Error for DocumentError {}

impl From<anyhow::Error> for DocumentError {
    fn from(error: anyhow::Error) -> Self {
        Self::Store(error)
    }
}

impl From<rusqlite::Error> for DocumentError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error.into())
    }
}

/// Space used by the live store.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub documents: u64,
    /// Distinct blob bytes.
    pub used_bytes: u64,
}

pub struct DocumentStore {
    dir: PathBuf,
}

/// Whether `id` names a document of this store.
pub fn is_document_id(id: &str) -> bool {
    id.strip_prefix("doc-").is_some_and(|rest| rest.len() == 32 && rest.bytes().all(|b| b.is_ascii_hexdigit()))
}

impl DocumentStore {
    pub fn open_default() -> anyhow::Result<Self> {
        Ok(Self::at(crate::config::paths::runtime_dir()?.join("documents")))
    }

    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn connection(&self) -> anyhow::Result<Connection> {
        crate::management::client_tokens::ensure_private_dir(&self.dir)?;
        let path = self.dir.join("documents-v1.sqlite3");
        let connection = Connection::open(&path).with_context(|| format!("open {}", path.display()))?;
        connection.busy_timeout(std::time::Duration::from_secs(10))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS documents (
                document_id TEXT PRIMARY KEY,
                sha256 TEXT NOT NULL,
                bytes INTEGER NOT NULL,
                media_type TEXT NOT NULL,
                created_at TEXT NOT NULL,
                source_kind TEXT NOT NULL,
                source_id TEXT NOT NULL,
                producer TEXT NOT NULL,
                readers TEXT NOT NULL,
                title TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS documents_source ON documents (source_kind, source_id);
            CREATE INDEX IF NOT EXISTS documents_sha ON documents (sha256);",
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

    fn blob_path(&self, sha256: &str) -> PathBuf {
        self.dir.join("blobs").join(&sha256[..2]).join(sha256)
    }

    /// Store a document. Same bytes share one blob; every call is its own
    /// record (readers and lifetime follow the source).
    pub fn put(&self, new: NewDocument, quota_bytes: u64, now: DateTime<Utc>) -> Result<DocumentRecord, DocumentError> {
        if new.bytes.len() > MAX_DOCUMENT_BYTES {
            return Err(DocumentError::TooLarge { bytes: new.bytes.len() });
        }
        let sha256 = crate::asks::sha256_hex(&new.bytes);
        let connection = self.connection()?;
        let blob_exists: bool =
            connection.query_row("SELECT EXISTS(SELECT 1 FROM documents WHERE sha256 = ?1)", [&sha256], |row| row.get(0))?;
        if !blob_exists {
            let used = usage_of(&connection)?.used_bytes;
            if used + new.bytes.len() as u64 > quota_bytes {
                return Err(DocumentError::QuotaFull { used, quota: quota_bytes });
            }
        }
        let path = self.blob_path(&sha256);
        if !path.exists() {
            crate::management::client_tokens::ensure_private_dir(path.parent().context("blob dir")?)?;
            crate::config::atomic::write_private_bytes_atomic(&path, &new.bytes)?;
        }
        let record = DocumentRecord {
            document_id: format!("doc-{}", uuid::Uuid::new_v4().simple()),
            sha256,
            bytes: new.bytes.len(),
            media_type: new.media_type,
            created_at: crate::asks::now_rfc3339(now),
            source: new.source,
            readers: new.readers,
            title: crate::asks::truncate_chars(&new.title, 200),
        };
        connection.execute(
            "INSERT INTO documents (document_id, sha256, bytes, media_type, created_at, source_kind, source_id, producer, readers, title)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                record.document_id,
                record.sha256,
                record.bytes as i64,
                record.media_type,
                record.created_at,
                record.source.kind,
                record.source.id,
                record.source.producer,
                serde_json::to_string(&record.readers).map_err(anyhow::Error::from)?,
                record.title,
            ],
        )?;
        Ok(record)
    }

    pub fn get(&self, document_id: &str) -> anyhow::Result<Option<DocumentRecord>> {
        if !is_document_id(document_id) {
            return Ok(None);
        }
        let connection = self.connection()?;
        Ok(connection
            .query_row(&format!("{SELECT} WHERE document_id = ?1"), [document_id], record_of)
            .optional()?)
    }

    /// Authorized ID lookup. Prefixes use SQLite key ranges, never LIKE;
    /// suffixes use exact trailing hex. Blob bytes are not loaded.
    pub fn id_candidates(&self, input: &str, reader: Option<&str>) -> anyhow::Result<Vec<String>> {
        let query = crate::short_ids::query(input);
        if !query.valid_hex || query.kind.is_some_and(|kind| kind != crate::short_ids::Kind::Document) { return Ok(Vec::new()); }
        let connection = self.connection()?;
        let low = format!("doc-{}", query.hex);
        let high = format!("{low}~");
        let sql = if query.suffix_only {
            format!("{SELECT} WHERE substr(document_id, -length(?3)) = ?3 AND ?1 <= ?2")
        } else {
            format!("{SELECT} WHERE (document_id >= ?1 AND document_id < ?2) OR substr(document_id, -length(?3)) = ?3")
        };
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map(params![low,high,query.hex], record_of)?;
        let mut result = Vec::new();
        for row in rows { let row = row?; if reader.is_none_or(|reader| row.readers.iter().any(|allowed| allowed == reader)) { result.push(row.document_id); } }
        Ok(result)
    }

    pub fn all_ids(&self) -> anyhow::Result<Vec<String>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare("SELECT document_id FROM documents ORDER BY document_id")?;
        let rows = statement.query_map([], |row| row.get(0))?;
        Ok(rows.collect::<Result<Vec<_>,_>>()?)
    }

    pub fn short_id(&self, id: &str, minimum: usize) -> anyhow::Result<String> {
        let index = crate::short_ids::Index::new(self.all_ids()?.into_iter().map(crate::short_ids::Entry::id));
        Ok(index.short(id,minimum))
    }

    /// The record and bytes for `reader` (`None` = the owner). Unknown and
    /// unreadable documents look the same.
    pub fn read(&self, document_id: &str, reader: Option<&str>) -> anyhow::Result<Option<(DocumentRecord, Vec<u8>)>> {
        let Some(record) = self.get(document_id)? else {
            return Ok(None);
        };
        if reader.is_some_and(|reader| !record.readers.iter().any(|allowed| allowed == reader)) {
            return Ok(None);
        }
        match std::fs::read(self.blob_path(&record.sha256)) {
            Ok(bytes) => Ok(Some((record, bytes))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Documents of one source.
    pub fn of_source(&self, kind: &str, id: &str) -> anyhow::Result<Vec<DocumentRecord>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(&format!("{SELECT} WHERE source_kind = ?1 AND source_id = ?2 ORDER BY created_at"))?;
        let rows = statement.query_map([kind, id], record_of)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn all(&self) -> anyhow::Result<Vec<DocumentRecord>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(&format!("{SELECT} ORDER BY created_at"))?;
        let rows = statement.query_map([], record_of)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn usage(&self) -> anyhow::Result<Usage> {
        usage_of(&self.connection()?)
    }

    /// Delete records (and blobs no other record uses). Returns freed bytes.
    pub fn delete(&self, document_ids: &[String]) -> anyhow::Result<u64> {
        if document_ids.is_empty() {
            return Ok(0);
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let mut shas = Vec::new();
        for id in document_ids {
            if let Some(sha) = transaction
                .query_row("SELECT sha256 FROM documents WHERE document_id = ?1", [id], |row| row.get::<_, String>(0))
                .optional()?
            {
                transaction.execute("DELETE FROM documents WHERE document_id = ?1", [id])?;
                shas.push(sha);
            }
        }
        let mut orphaned = Vec::new();
        for sha in shas {
            let used: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM documents WHERE sha256 = ?1)", [&sha], |row| row.get(0))?;
            if !used && !orphaned.contains(&sha) {
                orphaned.push(sha);
            }
        }
        transaction.commit()?;
        let mut freed = 0;
        for sha in orphaned {
            let path = self.blob_path(&sha);
            if let Ok(metadata) = std::fs::metadata(&path) {
                freed += metadata.len();
            }
            let _ = std::fs::remove_file(path);
        }
        Ok(freed)
    }
}

const SELECT: &str =
    "SELECT document_id, sha256, bytes, media_type, created_at, source_kind, source_id, producer, readers, title FROM documents";

fn record_of(row: &rusqlite::Row<'_>) -> rusqlite::Result<DocumentRecord> {
    let readers: String = row.get(8)?;
    Ok(DocumentRecord {
        document_id: row.get(0)?,
        sha256: row.get(1)?,
        bytes: row.get::<_, i64>(2)? as usize,
        media_type: row.get(3)?,
        created_at: row.get(4)?,
        source: DocumentSource { kind: row.get(5)?, id: row.get(6)?, producer: row.get(7)? },
        readers: serde_json::from_str(&readers).unwrap_or_default(),
        title: row.get(9)?,
    })
}

fn usage_of(connection: &Connection) -> anyhow::Result<Usage> {
    let documents: i64 = connection.query_row("SELECT COUNT(*) FROM documents", [], |row| row.get(0))?;
    let used: i64 = connection.query_row(
        "SELECT COALESCE(SUM(bytes), 0) FROM (SELECT sha256, MAX(bytes) AS bytes FROM documents GROUP BY sha256)",
        [],
        |row| row.get(0),
    )?;
    Ok(Usage { documents: documents as u64, used_bytes: used as u64 })
}

/// One page of a text document for an agent (`read_overlength_content`,
/// `delegation_read(delegation="doc-…")`).
pub fn read_page(store: &DocumentStore, document_id: &str, reader: &str, cursor: usize) -> Result<Value, (&'static str, String)> {
    let found = store
        .read(document_id, Some(reader))
        .map_err(|error| ("documents_unavailable", format!("{error:#}")))?;
    let (record, bytes) =
        found.ok_or(("document_not_found", "no such document you may read".to_string()))?;
    let text = String::from_utf8(bytes).map_err(|_| ("document_not_text", "the document is not text".to_string()))?;
    let page = crate::asks::answer::page(&text, cursor, PAGE_BYTES);
    Ok(json!({
        "ok": true,
        "document_id": record.document_id,
        "title": record.title,
        "bytes": text.len(),
        "media_type": record.media_type,
        "cursor": page.cursor,
        "next_cursor": page.next_cursor,
        "text": page.text,
    }))
}

/// Record input that could not be delivered as sent (origin `system`,
/// `agentId` = the agent it is about): a record only, shown in the agent's
/// timeline and the owner-notify list, never a need, toast or phone push
/// (the owner cannot act on it) — unless the owner raised the notice's
/// event `system.<kind>` in the notification level tables. Tests never
/// reach the real store: callers run in the management process only.
pub fn notify_owner(kind: &str, agent_id: &str, agent_name: Option<String>, text: String, delegation: Option<String>) {
    use crate::owner_notify::store::{Level, NewNotification, OwnerNotifyStore};
    if cfg!(test) {
        return;
    }
    let (settings, evaluator) = crate::notification_levels::Evaluator::current_with_owner();
    let Ok(store) = OwnerNotifyStore::open_default() else {
        return;
    };
    let new = NewNotification {
        agent_id: agent_id.to_string(),
        agent_name,
        text: crate::asks::truncate_chars(&text, crate::owner_notify::store::MAX_TEXT_CHARS),
        level: Level::Info,
        needs_reply: false,
        delegation,
        job: None,
        turn_id: None,
    };
    // Written by cutex, not by the agent: no reply, labelled as a cutex notice.
    let origin = crate::owner_notify::store::Origin::System;
    let created =
        store.with_state(|state| state.create_decided(new, origin, Some(kind), &settings, &evaluator, None, Utc::now()));
    if let Err(error) = created {
        eprintln!("documents: could not notify the owner: {error:#}");
        return;
    }
    crate::management::v2::owner_notify::publish_pending_notices(&store);
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn temp_store(label: &str) -> (DocumentStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!("cutex-documents-{label}-{}", uuid::Uuid::new_v4().simple()));
        (DocumentStore::at(&dir), dir)
    }

    pub fn new_doc(text: &str, readers: &[&str]) -> NewDocument {
        NewDocument {
            bytes: text.as_bytes().to_vec(),
            media_type: "text/plain; charset=utf-8".into(),
            source: DocumentSource { kind: "message".into(), id: "msg-1".into(), producer: "bus.message".into() },
            readers: readers.iter().map(|reader| reader.to_string()).collect(),
            title: "Message".into(),
        }
    }

    #[test]
    fn put_read_access_quota_and_delete() {
        let (store, dir) = temp_store("basic");
        let now = Utc::now();
        let first = store.put(new_doc(&"x".repeat(1000), &["cutex.a"]), 10_000, now).unwrap();
        assert!(is_document_id(&first.document_id));
        let (_, bytes) = store.read(&first.document_id, Some("cutex.a")).unwrap().unwrap();
        assert_eq!(bytes.len(), 1000);
        assert!(store.read(&first.document_id, Some("cutex.b")).unwrap().is_none(), "not a reader");
        assert!(store.read(&first.document_id, None).unwrap().is_some(), "the owner reads all");
        // Same bytes share the blob; a new record per source.
        let second = store.put(new_doc(&"x".repeat(1000), &["cutex.b"]), 10_000, now).unwrap();
        assert_ne!(first.document_id, second.document_id);
        assert_eq!(store.usage().unwrap(), Usage { documents: 2, used_bytes: 1000 });
        // Quota counts distinct blobs.
        match store.put(new_doc(&"y".repeat(9_500), &[]), 10_000, now) {
            Err(DocumentError::QuotaFull { used: 1000, quota: 10_000 }) => {}
            other => panic!("{other:?}"),
        }
        assert!(matches!(store.put(new_doc(&"z".repeat(MAX_DOCUMENT_BYTES + 1), &[]), u64::MAX, now), Err(DocumentError::TooLarge { .. })));
        // Paging for a reader.
        let page = read_page(&store, &first.document_id, "cutex.a", 0).unwrap();
        assert_eq!(page["bytes"], 1000);
        assert_eq!(read_page(&store, &first.document_id, "cutex.c", 0).unwrap_err().0, "document_not_found");
        // The blob goes with its last record.
        assert_eq!(store.delete(&[first.document_id.clone()]).unwrap(), 0);
        assert_eq!(store.delete(&[second.document_id.clone()]).unwrap(), 1000);
        assert_eq!(store.usage().unwrap().documents, 0);
        let _ = std::fs::remove_dir_all(dir);
    }
}
