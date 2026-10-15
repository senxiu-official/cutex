//! The artifact store: `<cutex home>/runtime/artifacts/` (SQLite WAL +
//! content-addressed blobs), shared by the Agent Bus and management
//! processes (busy timeout, immediate write transactions).

use std::path::{Path, PathBuf};

use anyhow::Context;
use base64::Engine;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::Value;

use super::{ArtifactSummary, ArtifactVersion, GitInfo, Producer, Scope, Which};

/// Open turn windows count as ending at most this long after their start.
pub const OPEN_WINDOW_CAP_MS: i64 = 24 * 60 * 60 * 1000;
/// Idempotency records are kept this long.
const ACTION_RETENTION_DAYS: i64 = 30;

#[derive(Debug)]
pub enum StoreError {
    NotFound,
    RevisionConflict { revision: u64 },
    NameConflict,
    QuotaFull { used: u64, quota: u64 },
    Invalid(String),
    Store(anyhow::Error),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str("artifact_not_found"),
            Self::RevisionConflict { revision } => write!(f, "artifact_revision_conflict: current revision {revision}"),
            Self::NameConflict => f.write_str("artifact_name_conflict: another artifact in this scope has that name"),
            Self::QuotaFull { used, quota } => write!(f, "artifact_quota_exceeded: {used} of {quota} bytes used"),
            Self::Invalid(message) => write!(f, "invalid_request: {message}"),
            Self::Store(error) => write!(f, "artifacts_unavailable: {error:#}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<anyhow::Error> for StoreError {
    fn from(error: anyhow::Error) -> Self {
        Self::Store(error)
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error.into())
    }
}

impl From<std::io::Error> for StoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Store(error.into())
    }
}

/// What a version stores.
#[derive(Clone, Debug)]
pub enum Content {
    /// Copied into the store.
    Blob { bytes: Vec<u8>, sha256: String },
    /// Registered by path (over `maxBlobBytes`); `sha256` only when hashed.
    External { sha256: Option<String>, bytes: u64 },
}

impl Content {
    pub fn sha256(&self) -> Option<&str> {
        match self {
            Self::Blob { sha256, .. } => Some(sha256),
            Self::External { sha256, .. } => sha256.as_deref(),
        }
    }

    pub fn len(&self) -> u64 {
        match self {
            Self::Blob { bytes, .. } => bytes.len() as u64,
            Self::External { bytes, .. } => *bytes,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Clone, Debug)]
pub struct NewVersion {
    pub scope: Scope,
    pub name: String,
    /// An explicit kind (also changes the artifact's kind).
    pub kind: Option<String>,
    /// The kind of a new artifact without an explicit one.
    pub default_kind: String,
    pub content: Content,
    pub media_type: String,
    pub source_path: String,
    pub source_mtime_ms: Option<i64>,
    pub note: Option<String>,
    pub producer: Producer,
    pub git: Option<GitInfo>,
}

#[derive(Clone, Debug)]
pub struct Outcome {
    pub artifact: ArtifactSummary,
    pub version: ArtifactVersion,
    pub new_version: bool,
    /// Metadata changes made by the publish (`kind`, `unarchived`).
    pub changes: Vec<&'static str>,
}

#[derive(Clone, Debug, Default)]
pub struct Patch {
    pub name: Option<String>,
    pub kind: Option<String>,
    pub starred: Option<bool>,
    pub archived: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Archived {
    #[default]
    No,
    Yes,
    All,
}

#[derive(Clone, Debug, Default)]
pub struct ListFilter {
    /// `None` = every scope.
    pub scopes: Option<Vec<Scope>>,
    pub q: Option<String>,
    pub kind: Option<String>,
    /// Artifacts with a version produced by (or attributed to) this session.
    pub producer: Option<String>,
    pub archived: Archived,
    pub limit: usize,
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub bytes: u64,
    pub blobs: u64,
    pub artifacts: u64,
    pub versions: u64,
}

/// One member turn (auto capture attribution).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnWindow {
    pub session_id: String,
    pub turn_id: Option<String>,
    pub project_id: Option<String>,
    pub display_name: Option<String>,
    pub ask_id: Option<String>,
    pub runtime_generation: Option<u64>,
    pub profile: Option<String>,
    pub model: Option<String>,
    pub started_at_ms: i64,
    pub ended_at_ms: Option<i64>,
}

impl TurnWindow {
    /// End used for overlap checks (open windows: now, capped).
    pub fn effective_end(&self, now_ms: i64) -> i64 {
        self.ended_at_ms.unwrap_or_else(|| now_ms.min(self.started_at_ms + OPEN_WINDOW_CAP_MS))
    }
}

/// Last scanned state of one captured file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureFile {
    pub bytes: u64,
    pub mtime_ms: i64,
    pub sha256: Option<String>,
}

pub struct ArtifactStore {
    dir: PathBuf,
}

pub fn encode_cursor(updated_at: &str, artifact_id: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("{updated_at}\n{artifact_id}"))
}

pub fn decode_cursor(cursor: &str) -> Option<(String, String)> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(cursor.trim()).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let (updated_at, artifact_id) = text.split_once('\n')?;
    super::is_artifact_id(artifact_id).then(|| (updated_at.to_string(), artifact_id.to_string()))
}

const ARTIFACT_COLUMNS: &str = "a.artifact_id, a.scope_kind, a.scope_id, a.name, a.kind, a.latest_version, a.revision, a.starred, \
     a.archived_at, a.created_at, a.updated_at";
const VERSION_COLUMNS: &str =
    "v.artifact_id, v.version, v.storage, v.sha256, v.bytes, v.media_type, v.source_path, v.source_mtime_ms, v.note, v.producer, v.git, v.created_at";

