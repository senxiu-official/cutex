//! SQLite index over the usage ledger (design 058 S1).
//!
//! The `YYYY-MM.jsonl` files under `usage-ledger/` and
//! `usage-ledger-external/<source>/` stay the canonical record; this index is
//! derived from them and can be dropped and rebuilt at any time. It holds one
//! row per distinct entry id (first seen in the old reader's file order wins)
//! with the columns reports filter on, plus the byte range of the entry's
//! line, so a report decodes only the lines it selected.
//!
//! Every file has a checkpoint: the byte length already indexed. Appends
//! advance it under the usage store lock right after the line is synced; a
//! read catches up any tail beyond it (an older binary, a crash between the
//! append and the index commit, a missing index), so recovery scans only
//! what follows the checkpoint. A file that shrank or whose first bytes
//! changed (purged and re-imported) is re-indexed from the start; rows of a
//! file that no longer exists are dropped.
//!
//! All index writes happen under the usage store lock. Readers pin a SQLite
//! snapshot and open the ledger files while holding it, then decode outside.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use rusqlite::params;
use rusqlite::Connection;
use rusqlite::OptionalExtension;
use sha2::Digest;

use crate::store::domain;
use crate::store::metrics;
use crate::store::metrics::OpCounts;
use crate::store::StoreDb;

use super::model::parse_timestamp;
use super::model::UsageLedgerEntry;
use super::store::USAGE_EXTERNAL_DIR;
use super::store::USAGE_LEDGER_DIR;

pub(super) const USAGE_INDEX_FILE: &str = "usage-index.sqlite3";
/// Bump when the schema or the meaning of a column changes: the index is
/// derived, so a different version is dropped and rebuilt.
const INDEX_FORMAT: i64 = 2;
/// Bytes of a file's start hashed to recognise a replaced file.
const FINGERPRINT_BYTES: u64 = 4096;
/// Source key of cutex's own ledger in `files.source` / `entries.source`.
const OWN_SOURCE: &str = "";
const KIND_USAGE: i64 = 0;
const KIND_RESET: i64 = 1;

/// One ledger file on disk: `source` is `""` for cutex's own ledger, else the
/// external source directory name; `month` is the file stem.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct LedgerFileKey {
    pub source: String,
    pub month: String,
}

