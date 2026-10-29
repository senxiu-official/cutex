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

use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
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
    asks: Option<crate::asks::store::AskStore>,
    documents: Option<super::DocumentStore>,
}

/// Facts stored in every complete gzip member, independently of the disposable index.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArchivedFacts {
    record_id: String,
    kind: String,
    title: String,
    participants: Vec<String>,
    closed_at: String,
    archived_at: String,
}

impl RecordArchive {
    pub fn open_default() -> anyhow::Result<Self> {
        Ok(Self::at(
            crate::config::paths::config_dir()?.join("archive"),
        ).with_asks(crate::asks::store::AskStore::open_default()?).with_documents(super::DocumentStore::open_default()?))
    }

    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into(), asks:None,documents:None }
    }

    pub fn with_asks(mut self,asks:crate::asks::store::AskStore)->Self {self.asks=Some(asks);self}
    pub fn with_documents(mut self,documents:super::DocumentStore)->Self {self.documents=Some(documents);self}

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn connection(&self) -> anyhow::Result<Connection> {
        crate::management::client_tokens::ensure_private_dir(&self.dir)?;
        let path = self.dir.join("index-v1.sqlite3");
        let connection =
            Connection::open(&path).with_context(|| format!("open {}", path.display()))?;
        connection.busy_timeout(std::time::Duration::from_secs(10))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
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
        let mut connection = self.connection()?;
        // The SQLite writer lock also owns the gzip tail. No process can append, dedupe or prune
        // through this writer between the index read and commit. Older writers may have left
        // complete members without index rows: the gzip records, not MAX(index), own those bytes.
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
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
        let indexed_end: i64 = transaction.query_row(
            "SELECT COALESCE(MAX(offset + length), 0) FROM records WHERE file = ?1",
            [&file],
            |row| row.get(0),
        )?;
        anyhow::ensure!(indexed_end >= 0, "archive index has a negative end offset");
        let path = self.dir.join(&file);
        let mut options = std::fs::OpenOptions::new();
        options.create(true).read(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut handle = options
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        let length = handle.metadata()?.len();
        anyhow::ensure!(
            length >= indexed_end as u64,
            "archive file is shorter than its committed index: {}",
            path.display()
        );
        let offset = self.recover_members(&transaction, &file, &month, &mut handle, length)?;
        // Recovery may have found this very record after a failed index insert. Commit the repaired
        // index, but never append a second copy of the complete member.
        let existing: Option<(String, i64, i64)> = transaction.query_row(
            "SELECT file, offset, length FROM records WHERE record_id = ?1",
            [&record.record_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional()?;
        if let Some((file, offset, length)) = existing {
            let previous = read_member(&self.dir.join(file), offset as u64, length as u64)?;
            let candidate: Value = serde_json::from_slice(&line)?;
            anyhow::ensure!(same_record(&previous, &candidate), "archive id already contains a different record; live source retained");
            transaction.commit()?;
            return Ok(());
        }
        // O_APPEND also prevents an old, non-cooperating writer from being overwritten if it
        // appends after recovery. Refuse changed lengths when observed, then index our actual end.
        anyhow::ensure!(
            handle.metadata()?.len() == offset,
            "archive changed before append; retry after other writers stop"
        );
        handle.write_all(&member)?;
        let offset = handle
            .stream_position()?
            .checked_sub(member.len() as u64)
            .context("invalid archive append offset")?;
        handle.sync_all()?;
        #[cfg(unix)]
        std::fs::File::open(&self.dir)?.sync_all()?;
        transaction.execute(
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
        transaction.commit()?;
        Ok(())
    }

    /// Rebuild a missing/incomplete/overlapping index from complete members of this month. The
    /// ordinary contiguous-index case does no decompression. Undecodable unindexed tails are
    /// synced to an orphan file before repairing the month. Committed ranges and invalid
    /// complete records remain untouched for explicit review.
    fn recover_members(
        &self,
        transaction: &rusqlite::Transaction<'_>,
        file: &str,
        month: &str,
        handle: &mut std::fs::File,
        length: u64,
    ) -> anyhow::Result<u64> {
        let mut statement = transaction
            .prepare("SELECT offset, length FROM records WHERE file = ?1 ORDER BY offset")?;
        let ranges = statement.query_map([file], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut covered = 0_u64;
        let mut contiguous = true;
        for range in ranges {
            let (offset, bytes) = range?;
            if offset < 0 || bytes <= 0 || offset as u64 != covered {
                contiguous = false;
                break;
            }
            covered = covered
                .checked_add(bytes as u64)
                .context("archive index range overflow")?;
        }
        drop(statement);
        if contiguous && covered == length {
            return Ok(length);
        }

        handle.seek(SeekFrom::Start(0))?;
        let mut reader = BufReader::new(handle.try_clone()?);
        let mut recovered = Vec::new();
        let mut ids = std::collections::BTreeMap::<String, Value>::new();
        let indexed_end: i64 = transaction.query_row("SELECT COALESCE(MAX(offset + length), 0) FROM records WHERE file = ?1", [file], |row| row.get(0))?;
        let mut end = 0_u64;
        while end < length {
            let mut text = Vec::new();
            // bufread::GzDecoder stops exactly at one member, unlike the read decoder's
            // permitted read-ahead; stream_position subtracts BufReader's unconsumed bytes.
            let result = flate2::bufread::GzDecoder::new(&mut reader).read_to_end(&mut text);
            let next = reader.stream_position()?;
            match result {
                Ok(_) => {
                    anyhow::ensure!(
                        next > end && next <= length,
                        "invalid gzip member boundary in {file}"
                    );
                    let facts: ArchivedFacts =
                        serde_json::from_slice(&text).with_context(|| {
                            format!(
                                "invalid complete archive record at {file}:{end}; archive retained"
                            )
                        })?;
                    anyhow::ensure!(
                        !facts.record_id.is_empty(),
                        "empty archive record id at {file}:{end}; archive retained"
                    );
                    let value: Value = serde_json::from_slice(&text)?;
                    if let Some(previous) = ids.insert(facts.record_id.clone(), value.clone()) {
                        anyhow::ensure!(same_record(&previous, &value), "conflicting duplicate archive record at {file}:{end}; archive retained");
                        end = next;
                        continue;
                    }
                    let existing: Option<(String, i64, i64)> = transaction.query_row(
                        "SELECT file, offset, length FROM records WHERE record_id = ?1 AND file != ?2",
                        params![facts.record_id, file], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    ).optional()?;
                    if let Some((other, offset, bytes)) = existing {
                        let previous = read_member(&self.dir.join(other), offset as u64, bytes as u64)?;
                        anyhow::ensure!(same_record(&previous, &value), "conflicting duplicate archive record at {file}:{end}; archive retained");
                        end = next;
                        continue;
                    }
                    let closed = DateTime::parse_from_rfc3339(&facts.closed_at)
                        .context("invalid archived close time; archive retained")?;
                    anyhow::ensure!(
                        closed.format("%Y-%m").to_string() == month,
                        "archive record belongs to another month; archive retained"
                    );
                    recovered.push((end, next - end, facts));
                    end = next;
                }
                Err(error) => {
                    // Committed bytes are never automatically truncated. An unindexed tail
                    // is saved and synced byte-for-byte while we hold the writer lock.
                    anyhow::ensure!(end >= indexed_end as u64, "cannot decode committed archive member at {file}:{end}: {error}; archive retained");
                    drop(reader);
                    self.quarantine_tail(handle, file, end, length)?;
                    return self.recover_members(transaction, file, month, handle, end);
                }
            }
        }
        drop(reader);
        // An old writer does not participate in this SQLite fence. Do not index a file that grew
        // during recovery, or commit offsets based on its former size.
        anyhow::ensure!(
            handle.metadata()?.len() == length,
            "archive changed during recovery; retry after other writers stop"
        );
        transaction.execute("DELETE FROM records WHERE file = ?1", [file])?;
        for (offset, bytes, facts) in recovered {
            transaction.execute(
                "INSERT INTO records (record_id, kind, month, file, offset, length, title, participants, closed_at, archived_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![facts.record_id, facts.kind, month, file, offset as i64, bytes as i64,
                    crate::asks::truncate_chars(&facts.title, 200), serde_json::to_string(&facts.participants)?, facts.closed_at, facts.archived_at],
            )?;
        }
        Ok(end)
    }

    fn quarantine_tail(&self, handle: &mut std::fs::File, file: &str, end: u64, length: u64) -> anyhow::Result<()> {
        anyhow::ensure!(handle.metadata()?.len() == length, "archive changed during recovery");
        handle.seek(SeekFrom::Start(end))?;
        let mut tail = Vec::new();
        handle.take(length - end).read_to_end(&mut tail)?;
        anyhow::ensure!(tail.len() as u64 == length - end, "archive changed during recovery");
        let orphan = self.dir.join(format!("{}.orphan-{}-{}", file.trim_end_matches(".jsonl.gz"), Utc::now().timestamp_millis(), uuid::Uuid::new_v4()));
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut saved = options.open(&orphan)?;
        saved.write_all(&tail)?;
        saved.sync_all()?;
        #[cfg(unix)]
        std::fs::File::open(&self.dir)?.sync_all()?;
        handle.seek(SeekFrom::Start(end))?;
        let mut checked = Vec::new();
        handle.take(length - end).read_to_end(&mut checked)?;
        anyhow::ensure!(handle.metadata()?.len() == length && checked == tail, "archive changed while saving orphan; original retained");
        // Windows append handles carry FILE_APPEND_DATA without FILE_WRITE_DATA.
        // Open the same locked file for repair only after the orphan is durable.
        let repair = std::fs::OpenOptions::new().read(true).write(true).open(self.dir.join(file))?;
        anyhow::ensure!(repair.metadata()?.len() == length, "archive changed before tail repair");
        repair.set_len(end)?;
        repair.sync_all()?;
        Ok(())
    }

    /// One archived record with its documents' text.
    pub fn get(&self, record_id: &str) -> anyhow::Result<Option<Value>> {
        if let Some(asks)=&self.asks {
            if asks.archived_identities()?.iter().any(|(kind,id)|kind=="asks"&&id==record_id) {
                let state=asks.lookup_snapshot(&[record_id])?;
                let ask=state.asks.get(record_id).context("archived ask disappeared; retry history read")?;
                let answers:Vec<_>=state.answers.values().filter(|a|a.ask_id==record_id).collect();
                let questions:Vec<_>=state.questions.values().filter(|q|q.ask_id==record_id).collect();
                let deliveries:Vec<_>=state.deliveries.values().filter(|d|d.ask_id==record_id).collect();
                let artifact_ids:std::collections::BTreeSet<_>=ask.body.attachments.iter().chain(ask.body.text_artifact.iter())
                    .chain(answers.iter().filter_map(|answer|answer.artifact_id.as_ref())).collect();
                let mut texts=Vec::new();
                for id in artifact_ids {
                    if let Some((_,bytes))=asks.read_artifact(id,None)? {
                        let text=std::str::from_utf8(&bytes).ok();
                        use base64::Engine as _;
                        texts.push(json!({"artifactId":id,"bytes":bytes.len(),"text":text,
                            "encoding":if text.is_some(){"utf8"}else{"base64"},
                            "content":if text.is_none(){Some(base64::engine::general_purpose::STANDARD.encode(&bytes))}else{None}}));
                    }
                }
                let mut documents=Vec::new();
                if let Some(store)=&self.documents {
                    for doc in store.of_source("ask",record_id)? {
                        if let Some((_,bytes))=store.read(&doc.document_id,None)? {
                            documents.push(json!({"documentId":doc.document_id,"title":doc.title,"bytes":doc.bytes,"text":String::from_utf8_lossy(&bytes)}));
                        }
                    }
                }
                return Ok(Some(json!({"recordId":record_id,"kind":"ask","title":ask.title,
                    "archivedAt":asks.archived_at(record_id)?.context("archive membership changed; retry history read")?,
                    "participants":ask.requester.session().into_iter().chain([ask.target_cutex_session_id.as_str()]).collect::<Vec<_>>(),
                    "closedAt":ask.closed_at,"record":{"ask":ask,"answers":answers,"questions":questions,"deliveries":deliveries,"texts":texts},"documents":documents})));
            }
        }
        let connection = self.connection()?;
        let Some((file, offset, length)) = connection
            .query_row(
                "SELECT file, offset, length FROM records WHERE record_id = ?1",
                [record_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
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

    pub fn list(
        &self,
        filter: &ArchiveFilter,
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<(Vec<ArchiveEntry>, bool)> {
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
            if filter
                .kind
                .as_deref()
                .is_some_and(|kind| kind != entry.kind)
                || filter
                    .month
                    .as_deref()
                    .is_some_and(|month| month != entry.month)
                || filter
                    .involving
                    .as_deref()
                    .is_some_and(|who| !entry.participants.iter().any(|p| p == who))
                || query.as_deref().is_some_and(|query| {
                    !entry.title.to_lowercase().contains(query)
                        && !entry.record_id.to_lowercase().contains(query)
                })
            {
                continue;
            }
            matched.push(entry);
        }
        if let Some(asks)=&self.asks {
            for (ask,archived_at) in asks.archived_asks()? {
                // Explicit archive search only. Phase 2 will push these
                // summaries into indexed columns instead of hydrating asks.
                let id=ask.ask_id.clone();
                matched.retain(|old|old.record_id!=id);
                let closed=ask.closed_at.clone().context("archived ask has no close timestamp")?;
                let entry=ArchiveEntry {record_id:id,kind:"ask".into(),month:closed.get(..7).context("invalid archived close date")?.into(),
                    title:ask.title.clone(),participants:ask.requester.session().into_iter().map(str::to_string).chain([ask.target_cutex_session_id.clone()]).collect(),
                    closed_at:closed,archived_at,bytes:serde_json::to_vec(&ask)?.len() as u64};
                if filter.kind.as_ref().is_some_and(|kind|kind!=&entry.kind)
                    ||filter.month.as_ref().is_some_and(|month|month!=&entry.month)
                    ||filter.involving.as_ref().is_some_and(|who|!entry.participants.contains(who))
                    ||query.as_deref().is_some_and(|q|!entry.title.to_lowercase().contains(q)&&!entry.record_id.to_lowercase().contains(q)) {continue;}
                matched.push(entry);
            }
            matched.sort_by(|a,b|b.closed_at.cmp(&a.closed_at).then_with(||a.record_id.cmp(&b.record_id)));
        }
        let has_more = matched.len() > offset + limit;
        Ok((
            matched.into_iter().skip(offset).take(limit).collect(),
            has_more,
        ))
    }

    /// Delete months past retention. Returns (months, records) removed.
    pub fn prune(&self, retention_days: u32, now: DateTime<Utc>) -> anyhow::Result<(usize, usize)> {
        if retention_days == 0 {
            return Ok((0, 0));
        }
        let cutoff =
            crate::asks::now_rfc3339(now - chrono::Duration::days(i64::from(retention_days)));
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut statement = transaction
            .prepare("SELECT month, file, MAX(closed_at) FROM records GROUP BY month, file")?;
        let months: Vec<(String, String, String)> = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<Result<_, _>>()?;
        drop(statement);
        let (mut gone_months, mut gone_records) = (0, 0);
        let mut unindexed_files = Vec::new();
        for (month, file, newest) in months {
            if newest >= cutoff {
                continue;
            }
            gone_records +=
                transaction.execute("DELETE FROM records WHERE month = ?1", [&month])?;
            unindexed_files.push(file);
        }
        transaction.commit()?;
        // A crash now may leave an unindexed file, never an index pointing at a deleted file.
        // Recheck after reacquiring the writer lock: an append may have recreated the month since
        // the deletion committed, in which case its new bytes must not be removed.
        let cleanup = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for file in unindexed_files {
            let indexed: bool = cleanup.query_row(
                "SELECT EXISTS(SELECT 1 FROM records WHERE file = ?1)",
                [&file],
                |row| row.get(0),
            )?;
            if !indexed {
                let _ = std::fs::remove_file(self.dir.join(file));
                gone_months += 1;
            }
        }
        cleanup.commit()?;
        Ok((gone_months, gone_records))
    }

    /// `{bytes, months, records, oldest}`.
    pub fn usage(&self) -> anyhow::Result<Value> {
        let connection = self.connection()?;
        let (records, oldest): (i64, Option<String>) =
            connection.query_row("SELECT COUNT(*), MIN(closed_at) FROM records", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
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

fn read_member(path: &Path, offset: u64, length: u64) -> anyhow::Result<Value> {
    let mut handle = std::fs::File::open(path)?;
    handle.seek(SeekFrom::Start(offset))?;
    let mut bytes = vec![0; length as usize];
    handle.read_exact(&mut bytes)?;
    let mut text = Vec::new();
    flate2::read::GzDecoder::new(bytes.as_slice()).read_to_end(&mut text)?;
    Ok(serde_json::from_slice(&text)?)
}

// Historical sweep retries could choose another fallback close month. Payloads
// and documents must agree; only the two archive timestamps may differ.
fn same_record(left: &Value, right: &Value) -> bool {
    let stable = |value: &Value| {
        let mut value = value.clone();
        if let Some(object) = value.as_object_mut() {
            object.remove("archivedAt");
            object.remove("closedAt");
        }
        value
    };
    stable(left) == stable(right)
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

    fn month_path(archive: &RecordArchive, at: DateTime<Utc>) -> PathBuf {
        archive
            .dir
            .join(format!("records-{}.jsonl.gz", at.format("%Y-%m")))
    }

    fn all_member_ids(path: &Path) -> Vec<String> {
        let mut text = String::new();
        flate2::read::MultiGzDecoder::new(std::fs::File::open(path).unwrap())
            .read_to_string(&mut text)
            .unwrap();
        text.lines()
            .map(|line| {
                serde_json::from_str::<Value>(line).unwrap()["recordId"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    fn orphan_bytes(dir: &Path) -> Vec<Vec<u8>> {
        std::fs::read_dir(dir).unwrap().map(Result::unwrap)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".orphan-"))
            .map(|entry| std::fs::read(entry.path()).unwrap()).collect()
    }

    #[test]
    fn retry_with_changed_payload_refuses_to_discard_the_live_source() {
        let dir = std::env::temp_dir().join(format!("cutex-archive-conflict-{}", uuid::Uuid::new_v4()));
        let archive = RecordArchive::at(&dir);
        let now = Utc::now();
        archive.append(record("one", now), now).unwrap();
        let mut changed = record("one", now);
        changed.documents[0]["text"] = json!("new answer after earlier archive");
        assert!(archive.append(changed, now).unwrap_err().to_string().contains("live source retained"));
        assert_eq!(archive.get("one").unwrap().unwrap()["documents"][0]["text"], "body");
        assert_eq!(all_member_ids(&month_path(&archive, now)), ["one"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn zero_filled_tail_is_saved_byte_for_byte_before_resuming() {
        let dir = std::env::temp_dir().join(format!("cutex-archive-zero-{}", uuid::Uuid::new_v4()));
        let archive = RecordArchive::at(&dir);
        let now = Utc::now();
        archive.append(record("one", now), now).unwrap();
        let path = month_path(&archive, now);
        let original = std::fs::read(&path).unwrap();
        let zeros = vec![0; 4096];
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&zeros).unwrap(); file.sync_all().unwrap(); drop(file);
        archive.append(record("two", now), now).unwrap();
        assert_eq!(orphan_bytes(&dir), vec![zeros]);
        assert!(std::fs::read(&path).unwrap().starts_with(&original));
        assert_eq!(all_member_ids(&path), ["one", "two"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn identical_historical_cross_month_duplicate_does_not_block_rebuild() {
        let dir = std::env::temp_dir().join(format!("cutex-archive-duplicate-{}", uuid::Uuid::new_v4()));
        let archive = RecordArchive::at(&dir);
        let first = DateTime::parse_from_rfc3339("2026-08-31T00:00:00Z").unwrap().with_timezone(&Utc);
        let next = first + chrono::Duration::days(2);
        archive.append(record("one", first), first).unwrap();
        archive.connection().unwrap().execute("DELETE FROM records", []).unwrap();
        // Historical retry chose the next sweep's month after losing the first index row.
        archive.append(record("one", next), next).unwrap();
        archive.append(record("two", first), next).unwrap();
        assert_eq!(archive.get("one").unwrap().unwrap()["record"]["askId"], "one");
        assert_eq!(archive.get("two").unwrap().unwrap()["record"]["askId"], "two");
        assert_eq!(all_member_ids(&month_path(&archive, first)), ["one", "two"]);
        assert_eq!(all_member_ids(&month_path(&archive, next)), ["one"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn append_reindexes_complete_unindexed_members_without_truncating_them() {
        let dir = std::env::temp_dir().join(format!("cutex-archive-tail-{}", uuid::Uuid::new_v4()));
        let archive = RecordArchive::at(&dir);
        let now = Utc::now();
        archive.append(record("one", now), now).unwrap();
        let path = month_path(&archive, now);
        let committed_end = std::fs::metadata(&path).unwrap().len();
        let mut orphan = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        orphan.write_all(&serde_json::to_vec(&json!({
            "recordId": "uncommitted", "kind": "ask", "title": "recovered record", "participants": ["cutex.a"],
            "closedAt": crate::asks::now_rfc3339(now), "archivedAt": crate::asks::now_rfc3339(now),
            "record": {"askId": "uncommitted"}, "documents": [{"text": "saved document"}]
        })).unwrap()).unwrap();
        orphan.write_all(b"\n").unwrap();
        let orphan = orphan.finish().unwrap();
        let recovered_end = committed_end + orphan.len() as u64;
        let mut handle = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        handle.write_all(&orphan).unwrap();
        handle.sync_all().unwrap();
        drop(handle);
        archive.append(record("two", now), now).unwrap();
        assert_eq!(all_member_ids(&path), ["one", "uncommitted", "two"]);
        let connection = archive.connection().unwrap();
        let offset: i64 = connection
            .query_row(
                "SELECT offset FROM records WHERE record_id = 'two'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(offset as u64, recovered_end);
        assert_eq!(
            archive.get("uncommitted").unwrap().unwrap()["documents"][0]["text"],
            "saved document"
        );
        assert_eq!(
            archive.get("one").unwrap().unwrap()["record"]["askId"],
            "one"
        );
        assert_eq!(
            archive.get("two").unwrap().unwrap()["record"]["askId"],
            "two"
        );
        drop(connection);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn append_quarantines_a_torn_first_member_and_continues() {
        let dir =
            std::env::temp_dir().join(format!("cutex-archive-first-tail-{}", uuid::Uuid::new_v4()));
        let archive = RecordArchive::at(&dir);
        drop(archive.connection().unwrap());
        let now = Utc::now();
        let path = month_path(&archive, now);
        std::fs::write(&path, b"\x1f\x8b\x08\x00torn").unwrap();
        let before=std::fs::read(&path).unwrap();
        archive.append(record("one", now), now).unwrap();
        assert_eq!(orphan_bytes(&dir), vec![before]);
        assert_eq!(all_member_ids(&path), ["one"]);
        assert!(archive.get("one").unwrap().is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn partial_tail_after_complete_members_is_retained_and_does_not_hide_indexed_records() {
        let dir=std::env::temp_dir().join(format!("cutex-archive-partial-safe-{}",uuid::Uuid::new_v4()));
        let archive=RecordArchive::at(&dir);let now=Utc::now();
        archive.append(record("one",now),now).unwrap();
        let path=month_path(&archive,now);
        let mut handle=std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        handle.write_all(b"\x1f\x8b\x08\x00unfinished").unwrap();handle.sync_all().unwrap();drop(handle);
        let before=std::fs::read(&path).unwrap();
        archive.append(record("two",now),now).unwrap();
        assert_eq!(orphan_bytes(&dir), vec![b"\x1f\x8b\x08\x00unfinished".to_vec()]);
        assert!(std::fs::read(&path).unwrap().starts_with(&before[..before.len()-14]));
        assert_eq!(all_member_ids(&path), ["one", "two"]);
        assert!(archive.get("two").unwrap().is_some());
        assert_eq!(archive.get("one").unwrap().unwrap()["record"]["askId"],"one");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn append_retry_reindexes_bytes_from_a_failed_index_insert_without_duplicate_members() {
        let dir = std::env::temp_dir().join(format!(
            "cutex-archive-insert-failure-{}",
            uuid::Uuid::new_v4()
        ));
        let archive = RecordArchive::at(&dir);
        let now = Utc::now();
        archive.append(record("one", now), now).unwrap();
        let connection = archive.connection().unwrap();
        connection.execute_batch("CREATE TRIGGER fail_append BEFORE INSERT ON records WHEN NEW.record_id = 'two' BEGIN SELECT RAISE(ABORT, 'injected insert failure'); END;").unwrap();
        assert!(archive.append(record("two", now), now).is_err());
        assert!(archive.get("two").unwrap().is_none());
        assert_eq!(
            archive.get("one").unwrap().unwrap()["record"]["askId"],
            "one"
        );
        connection
            .execute_batch("DROP TRIGGER fail_append;")
            .unwrap();
        drop(connection);
        archive.append(record("two", now), now).unwrap();
        assert_eq!(all_member_ids(&month_path(&archive, now)), ["one", "two"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn append_recovers_missing_empty_behind_gapped_and_overlapping_indexes_without_losing_records()
    {
        for damage in ["missing", "empty", "behind", "gap", "overlap"] {
            let dir = std::env::temp_dir().join(format!(
                "cutex-archive-index-{damage}-{}",
                uuid::Uuid::new_v4()
            ));
            let archive = RecordArchive::at(&dir);
            let now = Utc::now();
            archive.append(record("one", now), now).unwrap();
            archive.append(record("two", now), now).unwrap();
            let path = month_path(&archive, now);
            let original = std::fs::read(&path).unwrap();
            if damage == "missing" {
                // Every connection is closed: this models restoring only the .gz or losing the
                // disposable index, with the source records already swept from their live store.
                for suffix in ["", "-wal", "-shm"] {
                    let index = dir.join(format!("index-v1.sqlite3{suffix}"));
                    if index.exists() {
                        std::fs::remove_file(index).unwrap();
                    }
                }
            } else {
                let connection = archive.connection().unwrap();
                connection
                    .execute_batch(match damage {
                        "empty" => "DELETE FROM records;",
                        "behind" => "DELETE FROM records WHERE record_id = 'two';",
                        "gap" => "DELETE FROM records WHERE record_id = 'one';",
                        "overlap" => "UPDATE records SET offset = 0 WHERE record_id = 'two';",
                        _ => unreachable!(),
                    })
                    .unwrap();
            }
            // Retrying an already archived record must commit recovery, not return early with
            // a still-missing index or manufacture a second member for that record.
            archive.append(record("two", now), now).unwrap();
            assert_eq!(
                std::fs::read(&path).unwrap(),
                original,
                "{damage}: recovered records were rewritten"
            );
            for id in ["one", "two"] {
                let value = archive.get(id).unwrap().unwrap();
                assert_eq!(value["record"]["askId"], id, "{damage}");
                assert_eq!(value["documents"][0]["text"], "body", "{damage}");
            }
            archive.append(record("three", now), now).unwrap();
            assert_eq!(
                &std::fs::read(&path).unwrap()[..original.len()],
                original.as_slice(),
                "{damage}: existing members were overwritten"
            );
            assert_eq!(all_member_ids(&path), ["one", "two", "three"], "{damage}");
            assert_eq!(
                archive
                    .list(&ArchiveFilter::default(), 10, 0)
                    .unwrap()
                    .0
                    .len(),
                3,
                "{damage}"
            );
            assert_eq!(
                archive.get("three").unwrap().unwrap()["record"]["askId"],
                "three"
            );
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn append_quarantines_unindexed_crc_failure_bytes_and_continues() {
        let dir = std::env::temp_dir().join(format!("cutex-archive-crc-{}", uuid::Uuid::new_v4()));
        let archive = RecordArchive::at(&dir);
        let now = Utc::now();
        archive.append(record("one", now), now).unwrap();
        archive.append(record("two", now), now).unwrap();
        let path = month_path(&archive, now);
        let mut corrupted = std::fs::read(&path).unwrap();
        let crc = corrupted.len() - 8;
        corrupted[crc] ^= 1;
        std::fs::write(&path, &corrupted).unwrap();
        archive
            .connection()
            .unwrap()
            .execute("DELETE FROM records WHERE record_id = 'two'", [])
            .unwrap();
        archive.append(record("three", now), now).unwrap();
        let orphan = orphan_bytes(&dir);
        assert_eq!(orphan.len(), 1);
        assert!(corrupted.ends_with(&orphan[0]));
        assert_eq!(all_member_ids(&path), ["one", "three"]);
        assert!(archive.get("three").unwrap().is_some());
        assert_eq!(
            archive.get("one").unwrap().unwrap()["record"]["askId"],
            "one"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn append_preserves_complete_gzip_with_unindexable_record_and_refuses_to_extend_it() {
        let dir = std::env::temp_dir().join(format!(
            "cutex-archive-invalid-record-{}",
            uuid::Uuid::new_v4()
        ));
        let archive = RecordArchive::at(&dir);
        let now = Utc::now();
        archive.append(record("one", now), now).unwrap();
        let path = month_path(&archive, now);
        let mut member = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        member
            .write_all(b"{\"recordId\":\"unknown-layout\"}\n")
            .unwrap();
        let mut handle = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        handle.write_all(&member.finish().unwrap()).unwrap();
        handle.sync_all().unwrap();
        drop(handle);
        let original = std::fs::read(&path).unwrap();
        assert!(archive
            .append(record("two", now), now)
            .unwrap_err()
            .to_string()
            .contains("archive retained"));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(all_member_ids(&path), ["one", "unknown-layout"]);
        assert!(archive.get("two").unwrap().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn independent_archive_writers_are_idempotent_and_members_do_not_overlap() {
        let dir =
            std::env::temp_dir().join(format!("cutex-archive-writers-{}", uuid::Uuid::new_v4()));
        let archive = RecordArchive::at(&dir);
        drop(archive.connection().unwrap());
        let now = Utc::now();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let workers: Vec<_> = (0..4)
            .map(|i| {
                let other = RecordArchive::at(&dir);
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    other.append(record("shared", now), now).unwrap();
                    for n in 0..4 {
                        other.append(record(&format!("{i}-{n}"), now), now).unwrap();
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let mut ids = all_member_ids(&month_path(&archive, now));
        assert_eq!(
            ids.len(),
            17,
            "one shared record and sixteen unique records"
        );
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 17);
        for id in &ids {
            assert_eq!(
                archive.get(id).unwrap().unwrap()["record"]["askId"],
                id.as_str()
            );
        }
        let connection = archive.connection().unwrap();
        let mut statement = connection
            .prepare("SELECT offset, length FROM records ORDER BY offset")
            .unwrap();
        let ranges: Vec<(i64, i64)> = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let mut end = 0;
        for (offset, length) in ranges {
            assert_eq!(offset, end);
            end += length;
        }
        assert_eq!(
            end as u64,
            std::fs::metadata(month_path(&archive, now)).unwrap().len()
        );
        drop(statement);
        drop(connection);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn append_refuses_to_extend_a_file_shorter_than_the_committed_index() {
        let dir =
            std::env::temp_dir().join(format!("cutex-archive-short-file-{}", uuid::Uuid::new_v4()));
        let archive = RecordArchive::at(&dir);
        let now = Utc::now();
        archive.append(record("one", now), now).unwrap();
        let path = month_path(&archive, now);
        let shorter = std::fs::metadata(&path).unwrap().len() - 1;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(shorter)
            .unwrap();
        assert!(archive
            .append(record("two", now), now)
            .unwrap_err()
            .to_string()
            .contains("shorter than its committed index"));
        assert_eq!(std::fs::metadata(&path).unwrap().len(), shorter);
        assert!(archive.get("two").unwrap().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn append_read_list_and_prune_by_month() {
        let dir =
            std::env::temp_dir().join(format!("cutex-archive-{}", uuid::Uuid::new_v4().simple()));
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
        assert_eq!(
            archive.get("ask-1").unwrap().unwrap()["record"]["askId"],
            "ask-1"
        );
        let (all, more) = archive.list(&ArchiveFilter::default(), 10, 0).unwrap();
        assert_eq!((all.len(), more), (3, false));
        let (page, more) = archive.list(&ArchiveFilter::default(), 1, 0).unwrap();
        assert_eq!((page.len(), more), (1, true));
        let filter = ArchiveFilter {
            query: Some("TITLE ask-1".into()),
            ..Default::default()
        };
        assert_eq!(archive.list(&filter, 10, 0).unwrap().0.len(), 1);
        assert_eq!(archive.prune(365, now).unwrap(), (1, 1));
        assert!(archive.get("ask-old").unwrap().is_none());
        assert!(archive.get("ask-1").unwrap().is_some());
        assert_eq!(
            archive
                .prune(0, now + chrono::Duration::days(4000))
                .unwrap(),
            (0, 0),
            "0 keeps"
        );
        assert_eq!(archive.usage().unwrap()["records"], 2);
        let _ = std::fs::remove_dir_all(dir);
    }
}