fn version_at(row: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<ArtifactVersion> {
    let artifact_id: String = row.get(at)?;
    let version = row.get::<_, i64>(at + 1)? as u64;
    let producer: String = row.get(at + 9)?;
    let git: Option<String> = row.get(at + 10)?;
    let reference = super::version_ref(&artifact_id, version);
    Ok(ArtifactVersion {
        link: super::link(&reference),
        reference,
        artifact_id,
        version,
        storage: row.get(at + 2)?,
        sha256: row.get(at + 3)?,
        bytes: row.get::<_, i64>(at + 4)? as u64,
        media_type: row.get(at + 5)?,
        source_path: row.get(at + 6)?,
        source_mtime_ms: row.get(at + 7)?,
        note: row.get(at + 8)?,
        producer: serde_json::from_str(&producer).unwrap_or_default(),
        git: git.and_then(|git| serde_json::from_str(&git).ok()),
        created_at: row.get(at + 11)?,
    })
}

fn summary_of(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactSummary> {
    Ok(ArtifactSummary {
        artifact_id: row.get(0)?,
        scope: Scope { kind: row.get(1)?, id: row.get(2)? },
        name: row.get(3)?,
        kind: row.get(4)?,
        latest_version: row.get::<_, i64>(5)? as u64,
        revision: row.get::<_, i64>(6)? as u64,
        starred: row.get::<_, i64>(7)? != 0,
        archived_at: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
        latest: version_at(row, 11)?,
    })
}

fn window_of(row: &rusqlite::Row<'_>) -> rusqlite::Result<TurnWindow> {
    let turn: String = row.get(1)?;
    Ok(TurnWindow {
        session_id: row.get(0)?,
        turn_id: (!turn.is_empty()).then_some(turn),
        project_id: row.get(2)?,
        display_name: row.get(3)?,
        ask_id: row.get(4)?,
        runtime_generation: row.get::<_, Option<i64>>(5)?.map(|value| value as u64),
        profile: row.get(6)?,
        model: row.get(7)?,
        started_at_ms: row.get(8)?,
        ended_at_ms: row.get(9)?,
    })
}

const WINDOW_COLUMNS: &str =
    "session_id, turn_id, project_id, display_name, ask_id, runtime_generation, profile, model, started_at_ms, ended_at_ms";

fn usage_of(connection: &Connection) -> rusqlite::Result<Usage> {
    let (bytes, blobs): (i64, i64) = connection.query_row(
        "SELECT COALESCE(SUM(bytes), 0), COUNT(*) FROM
            (SELECT sha256, MAX(bytes) AS bytes FROM artifact_versions WHERE storage = 'blob' GROUP BY sha256)",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let artifacts: i64 = connection.query_row("SELECT COUNT(*) FROM artifacts", [], |row| row.get(0))?;
    let versions: i64 = connection.query_row("SELECT COUNT(*) FROM artifact_versions", [], |row| row.get(0))?;
    Ok(Usage { bytes: bytes as u64, blobs: blobs as u64, artifacts: artifacts as u64, versions: versions as u64 })
}

fn same_content(latest: &ArtifactVersion, new: &NewVersion) -> bool {
    match (latest.sha256.as_deref(), new.content.sha256()) {
        (Some(old), Some(new)) => old == new,
        (None, None) => {
            latest.bytes == new.content.len()
                && latest.source_mtime_ms == new.source_mtime_ms
                && latest.source_path == new.source_path
        }
        _ => false,
    }
}

impl ArtifactStore {
    pub fn open_default() -> anyhow::Result<Self> {
        Ok(Self::at(crate::config::paths::runtime_dir()?.join("artifacts")))
    }

    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn connection(&self) -> anyhow::Result<Connection> {
        crate::management::client_tokens::ensure_private_dir(&self.dir)?;
        let path = self.dir.join("artifacts-v1.sqlite3");
        let connection = Connection::open(&path).with_context(|| format!("open {}", path.display()))?;
        connection.busy_timeout(std::time::Duration::from_secs(10))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS artifacts (
                artifact_id TEXT PRIMARY KEY,
                scope_kind TEXT NOT NULL,
                scope_id TEXT NOT NULL,
                name TEXT NOT NULL,
                kind TEXT NOT NULL,
                latest_version INTEGER NOT NULL,
                revision INTEGER NOT NULL,
                starred INTEGER NOT NULL DEFAULT 0,
                archived_at TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                UNIQUE (scope_kind, scope_id, name)
            );
            CREATE INDEX IF NOT EXISTS artifacts_updated ON artifacts (updated_at, artifact_id);
            CREATE TABLE IF NOT EXISTS artifact_versions (
                artifact_id TEXT NOT NULL,
                version INTEGER NOT NULL,
                storage TEXT NOT NULL,
                sha256 TEXT,
                bytes INTEGER NOT NULL,
                media_type TEXT NOT NULL,
                source_path TEXT NOT NULL,
                source_mtime_ms INTEGER,
                note TEXT,
                producer TEXT NOT NULL,
                producer_kind TEXT NOT NULL,
                producer_session TEXT,
                git TEXT,
                created_at TEXT NOT NULL,
                PRIMARY KEY (artifact_id, version)
            );
            CREATE INDEX IF NOT EXISTS artifact_versions_sha ON artifact_versions (sha256);
            CREATE INDEX IF NOT EXISTS artifact_versions_producer ON artifact_versions (producer_session);
            CREATE TABLE IF NOT EXISTS actions (
                caller TEXT NOT NULL,
                action_id TEXT NOT NULL,
                request_sha256 TEXT NOT NULL,
                result TEXT NOT NULL,
                at TEXT NOT NULL,
                PRIMARY KEY (caller, action_id)
            );
            CREATE INDEX IF NOT EXISTS actions_at ON actions (at);
            CREATE TABLE IF NOT EXISTS turn_windows (
                session_id TEXT NOT NULL,
                turn_id TEXT NOT NULL,
                project_id TEXT,
                display_name TEXT,
                ask_id TEXT,
                runtime_generation INTEGER,
                profile TEXT,
                model TEXT,
                started_at_ms INTEGER NOT NULL,
                ended_at_ms INTEGER,
                PRIMARY KEY (session_id, turn_id, started_at_ms)
            );
            CREATE INDEX IF NOT EXISTS turn_windows_project ON turn_windows (project_id, started_at_ms);
            CREATE TABLE IF NOT EXISTS capture_files (
                project_id TEXT NOT NULL,
                rel_path TEXT NOT NULL,
                bytes INTEGER NOT NULL,
                mtime_ms INTEGER NOT NULL,
                sha256 TEXT,
                PRIMARY KEY (project_id, rel_path)
            );",
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

    pub fn blob_path(&self, sha256: &str) -> PathBuf {
        let prefix = sha256.get(..2).unwrap_or("00");
        self.dir.join("blobs").join(prefix).join(sha256)
    }

    pub fn read_blob(&self, sha256: &str) -> std::io::Result<Vec<u8>> {
        std::fs::read(self.blob_path(sha256))
    }

    /// Publish a version: same scope + name = next version; the same
    /// content as the latest version adds nothing (`new_version: false`).
    /// An explicit kind updates the artifact's kind; publishing into an
    /// archived artifact unarchives it.
    pub fn publish(&self, new: NewVersion, quota_bytes: u64, now: DateTime<Utc>) -> Result<Outcome, StoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let at = crate::asks::now_rfc3339(now);
        let existing = transaction
            .query_row(
                &format!(
                    "SELECT {ARTIFACT_COLUMNS}, {VERSION_COLUMNS} FROM artifacts a
                     JOIN artifact_versions v ON v.artifact_id = a.artifact_id AND v.version = a.latest_version
                     WHERE a.scope_kind = ?1 AND a.scope_id = ?2 AND a.name = ?3"
                ),
                params![new.scope.kind, new.scope.id, new.name],
                summary_of,
            )
            .optional()?;
        let mut changes = Vec::new();
        if let Some(current) = &existing {
            if same_content(&current.latest, &new) {
                let mut revision = current.revision;
                if new.kind.as_deref().is_some_and(|kind| kind != current.kind) {
                    changes.push("kind");
                }
                if current.archived_at.is_some() {
                    changes.push("unarchived");
                }
                if !changes.is_empty() {
                    revision += 1;
                    transaction.execute(
                        "UPDATE artifacts SET kind = ?2, archived_at = NULL, revision = ?3, updated_at = ?4 WHERE artifact_id = ?1",
                        params![current.artifact_id, new.kind.as_deref().unwrap_or(&current.kind), revision as i64, at],
                    )?;
                }
                transaction.commit()?;
                let artifact = self.get(&current.artifact_id)?.ok_or(StoreError::NotFound)?;
                let version = artifact.latest.clone();
                return Ok(Outcome { artifact, version, new_version: false, changes });
            }
        }
        // Space for a new blob.
        let mut wrote_blob = None;
        if let Content::Blob { bytes, sha256 } = &new.content {
            let known: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM artifact_versions WHERE storage = 'blob' AND sha256 = ?1)",
                [sha256],
                |row| row.get(0),
            )?;
            if !known {
                let used = usage_of(&transaction)?.bytes;
                if used.saturating_add(bytes.len() as u64) > quota_bytes {
                    return Err(StoreError::QuotaFull { used, quota: quota_bytes });
                }
            }
            let path = self.blob_path(sha256);
            if !path.exists() {
                crate::management::client_tokens::ensure_private_dir(path.parent().context("blob dir")?)?;
                crate::config::atomic::write_private_bytes_atomic(&path, bytes)?;
                wrote_blob = Some(path);
            }
        }
        let (storage, sha256) = match &new.content {
            Content::Blob { sha256, .. } => ("blob", Some(sha256.clone())),
            Content::External { sha256, .. } => ("external", sha256.clone()),
        };
        let result = (|| -> Result<String, StoreError> {
            let (artifact_id, version) = match &existing {
                Some(current) => {
                    let version = current.latest_version + 1;
                    let mut revision = current.revision;
                    if new.kind.as_deref().is_some_and(|kind| kind != current.kind) {
                        changes.push("kind");
                    }
                    if current.archived_at.is_some() {
                        changes.push("unarchived");
                    }
                    if !changes.is_empty() {
                        revision += 1;
                    }
                    transaction.execute(
                        "UPDATE artifacts SET latest_version = ?2, kind = ?3, archived_at = NULL, revision = ?4, updated_at = ?5
                         WHERE artifact_id = ?1",
                        params![
                            current.artifact_id,
                            version as i64,
                            new.kind.as_deref().unwrap_or(&current.kind),
                            revision as i64,
                            at
                        ],
                    )?;
                    (current.artifact_id.clone(), version)
                }
                None => {
                    let artifact_id = super::new_artifact_id();
                    let kind = new.kind.clone().unwrap_or_else(|| new.default_kind.clone());
                    transaction.execute(
                        "INSERT INTO artifacts (artifact_id, scope_kind, scope_id, name, kind, latest_version, revision, starred,
                            archived_at, created_at, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, 1, 1, 0, NULL, ?6, ?6)",
                        params![artifact_id, new.scope.kind, new.scope.id, new.name, kind, at],
                    )?;
                    (artifact_id, 1)
                }
            };
            transaction.execute(
                "INSERT INTO artifact_versions (artifact_id, version, storage, sha256, bytes, media_type, source_path, source_mtime_ms,
                    note, producer, producer_kind, producer_session, git, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    artifact_id,
                    version as i64,
                    storage,
                    sha256,
                    new.content.len() as i64,
                    new.media_type,
                    new.source_path,
                    new.source_mtime_ms,
                    new.note,
                    serde_json::to_string(&new.producer).map_err(anyhow::Error::from)?,
                    new.producer.kind,
                    new.producer.session_id,
                    new.git.as_ref().map(serde_json::to_string).transpose().map_err(anyhow::Error::from)?,
                    at,
                ],
            )?;
            Ok(artifact_id)
        })();
        let artifact_id = match result.and_then(|id| transaction.commit().map(|()| id).map_err(StoreError::from)) {
            Ok(id) => id,
            Err(error) => {
                if let Some(path) = wrote_blob {
                    let _ = std::fs::remove_file(path);
                }
                return Err(error);
            }
        };
        let artifact = self.get(&artifact_id)?.ok_or(StoreError::NotFound)?;
        let version = artifact.latest.clone();
        Ok(Outcome { artifact, version, new_version: true, changes })
    }

    pub fn get(&self, artifact_id: &str) -> anyhow::Result<Option<ArtifactSummary>> {
        if !super::is_artifact_id(artifact_id) {
            return Ok(None);
        }
        let connection = self.connection()?;
        Ok(connection
            .query_row(
                &format!(
                    "SELECT {ARTIFACT_COLUMNS}, {VERSION_COLUMNS} FROM artifacts a
                     JOIN artifact_versions v ON v.artifact_id = a.artifact_id AND v.version = a.latest_version
                     WHERE a.artifact_id = ?1"
                ),
                [artifact_id],
                summary_of,
            )
            .optional()?)
    }

    /// All versions, newest first.
    pub fn versions(&self, artifact_id: &str) -> anyhow::Result<Vec<ArtifactVersion>> {
        let connection = self.connection()?;
        let mut statement =
            connection.prepare(&format!("SELECT {VERSION_COLUMNS} FROM artifact_versions v WHERE v.artifact_id = ?1 ORDER BY v.version DESC"))?;
        let rows = statement.query_map([artifact_id], |row| version_at(row, 0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// The artifact and one of its versions.
    pub fn version(&self, artifact_id: &str, which: Which) -> anyhow::Result<Option<(ArtifactSummary, ArtifactVersion)>> {
        let Some(artifact) = self.get(artifact_id)? else {
            return Ok(None);
        };
        let number = match which {
            Which::Latest => return Ok(Some((artifact.clone(), artifact.latest))),
            Which::Version(number) => number,
        };
        let connection = self.connection()?;
        let version = connection
            .query_row(
                &format!("SELECT {VERSION_COLUMNS} FROM artifact_versions v WHERE v.artifact_id = ?1 AND v.version = ?2"),
                params![artifact_id, number as i64],
                |row| version_at(row, 0),
            )
            .optional()?;
        Ok(version.map(|version| (artifact, version)))
    }

    /// Newest `updatedAt` first; `(items, nextCursor)`.
    pub fn list(&self, filter: &ListFilter) -> Result<(Vec<ArtifactSummary>, Option<String>), StoreError> {
        let mut clauses: Vec<String> = Vec::new();
        let mut values: Vec<rusqlite::types::Value> = Vec::new();
        let bind = |value: rusqlite::types::Value, values: &mut Vec<rusqlite::types::Value>| {
            values.push(value);
            format!("?{}", values.len())
        };
        if let Some(scopes) = &filter.scopes {
            if scopes.is_empty() {
                return Ok((Vec::new(), None));
            }
            let mut any = Vec::new();
            for scope in scopes {
                let kind = bind(scope.kind.clone().into(), &mut values);
                let id = bind(scope.id.clone().into(), &mut values);
                any.push(format!("(a.scope_kind = {kind} AND a.scope_id = {id})"));
            }
            clauses.push(format!("({})", any.join(" OR ")));
        }
        if let Some(q) = filter.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
            let escaped = q.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
            let pattern = bind(format!("%{escaped}%").into(), &mut values);
            clauses.push(format!("(a.name LIKE {pattern} ESCAPE '\\' OR v.note LIKE {pattern} ESCAPE '\\')"));
        }
        if let Some(kind) = &filter.kind {
            let kind = bind(kind.clone().into(), &mut values);
            clauses.push(format!("a.kind = {kind}"));
        }
        if let Some(producer) = &filter.producer {
            let producer = bind(producer.clone().into(), &mut values);
            clauses.push(format!(
                "EXISTS (SELECT 1 FROM artifact_versions p WHERE p.artifact_id = a.artifact_id AND p.producer_session = {producer})"
            ));
        }
        match filter.archived {
            Archived::No => clauses.push("a.archived_at IS NULL".into()),
            Archived::Yes => clauses.push("a.archived_at IS NOT NULL".into()),
            Archived::All => {}
        }
        if let Some(cursor) = &filter.cursor {
            let (updated_at, artifact_id) = decode_cursor(cursor).ok_or_else(|| StoreError::Invalid("cursor: not a cursor of this list".into()))?;
            let updated = bind(updated_at.into(), &mut values);
            let id = bind(artifact_id.into(), &mut values);
            clauses.push(format!("(a.updated_at < {updated} OR (a.updated_at = {updated} AND a.artifact_id < {id}))"));
        }
        let limit = filter.limit.clamp(1, 200);
        let limit_param = bind(((limit + 1) as i64).into(), &mut values);
        let where_clause = if clauses.is_empty() { String::new() } else { format!("WHERE {}", clauses.join(" AND ")) };
        let sql = format!(
            "SELECT {ARTIFACT_COLUMNS}, {VERSION_COLUMNS} FROM artifacts a
             JOIN artifact_versions v ON v.artifact_id = a.artifact_id AND v.version = a.latest_version
             {where_clause} ORDER BY a.updated_at DESC, a.artifact_id DESC LIMIT {limit_param}"
        );
        let connection = self.connection()?;
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map(rusqlite::params_from_iter(values.iter()), summary_of)?;
        let mut items = rows.collect::<Result<Vec<_>, _>>()?;
        let next = if items.len() > limit {
            items.truncate(limit);
            items.last().map(|last| encode_cursor(&last.updated_at, &last.artifact_id))
        } else {
            None
        };
        Ok((items, next))
    }

    /// Change metadata under compare-and-swap. Returns the artifact and the
    /// changes made (one revision step for all of them).
    pub fn patch(
        &self,
        artifact_id: &str,
        expected_revision: u64,
        patch: Patch,
        now: DateTime<Utc>,
    ) -> Result<(ArtifactSummary, Vec<&'static str>), StoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = transaction
            .query_row(
                &format!(
                    "SELECT {ARTIFACT_COLUMNS}, {VERSION_COLUMNS} FROM artifacts a
                     JOIN artifact_versions v ON v.artifact_id = a.artifact_id AND v.version = a.latest_version
                     WHERE a.artifact_id = ?1"
                ),
                [artifact_id],
                summary_of,
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;
        if current.revision != expected_revision {
            return Err(StoreError::RevisionConflict { revision: current.revision });
        }
        let mut changes = Vec::new();
        let mut next = current.clone();
        if let Some(name) = patch.name {
            let name = super::clean_name(&name).ok_or_else(|| StoreError::Invalid(format!("name: 1-{} characters", super::MAX_NAME_CHARS)))?;
            if name != current.name {
                let taken: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM artifacts WHERE scope_kind = ?1 AND scope_id = ?2 AND name = ?3)",
                    params![current.scope.kind, current.scope.id, name],
                    |row| row.get(0),
                )?;
                if taken {
                    return Err(StoreError::NameConflict);
                }
                next.name = name;
                changes.push("renamed");
            }
        }
        if let Some(kind) = patch.kind {
            if !super::is_kind(&kind) {
                return Err(StoreError::Invalid(format!("kind: one of {}", super::KINDS.join(", "))));
            }
            if kind != current.kind {
                next.kind = kind;
                changes.push("kind");
            }
        }
        if let Some(starred) = patch.starred {
            if starred != current.starred {
                next.starred = starred;
                changes.push("starred");
            }
        }
        let mut touched = changes.iter().any(|change| *change != "starred");
        match patch.archived {
            Some(true) if current.archived_at.is_none() => {
                next.archived_at = Some(crate::asks::now_rfc3339(now));
                changes.push("archived");
                touched = true;
            }
            Some(false) if current.archived_at.is_some() => {
                next.archived_at = None;
                changes.push("unarchived");
                touched = true;
            }
            _ => {}
        }
        if changes.is_empty() {
            return Ok((current, changes));
        }
        next.revision += 1;
        if touched {
            next.updated_at = crate::asks::now_rfc3339(now);
        }
        transaction.execute(
            "UPDATE artifacts SET name = ?2, kind = ?3, starred = ?4, archived_at = ?5, revision = ?6, updated_at = ?7 WHERE artifact_id = ?1",
            params![
                artifact_id,
                next.name,
                next.kind,
                i64::from(next.starred),
                next.archived_at,
                next.revision as i64,
                next.updated_at
            ],
        )?;
        transaction.commit()?;
        Ok((next, changes))
    }

    /// Delete an artifact with all versions; blobs nothing else uses are
    /// removed. Returns the deleted artifact.
    pub fn delete(&self, artifact_id: &str) -> Result<ArtifactSummary, StoreError> {
        let current = self.get(artifact_id)?.ok_or(StoreError::NotFound)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let shas: Vec<String> = {
            let mut statement = transaction
                .prepare("SELECT DISTINCT sha256 FROM artifact_versions WHERE artifact_id = ?1 AND storage = 'blob' AND sha256 IS NOT NULL")?;
            let rows = statement.query_map([artifact_id], |row| row.get(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        transaction.execute("DELETE FROM artifact_versions WHERE artifact_id = ?1", [artifact_id])?;
        transaction.execute("DELETE FROM artifacts WHERE artifact_id = ?1", [artifact_id])?;
        let mut orphaned = Vec::new();
        for sha in shas {
            let used: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM artifact_versions WHERE storage = 'blob' AND sha256 = ?1)",
                [&sha],
                |row| row.get(0),
            )?;
            if !used {
                orphaned.push(sha);
            }
        }
        transaction.commit()?;
        for sha in orphaned {
            let _ = std::fs::remove_file(self.blob_path(&sha));
        }
        Ok(current)
    }

    pub fn usage(&self) -> anyhow::Result<Usage> {
        Ok(usage_of(&self.connection()?)?)
    }

    /// Whether a version not produced by auto capture has this content in
    /// the scope ("explicit publish wins").
    pub fn explicit_has_sha(&self, scope: &Scope, sha256: &str) -> anyhow::Result<bool> {
        let connection = self.connection()?;
        Ok(connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM artifact_versions v JOIN artifacts a ON a.artifact_id = v.artifact_id
              WHERE a.scope_kind = ?1 AND a.scope_id = ?2 AND v.sha256 = ?3 AND v.producer_kind != 'auto')",
            params![scope.kind, scope.id, sha256],
            |row| row.get(0),
        )?)
    }

    // --- idempotency ---------------------------------------------------

    /// A remembered result for `(caller, action_id)`; `Err(())` when the id
    /// was used for a different request.
    pub fn replay(&self, caller: &str, action_id: &str, request_sha256: &str) -> anyhow::Result<Result<Option<Value>, ()>> {
        let connection = self.connection()?;
        let found: Option<(String, String)> = connection
            .query_row(
                "SELECT request_sha256, result FROM actions WHERE caller = ?1 AND action_id = ?2",
                params![caller, action_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(match found {
            None => Ok(None),
            Some((digest, _)) if digest != request_sha256 => Err(()),
            Some((_, result)) => Ok(serde_json::from_str(&result).ok()),
        })
    }

    pub fn remember(&self, caller: &str, action_id: &str, request_sha256: &str, result: &Value, now: DateTime<Utc>) -> anyhow::Result<()> {
        let connection = self.connection()?;
        connection.execute(
            "INSERT OR REPLACE INTO actions (caller, action_id, request_sha256, result, at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![caller, action_id, request_sha256, result.to_string(), crate::asks::now_rfc3339(now)],
        )?;
        let cutoff = crate::asks::now_rfc3339(now - chrono::Duration::days(ACTION_RETENTION_DAYS));
        connection.execute("DELETE FROM actions WHERE at < ?1", [cutoff])?;
        Ok(())
    }

    // --- meta ----------------------------------------------------------

    pub fn meta(&self, key: &str) -> anyhow::Result<Option<String>> {
        let connection = self.connection()?;
        Ok(connection.query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| row.get(0)).optional()?)
    }

    pub fn set_meta(&self, key: &str, value: Option<&str>) -> anyhow::Result<()> {
        let connection = self.connection()?;
        match value {
            Some(value) => connection.execute("INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)", params![key, value])?,
            None => connection.execute("DELETE FROM meta WHERE key = ?1", [key])?,
        };
        Ok(())
    }

    // --- turn windows ----------------------------------------------------

    /// A member turn started (closes any window the session left open).
    pub fn window_started(&self, window: &TurnWindow) -> anyhow::Result<()> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "UPDATE turn_windows SET ended_at_ms = MAX(started_at_ms, ?2) WHERE session_id = ?1 AND ended_at_ms IS NULL",
            params![window.session_id, window.started_at_ms],
        )?;
        transaction.execute(
            &format!("INSERT OR REPLACE INTO turn_windows ({WINDOW_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL)"),
            params![
                window.session_id,
                window.turn_id.clone().unwrap_or_default(),
                window.project_id,
                window.display_name,
                window.ask_id,
                window.runtime_generation.map(|value| value as i64),
                window.profile,
                window.model,
                window.started_at_ms,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// The session's open turn ended. Returns the closed window's project.
    pub fn window_ended(&self, session_id: &str, ended_at_ms: i64) -> anyhow::Result<Option<String>> {
        let connection = self.connection()?;
        let project: Option<Option<String>> = connection
            .query_row(
                "SELECT project_id FROM turn_windows WHERE session_id = ?1 AND ended_at_ms IS NULL ORDER BY started_at_ms DESC LIMIT 1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        connection.execute(
            "UPDATE turn_windows SET ended_at_ms = MAX(started_at_ms, ?2) WHERE session_id = ?1 AND ended_at_ms IS NULL",
            params![session_id, ended_at_ms],
        )?;
        Ok(project.flatten())
    }

    /// Windows of a project that end at or after `since_ms` (open ones
    /// included).
    pub fn project_windows(&self, project_id: &str, since_ms: i64) -> anyhow::Result<Vec<TurnWindow>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(&format!(
            "SELECT {WINDOW_COLUMNS} FROM turn_windows
             WHERE project_id = ?1 AND (ended_at_ms IS NULL OR ended_at_ms >= ?2) ORDER BY started_at_ms"
        ))?;
        let rows = statement.query_map(params![project_id, since_ms], window_of)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn earliest_window_ms(&self, project_id: &str) -> anyhow::Result<Option<i64>> {
        let connection = self.connection()?;
        Ok(connection.query_row("SELECT MIN(started_at_ms) FROM turn_windows WHERE project_id = ?1", [project_id], |row| row.get(0))?)
    }

    /// Drop windows that ended (or, when open, started) before `before_ms`.
    pub fn prune_windows(&self, before_ms: i64) -> anyhow::Result<usize> {
        let connection = self.connection()?;
        Ok(connection.execute("DELETE FROM turn_windows WHERE COALESCE(ended_at_ms, started_at_ms + ?2) < ?1", params![before_ms, OPEN_WINDOW_CAP_MS])?)
    }

    // --- auto capture file state ------------------------------------------

    pub fn capture_files(&self, project_id: &str) -> anyhow::Result<std::collections::HashMap<String, CaptureFile>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare("SELECT rel_path, bytes, mtime_ms, sha256 FROM capture_files WHERE project_id = ?1")?;
        let rows = statement.query_map([project_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                CaptureFile { bytes: row.get::<_, i64>(1)? as u64, mtime_ms: row.get(2)?, sha256: row.get(3)? },
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Forget every recorded file of a project.
    pub fn clear_capture_files(&self, project_id: &str) -> anyhow::Result<usize> {
        let connection = self.connection()?;
        Ok(connection.execute("DELETE FROM capture_files WHERE project_id = ?1", [project_id])?)
    }

    /// Replace the recorded state of the given files (`None` = forget).
    pub fn set_capture_files(&self, project_id: &str, files: &[(String, Option<CaptureFile>)]) -> anyhow::Result<()> {
        if files.is_empty() {
            return Ok(());
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (path, state) in files {
            match state {
                Some(state) => transaction.execute(
                    "INSERT OR REPLACE INTO capture_files (project_id, rel_path, bytes, mtime_ms, sha256) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![project_id, path, state.bytes as i64, state.mtime_ms, state.sha256],
                )?,
                None => transaction.execute("DELETE FROM capture_files WHERE project_id = ?1 AND rel_path = ?2", params![project_id, path])?,
            };
        }
        transaction.commit()?;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn temp_store(label: &str) -> (ArtifactStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!("cutex-artifacts-{label}-{}", uuid::Uuid::new_v4().simple()));
        (ArtifactStore::at(&dir), dir)
    }

    pub fn agent(session: &str) -> Producer {
        Producer { kind: "agent".into(), session_id: Some(session.into()), display_name: Some("Eva".into()), ..Producer::default() }
    }

    pub fn blob(scope: &Scope, name: &str, text: &str, producer: Producer) -> NewVersion {
        NewVersion {
            scope: scope.clone(),
            name: name.into(),
            kind: None,
            default_kind: "report".into(),
            content: Content::Blob { bytes: text.as_bytes().to_vec(), sha256: crate::asks::sha256_hex(text.as_bytes()) },
            media_type: "text/markdown".into(),
            source_path: format!("D:\\ws\\{name}"),
            source_mtime_ms: Some(1),
            note: None,
            producer,
            git: None,
        }
    }

    #[test]
    fn versions_merge_by_scope_and_name_and_same_sha_is_a_no_op() {
        let (store, dir) = temp_store("merge");
        let now = Utc::now();
        let project = Scope::project("proj-a");
        let first = store.publish(blob(&project, "reports/r.md", "one", agent("cutex.a")), u64::MAX, now).unwrap();
        assert!(first.new_version);
        assert_eq!((first.version.version, first.artifact.latest_version, first.artifact.revision), (1, 1, 1));
        assert_eq!(first.version.reference, format!("{}@1", first.artifact.artifact_id));
        let same = store.publish(blob(&project, "reports/r.md", "one", agent("cutex.b")), u64::MAX, now).unwrap();
        assert!(!same.new_version);
        assert_eq!(same.version.version, 1);
        assert_eq!(same.version.producer.session_id.as_deref(), Some("cutex.a"), "the existing version is returned");
        let second = store.publish(blob(&project, "reports/r.md", "two", agent("cutex.b")), u64::MAX, now).unwrap();
        assert!(second.new_version);
        assert_eq!(second.artifact.artifact_id, first.artifact.artifact_id);
        assert_eq!(second.version.version, 2);
        assert_eq!(second.artifact.revision, 1, "a new version is not a metadata change");
        // Other scope or name = other artifact.
        let other_scope = store.publish(blob(&Scope::session("cutex.a"), "reports/r.md", "two", agent("cutex.a")), u64::MAX, now).unwrap();
        assert_ne!(other_scope.artifact.artifact_id, first.artifact.artifact_id);
        let other_name = store.publish(blob(&project, "reports/s.md", "two", agent("cutex.a")), u64::MAX, now).unwrap();
        assert_ne!(other_name.artifact.artifact_id, first.artifact.artifact_id);
        // Same bytes share a blob.
        assert_eq!(store.usage().unwrap(), Usage { bytes: 6, blobs: 2, artifacts: 3, versions: 4 });
        let versions = store.versions(&first.artifact.artifact_id).unwrap();
        assert_eq!(versions.iter().map(|v| v.version).collect::<Vec<_>>(), vec![2, 1]);
        let (_, v1) = store.version(&first.artifact.artifact_id, Which::Version(1)).unwrap().unwrap();
        assert_eq!(store.read_blob(v1.sha256.as_deref().unwrap()).unwrap(), b"one");
        assert!(store.version(&first.artifact.artifact_id, Which::Version(9)).unwrap().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn explicit_kind_unarchive_patch_and_delete() {
        let (store, dir) = temp_store("patch");
        let now = Utc::now();
        let project = Scope::project("proj-a");
        let first = store.publish(blob(&project, "a.md", "one", agent("cutex.a")), u64::MAX, now).unwrap();
        let id = first.artifact.artifact_id.clone();
        let (archived, changes) = store.patch(&id, 1, Patch { archived: Some(true), starred: Some(true), ..Patch::default() }, now).unwrap();
        assert_eq!(changes, vec!["starred", "archived"]);
        assert_eq!(archived.revision, 2);
        assert!(archived.archived_at.is_some());
        assert!(matches!(store.patch(&id, 1, Patch::default(), now), Err(StoreError::RevisionConflict { revision: 2 })));
        // Same content with an explicit kind: no version, kind changes, unarchived.
        let mut again = blob(&project, "a.md", "one", agent("cutex.a"));
        again.kind = Some("plan".into());
        let again = store.publish(again, u64::MAX, now).unwrap();
        assert!(!again.new_version);
        assert_eq!(again.changes, vec!["kind", "unarchived"]);
        assert_eq!((again.artifact.kind.as_str(), again.artifact.revision), ("plan", 3));
        assert!(again.artifact.archived_at.is_none());
        // Rename conflicts within the scope.
        store.publish(blob(&project, "b.md", "two", agent("cutex.a")), u64::MAX, now).unwrap();
        assert!(matches!(store.patch(&id, 3, Patch { name: Some("b.md".into()), ..Patch::default() }, now), Err(StoreError::NameConflict)));
        assert!(matches!(store.patch(&id, 3, Patch { kind: Some("poem".into()), ..Patch::default() }, now), Err(StoreError::Invalid(_))));
        let (renamed, changes) = store.patch(&id, 3, Patch { name: Some("c.md".into()), ..Patch::default() }, now).unwrap();
        assert_eq!((changes, renamed.name.as_str(), renamed.revision), (vec!["renamed"], "c.md", 4));
        let (same, changes) = store.patch(&id, 4, Patch { name: Some("c.md".into()), ..Patch::default() }, now).unwrap();
        assert!(changes.is_empty());
        assert_eq!(same.revision, 4);
        // Delete frees the blob nobody else uses.
        let sha = first.version.sha256.clone().unwrap();
        assert!(store.blob_path(&sha).exists());
        store.delete(&id).unwrap();
        assert!(!store.blob_path(&sha).exists());
        assert!(store.get(&id).unwrap().is_none());
        assert!(matches!(store.delete(&id), Err(StoreError::NotFound)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn quota_external_and_list_filters() {
        let (store, dir) = temp_store("list");
        let now = Utc::now();
        let project = Scope::project("proj-a");
        store.publish(blob(&project, "a.md", "12345", agent("cutex.a")), 8, now).unwrap();
        assert!(matches!(
            store.publish(blob(&project, "b.md", "6789", agent("cutex.a")), 8, now),
            Err(StoreError::QuotaFull { used: 5, quota: 8 })
        ));
        // Same bytes need no space.
        store.publish(blob(&project, "dup.md", "12345", agent("cutex.b")), 8, now).unwrap();
        // External files: content not stored; same size + mtime + path = no-op.
        let mut external = blob(&project, "big.bin", "", agent("cutex.a"));
        external.content = Content::External { sha256: None, bytes: 1 << 40 };
        external.source_mtime_ms = Some(7);
        let first = store.publish(external.clone(), 8, now).unwrap();
        assert_eq!(first.version.storage, "external");
        assert!(!store.publish(external.clone(), 8, now).unwrap().new_version);
        external.source_mtime_ms = Some(8);
        assert!(store.publish(external, 8, now).unwrap().new_version);
        // Lists: newest first with a cursor.
        let later = now + chrono::Duration::seconds(5);
        store.publish(blob(&Scope::session("cutex.c"), "notes.md", "n", agent("cutex.c")), u64::MAX, later).unwrap();
        let all = ListFilter { limit: 2, ..ListFilter::default() };
        let (page, next) = store.list(&all).unwrap();
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].name, "notes.md");
        let (rest, end) = store.list(&ListFilter { cursor: next, ..all.clone() }).unwrap();
        assert_eq!(rest.len(), 2);
        assert!(end.is_none());
        let names = |filter: ListFilter| store.list(&filter).unwrap().0.into_iter().map(|a| a.name).collect::<Vec<_>>();
        let mut project_names = names(ListFilter { scopes: Some(vec![project.clone()]), limit: 50, ..ListFilter::default() });
        project_names.sort();
        assert_eq!(project_names, vec!["a.md", "big.bin", "dup.md"]);
        assert_eq!(names(ListFilter { q: Some("DUP".into()), limit: 50, ..ListFilter::default() }), vec!["dup.md"]);
        assert_eq!(names(ListFilter { q: Some("%".into()), limit: 50, ..ListFilter::default() }), Vec::<String>::new());
        assert_eq!(names(ListFilter { producer: Some("cutex.b".into()), limit: 50, ..ListFilter::default() }), vec!["dup.md"]);
        assert_eq!(names(ListFilter { kind: Some("plan".into()), limit: 50, ..ListFilter::default() }), Vec::<String>::new());
        assert_eq!(names(ListFilter { archived: Archived::Yes, limit: 50, ..ListFilter::default() }), Vec::<String>::new());
        assert!(matches!(store.list(&ListFilter { cursor: Some("junk".into()), ..all }), Err(StoreError::Invalid(_))));
        assert!(store.explicit_has_sha(&project, &crate::asks::sha256_hex(b"12345")).unwrap());
        assert!(!store.explicit_has_sha(&Scope::project("proj-b"), &crate::asks::sha256_hex(b"12345")).unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn actions_meta_windows_and_capture_state() {
        let (store, dir) = temp_store("misc");
        let now = Utc::now();
        assert_eq!(store.replay("cutex.a", "act", "d1").unwrap(), Ok(None));
        store.remember("cutex.a", "act", "d1", &serde_json::json!({"ok": true}), now).unwrap();
        assert_eq!(store.replay("cutex.a", "act", "d1").unwrap(), Ok(Some(serde_json::json!({"ok": true}))));
        assert_eq!(store.replay("cutex.a", "act", "d2").unwrap(), Err(()));
        assert_eq!(store.replay("cutex.b", "act", "d2").unwrap(), Ok(None));
        store.set_meta("k", Some("v")).unwrap();
        assert_eq!(store.meta("k").unwrap().as_deref(), Some("v"));
        store.set_meta("k", None).unwrap();
        assert_eq!(store.meta("k").unwrap(), None);
        let window = |session: &str, start: i64| TurnWindow {
            session_id: session.into(),
            turn_id: Some(format!("t{start}")),
            project_id: Some("proj-a".into()),
            started_at_ms: start,
            ..TurnWindow::default()
        };
        store.window_started(&window("cutex.a", 100)).unwrap();
        store.window_started(&window("cutex.a", 200)).unwrap();
        store.window_started(&window("cutex.b", 150)).unwrap();
        assert_eq!(store.window_ended("cutex.b", 300).unwrap().as_deref(), Some("proj-a"));
        let windows = store.project_windows("proj-a", 0).unwrap();
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0].ended_at_ms, Some(200), "a new turn closes the stale window");
        assert_eq!(windows[2].ended_at_ms, None);
        assert_eq!(windows[2].effective_end(250), 250);
        assert_eq!(store.project_windows("proj-a", 250).unwrap().len(), 2);
        assert_eq!(store.earliest_window_ms("proj-a").unwrap(), Some(100));
        assert_eq!(store.prune_windows(250).unwrap(), 1);
        store
            .set_capture_files("proj-a", &[("reports/a.md".into(), Some(CaptureFile { bytes: 1, mtime_ms: 2, sha256: None }))])
            .unwrap();
        assert_eq!(store.capture_files("proj-a").unwrap().len(), 1);
        store.set_capture_files("proj-a", &[("reports/a.md".into(), None)]).unwrap();
        assert!(store.capture_files("proj-a").unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