impl LedgerFileKey {
    pub fn path(&self, root: &Path) -> PathBuf {
        let dir = if self.source.is_empty() {
            root.join(USAGE_LEDGER_DIR)
        } else {
            root.join(USAGE_EXTERNAL_DIR).join(&self.source)
        };
        dir.join(format!("{}.jsonl", self.month))
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStats {
    pub files_seen: u64,
    pub files_reindexed: u64,
    pub files_dropped: u64,
    pub bytes_scanned: u64,
    pub lines_scanned: u64,
    pub entries_added: u64,
}

/// The usage index database: domain `usage` of the 058 store layer
/// (`crate::store`), opened per ledger root.
pub(super) struct UsageIndex {
    db: Arc<StoreDb>,
}

/// Open index databases by path, for the life of the process.
static OPEN: std::sync::OnceLock<std::sync::Mutex<HashMap<PathBuf, Arc<StoreDb>>>> = std::sync::OnceLock::new();

/// Manifest id of the index (`crate::store::manifest`).
const STORE_ID: &str = "usage-index";

fn index_path(root: &Path) -> PathBuf {
    root.join(USAGE_INDEX_FILE)
}

/// Tables this index owns (dropped together on a format change).
const TABLES: [&str; 6] = [
    "usage_index_native_threads",
    "usage_index_entries",
    "usage_index_duplicates",
    "usage_index_warnings",
    "usage_index_files",
    "usage_index_meta",
];

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS usage_index_native_threads (
  thread_id TEXT NOT NULL,
  file_id INTEGER NOT NULL,
  PRIMARY KEY (thread_id, file_id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS usage_index_native_threads_file ON usage_index_native_threads(file_id);
CREATE TABLE IF NOT EXISTS usage_index_meta (
  key TEXT PRIMARY KEY,
  value INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS usage_index_files (
  file_id INTEGER PRIMARY KEY,
  source TEXT NOT NULL,
  month TEXT NOT NULL,
  checkpoint INTEGER NOT NULL,
  lines INTEGER NOT NULL,
  last_unterminated INTEGER NOT NULL,
  fingerprint BLOB NOT NULL,
  UNIQUE (source, month)
);
CREATE TABLE IF NOT EXISTS usage_index_entries (
  entry_id TEXT PRIMARY KEY,
  observed_at TEXT NOT NULL,
  observed_ms INTEGER NOT NULL,
  kind INTEGER NOT NULL,
  source TEXT NOT NULL,
  profile_id TEXT,
  file_id INTEGER NOT NULL,
  offset INTEGER NOT NULL,
  len INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS usage_index_entries_time ON usage_index_entries (observed_ms);
CREATE INDEX IF NOT EXISTS usage_index_entries_profile ON usage_index_entries (profile_id, observed_ms);
CREATE INDEX IF NOT EXISTS usage_index_entries_file ON usage_index_entries (file_id);
CREATE TABLE IF NOT EXISTS usage_index_duplicates (
  entry_id TEXT NOT NULL,
  observed_at TEXT NOT NULL,
  observed_ms INTEGER NOT NULL,
  kind INTEGER NOT NULL,
  source TEXT NOT NULL,
  profile_id TEXT,
  file_id INTEGER NOT NULL,
  offset INTEGER NOT NULL,
  len INTEGER NOT NULL,
  PRIMARY KEY (entry_id, file_id, offset)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS usage_index_warnings (
  file_id INTEGER NOT NULL,
  line INTEGER NOT NULL,
  message TEXT NOT NULL,
  PRIMARY KEY (file_id, line)
) WITHOUT ROWID;
";

impl UsageIndex {
    /// Native membership is independent of report entry-ID precedence. Even a
    /// malformed usage row with a readable threadId conservatively claims it,
    /// matching the predecessor collector's ThreadOnly scan.
    pub fn native_thread_exists(&self, thread: &str) -> anyhow::Result<bool> {
        self.db.read(|tx| {
            let found = tx.sql().query_row(
                "SELECT EXISTS(SELECT 1 FROM usage_index_native_threads WHERE thread_id=?1)",
                [thread],
                |row| row.get(0),
            )?;
            metrics::note(
                STORE_ID,
                OpCounts {
                    sql_statements: 1,
                    rows_visited: 1,
                    ..Default::default()
                },
            );
            Ok(found)
        })
    }
    pub fn sync_native(&self, root: &Path) -> anyhow::Result<SyncStats> {
        let months = month_stems(&root.join(USAGE_LEDGER_DIR))?;
        let stats = self.db.write(|tx| {
            let known = known_files(tx.sql())?;
            let mut stats = SyncStats::default();
            for (key, state) in &known {
                if key.source == OWN_SOURCE && !months.contains(&key.month) {
                    drop_file(tx.sql(), state.file_id)?;
                    stats.files_dropped += 1;
                }
            }
            for month in &months {
                let key = LedgerFileKey {
                    source: OWN_SOURCE.into(),
                    month: month.clone(),
                };
                stats.files_seen += 1;
                sync_file(tx.sql(), root, &key, known.get(&key), true, &mut stats)?;
            }
            Ok(stats)
        })?;
        note_sync(&stats);
        Ok(stats)
    }

    /// Open (or create) the index of `root`. A database of another format
    /// or one SQLite cannot read is removed and recreated empty; the next
    /// sync rebuilds it from the ledger.
    pub fn open(root: &Path) -> anyhow::Result<Self> {
        let path = index_path(root);
        let mut open = OPEN.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
        if let Some(db) = open.get(&path) {
            return Ok(Self { db: db.clone() });
        }
        let index = match Self::open_existing(&path) {
            Ok(index) => index,
            Err(error) if is_unusable(&error) => {
                eprintln!("usage index unusable, rebuilding: {error:#}");
                remove_index_files(&path)?;
                Self::open_existing(&path)?
            }
            Err(error) => return Err(error),
        };
        // Kept open for the process (the store layer's per-path reuse); the
        // schema check above runs once. Tests reopen, so their temporary
        // roots can be deleted.
        if !cfg!(test) {
            open.insert(path, index.db.clone());
        }
        Ok(index)
    }

    fn open_existing(path: &Path) -> anyhow::Result<Self> {
        let db = StoreDb::open_at(&domain::USAGE, path)?;
        super::store::secure_file(path)?;
        db.write(|tx| {
            let sql = tx.sql();
            sql.execute_batch(SCHEMA)?;
            let format: Option<i64> = sql
                .query_row("SELECT value FROM usage_index_meta WHERE key = 'format'", [], |row| row.get(0))
                .optional()?;
            if format.is_some_and(|format| format != INDEX_FORMAT) {
                // Derived data: another format is dropped and rebuilt.
                for table in TABLES {
                    sql.execute_batch(&format!("DROP TABLE {table};"))?;
                }
                sql.execute_batch(SCHEMA)?;
            }
            if format != Some(INDEX_FORMAT) {
                sql.execute("INSERT INTO usage_index_meta (key, value) VALUES ('format', ?1)", [INDEX_FORMAT])?;
            }
            Ok(())
        })?;
        Ok(Self { db })
    }

    /// Bring the index up to the ledger files under `root`: index tails past
    /// the checkpoints, re-index replaced files, drop vanished files.
    /// `verify` also compares the start fingerprint of unchanged-length
    /// files (once per process and root; cheap: 4 KiB per file).
    pub fn sync(&self, root: &Path, verify: bool) -> anyhow::Result<SyncStats> {
        let on_disk = ledger_files(root)?;
        let stats = self.db.write(|tx| {
            let sql = tx.sql();
            let known = known_files(sql)?;
            let mut stats = SyncStats { files_seen: on_disk.len() as u64, ..SyncStats::default() };
            for (key, state) in &known {
                if !on_disk.contains(key) {
                    drop_file(sql, state.file_id)?;
                    stats.files_dropped += 1;
                }
            }
            for key in &on_disk {
                sync_file(sql, root, key, known.get(key), verify, &mut stats)?;
            }
            Ok(stats)
        })?;
        note_sync(&stats);
        Ok(stats)
    }

    /// Index whatever follows the checkpoint of the given files (after an
    /// append under the same lock).
    pub fn sync_files(&self, root: &Path, keys: &[LedgerFileKey]) -> anyhow::Result<SyncStats> {
        let stats = self.db.write(|tx| {
            let sql = tx.sql();
            let known = known_files(sql)?;
            let mut stats = SyncStats::default();
            for key in keys {
                if key.path(root).exists() {
                    stats.files_seen += 1;
                    sync_file(sql, root, key, known.get(key), false, &mut stats)?;
                }
            }
            Ok(stats)
        })?;
        note_sync(&stats);
        Ok(stats)
    }

    /// Forget every row of one external source (its directory was removed).
    pub fn purge_source(&self, source: &str) -> anyhow::Result<()> {
        self.db.write(|tx| {
            let sql = tx.sql();
            let ids = sql
                .prepare("SELECT file_id FROM usage_index_files WHERE source = ?1")?
                .query_map([source], |row| row.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            for id in ids {
                drop_file(sql, id)?;
            }
            Ok(())
        })
    }

    /// Run `read` on one SQLite snapshot of the index. The snapshot is
    /// pinned and the visible ledger files (with `months`: only `YYYY-MM`
    /// files in `[first, last]`, as the month reader) are opened before
    /// `locked` (the usage store lock) is dropped; decoding then runs
    /// outside the lock and never sees later appends or purges.
    pub fn read<T, G>(
        &self,
        root: &Path,
        locked: G,
        visible: &dyn Fn(&str) -> bool,
        months: Option<(&str, &str)>,
        read: impl FnOnce(&mut IndexReader<'_>) -> anyhow::Result<T>,
    ) -> anyhow::Result<(T, ReadStats)> {
        self.db.read(|tx| {
            let sql = tx.sql();
            let mut files = HashMap::new();
            let mut keys = HashMap::new();
            for (key, state) in known_files(sql)? {
                let in_months = months.is_none_or(|(first, last)| {
                    key.month.len() == 7 && key.month.as_str() >= first && key.month.as_str() <= last
                });
                // As the month reader: files outside the months are not read
                // at all (also not as duplicate candidates).
                if !visible(&key.source) || !in_months {
                    continue;
                }
                let path = key.path(root);
                let file = File::open(&path)
                    .with_context(|| format!("Failed to open usage ledger: {}", path.display()))?;
                files.insert(state.file_id, file);
                keys.insert(state.file_id, (key, path));
            }
            drop(locked);
            let mut reader = IndexReader { sql, files, keys, stats: ReadStats::default() };
            let value = read(&mut reader)?;
            let stats = reader.stats;
            metrics::note(
                STORE_ID,
                OpCounts {
                    rows_visited: stats.rows_visited,
                    rows_materialized: stats.rows_decoded,
                    bytes_read: stats.bytes_decoded,
                    bytes_decoded: stats.bytes_decoded,
                    ..OpCounts::default()
                },
            );
            Ok((value, stats))
        })
    }
}

fn note_sync(stats: &SyncStats) {
    metrics::note(
        STORE_ID,
        OpCounts {
            bytes_read: stats.bytes_scanned,
            bytes_decoded: stats.bytes_scanned,
            rows_visited: stats.lines_scanned,
            rows_materialized: stats.entries_added,
            ..OpCounts::default()
        },
    );
}

/// One row per ledger file (months × sources): a few dozen rows.
fn known_files(sql: &Connection) -> anyhow::Result<BTreeMap<LedgerFileKey, FileState>> {
    let mut statement = sql.prepare(
        "SELECT file_id, source, month, checkpoint, lines, last_unterminated, fingerprint
         FROM usage_index_files ORDER BY file_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            LedgerFileKey { source: row.get(1)?, month: row.get(2)? },
            FileState {
                file_id: row.get(0)?,
                checkpoint: row.get::<_, i64>(3)? as u64,
                lines: row.get::<_, i64>(4)? as u64,
                last_unterminated: row.get::<_, i64>(5)? != 0,
                fingerprint: row.get(6)?,
            },
        ))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

struct FileState {
    file_id: i64,
    checkpoint: u64,
    lines: u64,
    last_unterminated: bool,
    fingerprint: Vec<u8>,
}

#[derive(Debug)]
struct UnusableIndex(String);

impl std::fmt::Display for UnusableIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UnusableIndex {}

fn is_unusable(error: &anyhow::Error) -> bool {
    if error.downcast_ref::<UnusableIndex>().is_some() {
        return true;
    }
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(failure, _))
                if matches!(failure.code, rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt)
        )
    })
}

fn remove_index_files(path: &Path) -> anyhow::Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        match fs::remove_file(PathBuf::from(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("remove unusable usage index"),
        }
    }
    Ok(())
}

/// Ledger files in the old reader's order: own ledger first, then external
/// sources by directory name; months sorted within each.
fn ledger_files(root: &Path) -> anyhow::Result<Vec<LedgerFileKey>> {
    let mut out = Vec::new();
    for month in month_stems(&root.join(USAGE_LEDGER_DIR))? {
        out.push(LedgerFileKey { source: OWN_SOURCE.to_string(), month });
    }
    let base = root.join(USAGE_EXTERNAL_DIR);
    let mut sources = match fs::read_dir(&base) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to read usage ledger: {}", base.display()))
        }
    };
    sources.sort();
    for source in sources {
        for month in month_stems(&base.join(&source))? {
            out.push(LedgerFileKey { source: source.clone(), month });
        }
    }
    Ok(out)
}

fn month_stems(dir: &Path) -> anyhow::Result<Vec<String>> {
    let mut stems = match fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|value| value == "jsonl"))
            .filter_map(|path| path.file_stem().and_then(|stem| stem.to_str()).map(str::to_string))
            .collect::<Vec<_>>(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to read usage ledger: {}", dir.display()))
        }
    };
    stems.sort();
    Ok(stems)
}

