//! Rebuildable per-source/file cursors; JSONL entry IDs remain billing authority.
use super::{FileState, ScanState, SourceSpec, STATE_VERSION};
use crate::store::{metrics, OpCounts, StoreDb};
use rusqlite::{params, OptionalExtension};
use std::{collections::BTreeMap, path::Path, sync::Arc};

const ID: &str = "usage-source-checkpoints";
const FILE: &str = "usage-collector-checkpoints.sqlite3";
const EPOCH_FILE: &str = "collector-checkpoint-epoch";
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS collector_header (
    source TEXT PRIMARY KEY, state BLOB NOT NULL, files INTEGER NOT NULL CHECK(files>=0), epoch TEXT NOT NULL) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS collector_files (
    source TEXT NOT NULL,path TEXT NOT NULL,state BLOB NOT NULL,pending INTEGER NOT NULL,
    PRIMARY KEY(source,path)) WITHOUT ROWID;
    CREATE INDEX IF NOT EXISTS collector_pending ON collector_files(source,path) WHERE pending=1;";

pub(super) struct Checkpoints {
    db: Arc<StoreDb>,
    source: String,
    pub needs_discovery: bool,
}
impl Checkpoints {
    pub fn open(root: &Path, spec: &SourceSpec) -> anyhow::Result<Self> {
        std::fs::create_dir_all(root)?;
        super::store::secure_directory(root)?;
        let db = StoreDb::open_at(&crate::store::domain::USAGE_COLLECTORS, &root.join(FILE))?;
        super::store::secure_file(&root.join(FILE))?;
        let needs_discovery = db.write(|tx| {
            tx.sql().execute_batch(SCHEMA)?;
            // An older build purges the source directory without knowing this
            // derived database. Tie its checkpoints to an epoch inside that
            // directory so re-adding the source cannot skip purged samples.
            let dir = super::source_dir(root, &spec.id);
            let epoch = std::fs::read_to_string(dir.join(EPOCH_FILE)).ok();
            let stored_epoch: Option<String> = tx
                .sql()
                .query_row(
                    "SELECT epoch FROM collector_header WHERE source=?1",
                    [&spec.id],
                    |r| r.get(0),
                )
                .optional()?;
            if epoch.is_some()
                && epoch == stored_epoch
                && header(tx.sql(), &spec.id)?.is_some_and(|(state, _)| {
                    state.path == spec.path.to_string_lossy() && state.version == STATE_VERSION
                })
            {
                return Ok(false);
            }
            let mut legacy = super::load_state(root, &spec.id);
            if legacy.path != spec.path.to_string_lossy() {
                legacy = ScanState {
                    version: STATE_VERSION,
                    path: spec.path.to_string_lossy().into_owned(),
                    ..Default::default()
                };
            }
            clear(tx.sql(), &spec.id)?;
            let count = legacy.files.len();
            for (path, state) in std::mem::take(&mut legacy.files) {
                put(tx.sql(), &spec.id, &path, &state)?;
            }
            std::fs::create_dir_all(&dir)?;
            super::store::secure_directory(&root.join(super::store::USAGE_EXTERNAL_DIR))?;
            super::store::secure_directory(&dir)?;
            let epoch = uuid::Uuid::new_v4().to_string();
            // If the process dies before the SQL commit, the epoch mismatch
            // forces another idempotent rebuild rather than trusting old rows.
            crate::config::atomic::write_private_bytes_atomic(
                &dir.join(EPOCH_FILE),
                epoch.as_bytes(),
            )?;
            tx.sql().execute(
                "INSERT INTO collector_header(source,state,files,epoch) VALUES (?1,?2,?3,?4)",
                params![spec.id, encode(&legacy)?, count as i64, epoch],
            )?;
            Ok(true)
        })?;
        Ok(Self {
            db,
            source: spec.id.clone(),
            needs_discovery,
        })
    }
    pub fn header(&self) -> anyhow::Result<(ScanState, usize)> {
        self.db.read(|tx| {
            header(tx.sql(), &self.source)?
                .ok_or_else(|| anyhow::anyhow!("collector header missing"))
        })
    }
    pub fn get(&self, path: &str) -> anyhow::Result<Option<FileState>> {
        self.db.read(|tx| {
            let bytes: Option<Vec<u8>> = tx
                .sql()
                .query_row(
                    "SELECT state FROM collector_files WHERE source=?1 AND path=?2",
                    params![self.source, path],
                    |r| r.get(0),
                )
                .optional()?;
            metrics::note(
                ID,
                OpCounts {
                    rows_visited: u64::from(bytes.is_some()),
                    sql_statements: 1,
                    ..Default::default()
                },
            );
            bytes.as_deref().map(decode).transpose()
        })
    }
    pub fn pending(&self) -> anyhow::Result<Vec<String>> {
        self.db.read(|tx| {
            let mut stmt = tx.sql().prepare(
                "SELECT path FROM collector_files WHERE source=?1 AND pending=1 ORDER BY path",
            )?;
            let paths = stmt
                .query_map([&self.source], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            metrics::note(
                ID,
                OpCounts {
                    rows_visited: paths.len() as u64,
                    sql_statements: 1,
                    ..Default::default()
                },
            );
            Ok(paths)
        })
    }
    pub fn paths(&self) -> anyhow::Result<Vec<String>> {
        self.db.read(|tx| {
            let mut stmt = tx
                .sql()
                .prepare("SELECT path FROM collector_files WHERE source=?1 ORDER BY path")?;
            let paths = stmt
                .query_map([&self.source], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            metrics::note(
                ID,
                OpCounts {
                    rows_visited: paths.len() as u64,
                    sql_statements: 1,
                    ..Default::default()
                },
            );
            Ok(paths)
        })
    }
    pub fn commit(
        &self,
        state: &ScanState,
        changes: &BTreeMap<String, Option<FileState>>,
    ) -> anyhow::Result<usize> {
        self.db.write(|tx| {
            let (_, mut count) = header(tx.sql(), &self.source)?
                .ok_or_else(|| anyhow::anyhow!("collector header missing"))?;
            for (path, state) in changes {
                let exists: bool = tx.sql().query_row(
                    "SELECT EXISTS(SELECT 1 FROM collector_files WHERE source=?1 AND path=?2)",
                    params![self.source, path],
                    |r| r.get(0),
                )?;
                match state {
                    Some(state) => {
                        put(tx.sql(), &self.source, path, state)?;
                        if !exists {
                            count += 1;
                        }
                    }
                    None => {
                        tx.sql().execute(
                            "DELETE FROM collector_files WHERE source=?1 AND path=?2",
                            params![self.source, path],
                        )?;
                        if exists {
                            count -= 1;
                        }
                    }
                }
            }
            tx.sql().execute(
                "UPDATE collector_header SET state=?1,files=?2 WHERE source=?3",
                params![encode(state)?, count as i64, self.source],
            )?;
            Ok(count)
        })
    }
}
fn header(db: &rusqlite::Connection, source: &str) -> anyhow::Result<Option<(ScanState, usize)>> {
    let value: Option<(Vec<u8>, i64)> = db
        .query_row(
            "SELECT state,files FROM collector_header WHERE source=?1",
            [source],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    value
        .map(|(bytes, count)| Ok((decode(&bytes)?, usize::try_from(count)?)))
        .transpose()
}
fn put(
    db: &rusqlite::Connection,
    source: &str,
    path: &str,
    state: &FileState,
) -> anyhow::Result<()> {
    db.execute("INSERT INTO collector_files(source,path,state,pending) VALUES (?1,?2,?3,?4) ON CONFLICT(source,path) DO UPDATE SET state=excluded.state,pending=excluded.pending",params![source,path,encode(state)?,i64::from(state.offset<state.len)])?;
    Ok(())
}
fn clear(db: &rusqlite::Connection, source: &str) -> anyhow::Result<()> {
    db.execute("DELETE FROM collector_files WHERE source=?1", [source])?;
    db.execute("DELETE FROM collector_header WHERE source=?1", [source])?;
    Ok(())
}
fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> anyhow::Result<T> {
    metrics::note(
        ID,
        OpCounts {
            bytes_read: bytes.len() as u64,
            bytes_decoded: bytes.len() as u64,
            ..Default::default()
        },
    );
    Ok(serde_json::from_slice(bytes)?)
}
fn encode<T: serde::Serialize>(state: &T) -> anyhow::Result<Vec<u8>> {
    let bytes = serde_json::to_vec(state)?;
    metrics::note(
        ID,
        OpCounts {
            bytes_encoded: bytes.len() as u64,
            bytes_written: bytes.len() as u64,
            ..Default::default()
        },
    );
    Ok(bytes)
}
pub(super) fn status(root: &Path, id: &str) -> anyhow::Result<Option<(ScanState, usize)>> {
    let path = root.join(FILE);
    if !path.exists() {
        return Ok(None);
    }
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    header(&db, id)
}
pub(super) fn forget(root: &Path, id: &str) -> anyhow::Result<()> {
    if !root.join(FILE).exists() {
        return Ok(());
    }
    let db = StoreDb::open_at(&crate::store::domain::USAGE_COLLECTORS, &root.join(FILE))?;
    db.write(|tx| {
        tx.sql().execute_batch(SCHEMA)?;
        clear(tx.sql(), id)
    })
}
