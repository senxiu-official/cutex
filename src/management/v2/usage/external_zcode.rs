//! `zcode` extra usage source: Zhipu ZCode's per-request usage table
//! `<path>/cli/db/db.sqlite` `model_usage` (SQLite, WAL mode).
//!
//! - Opened per scan and closed right after one SELECT; never writes into
//!   the database, never checkpoints, holds a read snapshot only for that
//!   query. Two ways, by whether ZCode has the database open:
//!   - **ZCode running** (a `-wal` or `-shm` file exists): read-only
//!     connection (`SQLITE_OPEN_READ_ONLY`, `query_only`, short busy
//!     timeout) with normal locking; committed rows still in ZCode's `-wal`
//!     are seen. Its `-wal` / `-shm` already exist, so nothing is created.
//!   - **ZCode not running** (a WAL-format file without `-wal` / `-shm`): a
//!     read-only open would CREATE `db.sqlite-wal` / `-shm` and, unable to
//!     checkpoint, leave them behind. So the file is read as
//!     `file:…?mode=ro&immutable=1` (no locks, no `-wal` / `-shm`, nothing
//!     created). `immutable` is only safe while nobody writes, so the main
//!     file's size / mtime and the absence of `-wal` / `-shm` are checked
//!     before and after the query; if ZCode started meanwhile the result is
//!     discarded and the read is repeated the locked way. (ZCode in WAL mode
//!     writes into `-wal`, which it creates first, and only a checkpoint
//!     touches the main file, so a writer cannot change what an immutable
//!     read sees without one of those checks noticing.) A database that is
//!     not in WAL format is always read the locked way.
//! - Only the id / token / model / provider / timestamp columns are selected;
//!   prompts, messages, error texts, raw usage JSON and the other tables (and
//!   `provider_config.json`) are never read.
//! - One row per `model_usage.id` with `status = 'completed'`. Incremental by
//!   a `completed_at` watermark (epoch ms) with a look-back window: rows of the
//!   window already imported are remembered by id, so a row that commits late
//!   with an older `completed_at` is still picked up and none is appended twice
//!   (the reader also dedups by entry id).
//! - Tokens: ZCode's `input_tokens` already includes the cache reads
//!   (`provider_total_tokens` = input + output); a row whose input is smaller
//!   than its cache reads + writes is treated as Anthropic-style and gets them
//!   added. Total = input + output.

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use chrono::TimeZone;
use chrono::Utc;
use rusqlite::Connection;
use rusqlite::OpenFlags;
use serde::Deserialize;
use serde::Serialize;

use super::non_empty;
use super::store;
use super::FileState;
use super::RowContext;
use super::ScanState;
use super::TokenUsageBreakdown;
use super::UsageAttribution;

/// Database below the source path (the ZCode home, `~/.zcode`).
pub(super) const DB_RELATIVE: &str = "cli/db/db.sqlite";
/// Rows completing up to this long before the watermark are re-read (and
/// skipped by id when already imported).
pub(super) const LOOKBACK_MS: i64 = 15 * 60 * 1000;
const BUSY_TIMEOUT: Duration = Duration::from_millis(2000);
/// One agent row in `GET /v2/usage`: ZCode keeps no working directory here.
const PROJECT_KEY: &str = "zcode";

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(super) struct ZcodeCursor {
    /// Largest `completed_at` imported (raw column value, epoch ms).
    #[serde(default)]
    pub watermark: i64,
    /// Ids imported with `completed_at` inside the look-back window.
    #[serde(default)]
    pub recent: BTreeMap<String, i64>,
}

struct Row {
    id: String,
    session_id: String,
    turn_id: Option<String>,
    logical_request_id: Option<String>,
    provider_id: Option<String>,
    model_id: Option<String>,
    completed_at: i64,
    input: i64,
    output: i64,
    reasoning: i64,
    cache_creation: i64,
    cache_read: i64,
}