fn drop_file(tx: &Connection, file_id: i64) -> anyhow::Result<()> {
    tx.execute("DELETE FROM usage_index_native_threads WHERE file_id = ?1", [file_id])?;
    tx.execute("DELETE FROM usage_index_entries WHERE file_id = ?1", [file_id])?;
    tx.execute("DELETE FROM usage_index_warnings WHERE file_id = ?1", [file_id])?;
    tx.execute("DELETE FROM usage_index_duplicates WHERE file_id = ?1", [file_id])?;
    tx.execute("DELETE FROM usage_index_files WHERE file_id = ?1", [file_id])?;
    Ok(())
}

fn fingerprint(file: &mut File, length: u64) -> anyhow::Result<Vec<u8>> {
    let mut head = Vec::new();
    file.seek(SeekFrom::Start(0))?;
    file.by_ref().take(length.min(FINGERPRINT_BYTES)).read_to_end(&mut head)?;
    Ok(sha2::Sha256::digest(&head).to_vec())
}

fn sync_file(
    tx: &Connection,
    root: &Path,
    key: &LedgerFileKey,
    known: Option<&FileState>,
    verify: bool,
    stats: &mut SyncStats,
) -> anyhow::Result<()> {
    let path = key.path(root);
    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(state) = known {
                drop_file(tx, state.file_id)?;
                stats.files_dropped += 1;
            }
            return Ok(());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to open usage ledger: {}", path.display()))
        }
    };
    let length = file.metadata()?.len();
    let mut start = match known {
        Some(state) if length < state.checkpoint => None,
        Some(state) if length == state.checkpoint && !verify => return Ok(()),
        Some(state) => {
            let head = fingerprint(&mut file, state.checkpoint)?;
            (head == state.fingerprint).then_some(state)
        }
        None => None,
    }
    .map(|state| (state.file_id, state.checkpoint, state.lines, state.last_unterminated));
    if start.is_some_and(|(_, checkpoint, _, _)| checkpoint == length) {
        return Ok(());
    }
    if start.is_none() {
        if let Some(state) = known {
            drop_file(tx, state.file_id)?;
            stats.files_reindexed += 1;
        }
    }
    let file_id = match start {
        Some((file_id, ..)) => file_id,
        None => {
            tx.execute(
                "INSERT INTO usage_index_files (source, month, checkpoint, lines, last_unterminated, fingerprint)
                 VALUES (?1, ?2, 0, 0, 0, x'')",
                params![key.source, key.month],
            )?;
            tx.last_insert_rowid()
        }
    };
    let (offset, mut lines, mut unterminated) = start.take().map_or((0, 0, false), |(_, c, l, u)| (c, l, u));
    file.seek(SeekFrom::Start(offset))?;
    let mut reader = BufReader::with_capacity(1 << 20, file.by_ref().take(length - offset));
    stats.bytes_scanned += length - offset;

    let mut insert_entry = tx.prepare_cached(
        "INSERT OR IGNORE INTO usage_index_entries
         (entry_id, observed_at, observed_ms, kind, source, profile_id, file_id, offset, len)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    let mut insert_warning = tx.prepare_cached(
        "INSERT OR REPLACE INTO usage_index_warnings (file_id, line, message) VALUES (?1, ?2, ?3)",
    )?;
    let mut select_primary =
        tx.prepare_cached("SELECT file_id FROM usage_index_entries WHERE entry_id = ?1")?;
    let mut insert_duplicate = tx.prepare_cached(
        "INSERT OR IGNORE INTO usage_index_duplicates
         (entry_id, observed_at, observed_ms, kind, source, profile_id, file_id, offset, len)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    let mut piece = Vec::new();
    let mut position = offset;
    loop {
        piece.clear();
        let read = reader.read_until(b'\n', &mut piece)?;
        if read == 0 {
            break;
        }
        let line_offset = position;
        position += read as u64;
        let terminated = piece.last() == Some(&b'\n');
        if terminated {
            piece.pop();
        }
        if unterminated {
            // The previous indexed line had no newline yet: this piece ends
            // it. Writers start every append with a newline, so this piece
            // is empty; anything else changed an indexed line.
            unterminated = false;
            if !piece.is_empty() {
                drop((insert_entry, insert_warning, select_primary, insert_duplicate, reader));
                return reindex_file(tx, root, key, file_id, stats);
            }
            if terminated {
                continue;
            }
            break;
        }
        if piece.is_empty() && !terminated {
            break;
        }
        lines += 1;
        unterminated = !terminated;
        stats.lines_scanned += 1;
        let mut bytes = piece.as_slice();
        if terminated && bytes.last() == Some(&b'\r') {
            bytes = &bytes[..bytes.len() - 1];
        }
        if key.source == OWN_SOURCE {
            metrics::note(STORE_ID,OpCounts{bytes_decoded:bytes.len() as u64,..Default::default()});
            #[derive(serde::Deserialize)]
            struct ThreadOnly { #[serde(rename="threadId")] thread: Option<String> }
            if let Ok(ThreadOnly { thread:Some(thread) }) = serde_json::from_slice(bytes) {
                tx.execute("INSERT OR IGNORE INTO usage_index_native_threads(thread_id,file_id) VALUES (?1,?2)", params![thread,file_id])?;
            }
        }
        let warning = match index_line(bytes) {
            Ok(None) => None,
            Ok(Some(row)) => {
                let values = params![
                    row.entry_id,
                    row.observed_at,
                    row.observed_ms,
                    row.kind,
                    row.source,
                    row.profile_id,
                    file_id,
                    line_offset as i64,
                    bytes.len() as i64,
                ];
                let changed = insert_entry.execute(values)?;
                stats.entries_added += changed as u64;
                if changed == 0 {
                    // Seen before. A repeat in the same file can never win
                    // (the old reader keeps the first line); one in another
                    // file can, when that file is hidden or purged later.
                    let primary: i64 = select_primary.query_row([&row.entry_id], |found| found.get(0))?;
                    if primary != file_id {
                        insert_duplicate.execute(values)?;
                    }
                }
                None
            }
            Err(message) => Some(message),
        };
        if let Some(message) = warning {
            insert_warning.execute(params![file_id, lines as i64, message])?;
        }
    }
    drop(reader);
    let fingerprint = fingerprint(&mut file, length)?;
    tx.execute(
        "UPDATE usage_index_files SET checkpoint = ?2, lines = ?3, last_unterminated = ?4, fingerprint = ?5 WHERE file_id = ?1",
        params![file_id, length as i64, lines as i64, unterminated as i64, fingerprint],
    )?;
    Ok(())
}

fn reindex_file(
    tx: &Connection,
    root: &Path,
    key: &LedgerFileKey,
    file_id: i64,
    stats: &mut SyncStats,
) -> anyhow::Result<()> {
    drop_file(tx, file_id)?;
    stats.files_reindexed += 1;
    sync_file(tx, root, key, None, false, stats)
}

struct IndexRow {
    entry_id: String,
    observed_at: String,
    observed_ms: i64,
    kind: i64,
    source: String,
    profile_id: Option<String>,
}

/// One ledger line as the old reader judged it: `Ok(None)` blank,
/// `Err(warning)` skipped with that warning (without the `path:line: `
/// prefix), else the row to index.
fn index_line(bytes: &[u8]) -> Result<Option<IndexRow>, String> {
    let Ok(line) = std::str::from_utf8(bytes) else {
        return Err("failed to read usage ledger line: stream did not contain valid UTF-8".to_string());
    };
    if line.trim().is_empty() {
        return Ok(None);
    }
    let entry = serde_json::from_str::<UsageLedgerEntry>(line)
        .map_err(|error| format!("ignored invalid usage ledger JSON: {error}"))?;
    entry
        .validate()
        .map_err(|error| format!("ignored invalid usage ledger entry: {error:#}"))?;
    let observed_ms = parse_timestamp(entry.observed_at())
        .map_err(|error| format!("ignored invalid usage ledger entry: {error:#}"))?
        .timestamp_millis();
    let (kind, source, profile_id) = match &entry {
        UsageLedgerEntry::Usage(sample) => (
            KIND_USAGE,
            sample.source.as_ref().map_or_else(String::new, |source| source.id.clone()),
            sample.profile_id.clone(),
        ),
        UsageLedgerEntry::ResetBoundary(boundary) => (KIND_RESET, String::new(), boundary.profile_id.clone()),
    };
    Ok(Some(IndexRow {
        entry_id: entry.entry_id().to_string(),
        observed_at: entry.observed_at().to_string(),
        observed_ms,
        kind,
        source,
        profile_id,
    }))
}

/// Which usage samples a read selects (all by index columns).
#[derive(Debug, Clone, Default)]
pub(super) struct SampleFilter {
    /// Inclusive lower / exclusive upper bound on the observed instant (ms).
    pub from_ms: i64,
    pub to_ms: i64,
    /// `?source=` of `GET /v2/usage`: `cutex`, `external` or a source id.
    pub source: Option<String>,
    /// Profile ids; rows without a stored id are always candidates (their
    /// profile is resolved by name at query time).
    pub profile_ids: Option<Vec<String>>,
}

impl SampleFilter {
    fn matches(&self, row: &Occurrence) -> bool {
        row.kind == KIND_USAGE
            && row.observed_ms >= self.from_ms
            && row.observed_ms < self.to_ms
            && match self.source.as_deref() {
                None => true,
                Some("cutex") => row.source.is_empty(),
                Some("external") => !row.source.is_empty(),
                Some(id) => row.source == id,
            }
            && self.profile_ids.as_ref().is_none_or(|ids| {
                row.profile_id.as_ref().is_none_or(|id| ids.contains(id))
            })
    }
}

/// Work counters of one read (design 058 §7).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReadStats {
    /// Index rows the query returned (before decoding).
    pub rows_visited: u64,
    /// Ledger lines decoded.
    pub rows_decoded: u64,
    pub bytes_decoded: u64,
}

/// An entry id's place in one file.
#[derive(Debug, Clone)]
struct Occurrence {
    entry_id: String,
    observed_at: String,
    observed_ms: i64,
    kind: i64,
    source: String,
    profile_id: Option<String>,
    file_id: i64,
    offset: u64,
    len: usize,
}

const OCCURRENCE_COLUMNS: &str = "entry_id, observed_at, observed_ms, kind, source, profile_id, file_id, offset, len";

fn occurrence(row: &rusqlite::Row<'_>) -> rusqlite::Result<Occurrence> {
    Ok(Occurrence {
        entry_id: row.get(0)?,
        observed_at: row.get(1)?,
        observed_ms: row.get(2)?,
        kind: row.get(3)?,
        source: row.get(4)?,
        profile_id: row.get(5)?,
        file_id: row.get(6)?,
        offset: row.get::<_, i64>(7)? as u64,
        len: row.get::<_, i64>(8)? as usize,
    })
}

pub(super) struct IndexReader<'a> {
    sql: &'a Connection,
    files: HashMap<i64, File>,
    /// Visible files only.
    keys: HashMap<i64, (LedgerFileKey, PathBuf)>,
    stats: ReadStats,
}

impl IndexReader<'_> {
    /// Warnings of the invalid lines of visible files (with `months`: only
    /// `YYYY-MM` files in `[first, last]`), in the old reader's order and
    /// wording.
    pub fn warnings(&self, months: Option<(&str, &str)>) -> anyhow::Result<Vec<String>> {
        let (first, last, any) = match months {
            Some((first, last)) => (first, last, 0),
            None => ("", "", 1),
        };
        let mut statement = self.sql.prepare(
            "SELECT w.file_id, w.line, w.message FROM usage_index_warnings w
             JOIN usage_index_files f ON f.file_id = w.file_id
             WHERE ?3 = 1 OR (length(f.month) = 7 AND f.month >= ?1 AND f.month <= ?2)
             ORDER BY f.source, f.month, w.line",
        )?;
        let rows = statement.query_map(params![first, last, any], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (file_id, line, message) = row?;
            if let Some((_, path)) = self.keys.get(&file_id) {
                out.push(format!("{}:{line}: {message}", path.display()));
            }
        }
        Ok(out)
    }

    /// Visit the selected usage samples in the old ledger order
    /// (`observedAt` text, then entry id), decoding only those lines.
    pub fn for_each_sample(
        &mut self,
        filter: &SampleFilter,
        mut visit: impl FnMut(super::model::UsageSample) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        self.stream(Some(filter), |entry| match entry {
            UsageLedgerEntry::Usage(sample) => visit(*sample),
            UsageLedgerEntry::ResetBoundary(_) => Ok(()),
        })
    }

    /// Every entry (both kinds) of the visible files, in the old ledger
    /// order. For whole-ledger consumers (CLI report, TUI).
    pub fn for_each_entry(
        &mut self,
        visit: impl FnMut(UsageLedgerEntry) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        self.stream(None, visit)
    }

    /// The winning occurrence of every entry id that also appears in
    /// another file: the first one in the old reader's file order among the
    /// visible files (`None`: no visible occurrence). Normally empty.
    fn duplicate_winners(&self) -> anyhow::Result<HashMap<String, Option<Occurrence>>> {
        let mut statement = self.sql.prepare(&format!(
            "SELECT {OCCURRENCE_COLUMNS} FROM usage_index_duplicates
             UNION ALL
             SELECT {OCCURRENCE_COLUMNS} FROM usage_index_entries
             WHERE entry_id IN (SELECT entry_id FROM usage_index_duplicates)"
        ))?;
        let mut winners = HashMap::<String, Option<Occurrence>>::new();
        for row in statement.query_map([], occurrence)? {
            let row = row?;
            let slot = winners.entry(row.entry_id.clone()).or_insert(None);
            let Some((key, _)) = self.keys.get(&row.file_id) else { continue };
            let better = match slot {
                None => true,
                Some(current) => {
                    let (current_key, _) = &self.keys[&current.file_id];
                    (key, row.offset) < (current_key, current.offset)
                }
            };
            if better {
                *slot = Some(row);
            }
        }
        Ok(winners)
    }

    fn stream(
        &mut self,
        filter: Option<&SampleFilter>,
        mut visit: impl FnMut(UsageLedgerEntry) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let winners = self.duplicate_winners()?;
        let mut extra = winners
            .values()
            .flatten()
            .filter(|row| filter.is_none_or(|filter| filter.matches(row)))
            .cloned()
            .collect::<Vec<_>>();
        extra.sort_by(|left, right| {
            (left.observed_at.as_str(), left.entry_id.as_str()).cmp(&(right.observed_at.as_str(), right.entry_id.as_str()))
        });
        let mut extra = extra.into_iter().peekable();

        // Without duplicates (the normal case) only the line position is
        // needed per row.
        let light = winners.is_empty();
        let columns = if light { "file_id, offset, len" } else { "file_id, offset, len, entry_id, observed_at" };
        // `+file_id`: never pick the per-file index for a range read.
        let mut sql = format!("SELECT {columns} FROM usage_index_entries WHERE +file_id IN (");
        let mut values: Vec<rusqlite::types::Value> = Vec::new();
        let mut file_ids = self.keys.keys().copied().collect::<Vec<_>>();
        file_ids.sort_unstable();
        sql.push_str(&placeholders(0, file_ids.len()));
        sql.push(')');
        values.extend(file_ids.into_iter().map(rusqlite::types::Value::from));
        if let Some(filter) = filter {
            let (from, to) = (values.len() + 1, values.len() + 2);
            values.push(filter.from_ms.into());
            values.push(filter.to_ms.into());
            let range = format!("observed_ms >= ?{from} AND observed_ms < ?{to}");
            match &filter.profile_ids {
                // Each branch is a range on (profile_id, observed_ms).
                Some(ids) => {
                    sql.push_str(&format!(
                        " AND ((profile_id IS NULL AND {range}) OR (profile_id IN ({}) AND {range}))",
                        placeholders(values.len(), ids.len())
                    ));
                    values.extend(ids.iter().cloned().map(rusqlite::types::Value::from));
                }
                None => sql.push_str(&format!(" AND {range}")),
            }
            sql.push_str(&format!(" AND kind = {KIND_USAGE}"));
            match filter.source.as_deref() {
                None => {}
                Some("cutex") => sql.push_str(" AND source = ''"),
                Some("external") => sql.push_str(" AND source <> ''"),
                Some(id) => {
                    sql.push_str(&format!(" AND source = ?{}", values.len() + 1));
                    values.push(id.to_string().into());
                }
            }
        }
        sql.push_str(" ORDER BY observed_at, entry_id");
        let mut statement = self.sql.prepare(&sql)?;
        let mut rows = statement.query(rusqlite::params_from_iter(values))?;
        let mut stats = self.stats;
        let files = &mut self.files;
        let keys = &self.keys;
        let mut windows = HashMap::<i64, Window>::new();
        let mut decode = |file_id: i64, offset: u64, len: usize, stats: &mut ReadStats| -> anyhow::Result<UsageLedgerEntry> {
            let file = match files.entry(file_id) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let path = &keys[&file_id].1;
                    entry.insert(
                        File::open(path)
                            .with_context(|| format!("Failed to open usage ledger: {}", path.display()))?,
                    )
                }
            };
            let bytes = windows.entry(file_id).or_default().line(file, offset, len)?;
            stats.rows_decoded += 1;
            stats.bytes_decoded += len as u64;
            serde_json::from_slice::<UsageLedgerEntry>(bytes)
                .with_context(|| format!("usage index points at an undecodable line (file {file_id} offset {offset})"))
        };
        while let Some(row) = rows.next()? {
            let (file_id, offset, len) = (row.get::<_, i64>(0)?, row.get::<_, i64>(1)? as u64, row.get::<_, i64>(2)? as usize);
            stats.rows_visited += 1;
            if !light {
                let (entry_id, observed_at) = (row.get::<_, String>(3)?, row.get::<_, String>(4)?);
                if winners.contains_key(&entry_id) {
                    continue;
                }
                while let Some(first) = extra.next_if(|first| {
                    (first.observed_at.as_str(), first.entry_id.as_str()) < (observed_at.as_str(), entry_id.as_str())
                }) {
                    visit(decode(first.file_id, first.offset, first.len, &mut stats)?)?;
                }
            }
            visit(decode(file_id, offset, len, &mut stats)?)?;
        }
        for first in extra {
            visit(decode(first.file_id, first.offset, first.len, &mut stats)?)?;
        }
        self.stats = stats;
        Ok(())
    }
}