impl Row {
    fn breakdown(&self) -> TokenUsageBreakdown {
        let clamp = |value: i64| value.max(0) as u64;
        let cache_read = clamp(self.cache_read);
        let cache_write = clamp(self.cache_creation);
        let mut input = clamp(self.input);
        if input < cache_read.saturating_add(cache_write) {
            input = input.saturating_add(cache_read).saturating_add(cache_write);
        }
        let output = clamp(self.output);
        TokenUsageBreakdown {
            total_tokens: input.saturating_add(output),
            input_tokens: input,
            cached_input_tokens: cache_read,
            cache_write_input_tokens: cache_write,
            output_tokens: output,
            reasoning_output_tokens: clamp(self.reasoning),
        }
    }
}

/// `completed_at` as RFC 3339. ZCode writes epoch milliseconds; a value that
/// can only be seconds (before 1973 as ms) is read as seconds.
pub(super) fn timestamp(raw: i64) -> Option<String> {
    let millis = if raw.abs() < 100_000_000_000 { raw.checked_mul(1000)? } else { raw };
    Utc.timestamp_millis_opt(millis).single().map(|at| at.to_rfc3339())
}

/// The database file, refusing links on the way (a source reads only its
/// own tree).
fn database(base: &Path) -> Result<PathBuf, &'static str> {
    let mut path = base.to_path_buf();
    for part in DB_RELATIVE.split('/') {
        path.push(part);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => return Err("database path is a link"),
            Ok(_) => {}
            Err(_) => return Err("no ZCode usage database (cli/db/db.sqlite)"),
        }
    }
    if !path.is_file() {
        return Err("no ZCode usage database (cli/db/db.sqlite)");
    }
    Ok(path)
}

/// `<db>-wal` / `<db>-shm`.
fn sidecar(db: &Path, suffix: &str) -> PathBuf {
    let mut name = db.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// The main file's (len, mtime) when no process has it open in WAL mode:
/// WAL format (header bytes 18 / 19 = 2) and neither `-wal` nor `-shm`
/// present. `None` = read with locks.
fn quiescent_signature(db: &Path) -> Option<(u64, std::time::SystemTime)> {
    if ["-wal", "-shm"].iter().any(|suffix| std::fs::symlink_metadata(sidecar(db, suffix)).is_ok()) {
        return None;
    }
    let mut header = [0u8; 20];
    std::io::Read::read_exact(&mut std::fs::File::open(db).ok()?, &mut header).ok()?;
    if header[18] != 2 || header[19] != 2 {
        return None;
    }
    let meta = std::fs::metadata(db).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

/// `file:` URI for an absolute path (percent-encoded; `/` separators).
fn file_uri(db: &Path, query: &str) -> Option<String> {
    let text = db.to_str()?.replace('\\', "/");
    let mut uri = String::from(if text.starts_with('/') { "file://" } else { "file:///" });
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'.' | b'_' | b'~' | b':') {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri.push('?');
    uri.push_str(query);
    Some(uri)
}

fn read_rows(db: &Path, since: i64) -> rusqlite::Result<Vec<Row>> {
    if let (Some(before), Some(uri)) = (quiescent_signature(db), file_uri(db, "mode=ro&immutable=1")) {
        let connection = Connection::open_with_flags(
            &uri,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let rows = query_rows(&connection, since);
        drop(connection);
        if quiescent_signature(db) == Some(before) {
            return rows;
        }
        // ZCode opened or changed it meanwhile: read it the locked way.
    }
    let connection = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    query_rows(&connection, since)
}

fn query_rows(connection: &Connection, since: i64) -> rusqlite::Result<Vec<Row>> {
    connection.pragma_update(None, "query_only", true)?;
    let mut statement = connection.prepare(
        "SELECT id, session_id, turn_id, logical_request_id, provider_id, model_id, completed_at, \
         input_tokens, output_tokens, reasoning_tokens, cache_creation_input_tokens, cache_read_input_tokens \
         FROM model_usage WHERE status = 'completed' AND completed_at IS NOT NULL AND completed_at >= ?1 \
         ORDER BY completed_at, id",
    )?;
    let rows = statement
        .query_map([since], |row| {
            Ok(Row {
                id: row.get(0)?,
                session_id: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                turn_id: row.get(2)?,
                logical_request_id: row.get(3)?,
                provider_id: row.get(4)?,
                model_id: row.get(5)?,
                completed_at: row.get(6)?,
                input: row.get::<_, Option<i64>>(7)?.unwrap_or(0),
                output: row.get::<_, Option<i64>>(8)?.unwrap_or(0),
                reasoning: row.get::<_, Option<i64>>(9)?.unwrap_or(0),
                cache_creation: row.get::<_, Option<i64>>(10)?.unwrap_or(0),
                cache_read: row.get::<_, Option<i64>>(11)?.unwrap_or(0),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Import new completed rows; returns the number of rows written. Errors
/// (no database, SQLite failure, ledger write) go to `errors`.
pub(super) fn scan(
    root: &Path,
    state: &mut ScanState,
    context: &RowContext<'_>,
    errors: &mut Vec<String>,
    seen: &mut HashSet<String>,
) -> u64 {
    let db = match database(&context.spec.path) {
        Ok(db) => db,
        Err(message) => {
            errors.push(format!("{DB_RELATIVE}: {message}"));
            return 0;
        }
    };
    seen.insert(DB_RELATIVE.to_string());
    let mut cursor = state.zcode.clone().unwrap_or_default();
    let since = if cursor.watermark > 0 { cursor.watermark.saturating_sub(LOOKBACK_MS) } else { i64::MIN };
    let rows = match read_rows(&db, since) {
        Ok(rows) => rows,
        Err(error) => {
            // SQLite messages carry no row contents.
            errors.push(format!("{DB_RELATIVE}: {error}"));
            return 0;
        }
    };
    let mut entries = Vec::new();
    let mut imported = Vec::new();
    for row in &rows {
        if cursor.recent.contains_key(&row.id) {
            continue;
        }
        imported.push((row.id.clone(), row.completed_at));
        let tokens = row.breakdown();
        if tokens.detailed_is_zero() {
            continue;
        }
        let Some(observed) = timestamp(row.completed_at) else { continue };
        let Some(thread) = non_empty(Some(&row.session_id)) else { continue };
        let turn = non_empty(row.turn_id.as_deref())
            .or_else(|| non_empty(row.logical_request_id.as_deref()))
            .unwrap_or_else(|| row.id.clone());
        let attribution = UsageAttribution {
            provider: non_empty(row.provider_id.as_deref()),
            model: non_empty(row.model_id.as_deref()),
            request_model: None,
            reasoning_effort: None,
            service_tier: None,
        };
        entries.extend(context.sample(
            format!("zcode:{}", row.id),
            &observed,
            PROJECT_KEY,
            None,
            &thread,
            &turn,
            attribution,
            tokens,
        ));
    }
    if let Err(error) = store::append_external_entries(root, &context.spec.id, &entries) {
        errors.push(format!("{DB_RELATIVE}: ledger write failed: {error:#}"));
        return 0;
    }
    for (id, completed_at) in imported {
        cursor.watermark = cursor.watermark.max(completed_at);
        cursor.recent.insert(id, completed_at);
    }
    let floor = cursor.watermark.saturating_sub(LOOKBACK_MS);
    cursor.recent.retain(|_, completed_at| *completed_at >= floor);
    state.zcode = Some(cursor);
    // Status only (`files`): the database's current size / mtime.
    let mut file = FileState::default();
    if let Ok(meta) = std::fs::metadata(&db) {
        let (len, mtime_ms, inode) = super::file_signature(&meta);
        file = FileState { len, mtime_ms, inode, offset: len, cursor: Default::default() };
    }
    state.files.insert(DB_RELATIVE.to_string(), file);
    entries.len() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uris_are_absolute_and_percent_encoded() {
        assert_eq!(
            file_uri(Path::new("/home/a b/.zcode/cli/db/db.sqlite"), "mode=ro&immutable=1").unwrap(),
            "file:///home/a%20b/.zcode/cli/db/db.sqlite?mode=ro&immutable=1"
        );
        assert_eq!(file_uri(Path::new("/x/#?%.sqlite"), "q").unwrap(), "file:///x/%23%3F%25.sqlite?q");
        #[cfg(windows)]
        assert_eq!(file_uri(Path::new(r"C:\Users\a\db.sqlite"), "q").unwrap(), "file:///C:/Users/a/db.sqlite?q");
    }
}