fn placeholders(already: usize, count: usize) -> String {
    if count == 0 {
        return "NULL".to_string();
    }
    (1..=count).map(|n| format!("?{}", already + n)).collect::<Vec<_>>().join(",")
}


/// A read-ahead window over one ledger file: rows come back in time order,
/// which is close to file order, so most lines are served from memory
/// instead of one seek + read each.
#[derive(Default)]
struct Window {
    start: u64,
    data: Vec<u8>,
}

const WINDOW_BYTES: usize = 1 << 20;

impl Window {
    fn line(&mut self, file: &mut File, offset: u64, len: usize) -> anyhow::Result<&[u8]> {
        let end = self.start + self.data.len() as u64;
        let inside = offset >= self.start && offset + len as u64 <= end;
        if !inside {
            // Read ahead only when moving forward through the file (lines
            // appended in time order); a jump reads just the line.
            let forward = self.data.is_empty() || (offset >= self.start && offset < end + WINDOW_BYTES as u64);
            self.start = offset;
            self.data.clear();
            file.seek(SeekFrom::Start(offset))?;
            let size = if forward { len.max(WINDOW_BYTES) } else { len };
            file.by_ref().take(size as u64).read_to_end(&mut self.data)?;
            if self.data.len() < len {
                anyhow::bail!("usage ledger shorter than its index (offset {offset})");
            }
        }
        let at = (offset - self.start) as usize;
        Ok(&self.data[at..at + len])
    }
}
