//! 058 S4: the agent/project roster as keyed SQLite rows.
//!
//! The legacy `agent-management-v1.json` held every collection of
//! [`AgentManagementSnapshot`] in one pretty document that was rewritten on
//! each mutation. Here each collection entry is one row in a table split by
//! domain (agents, projects, memberships, grants, presentation, current
//! actions, receipts, history events), keyed by its map key and indexed by the
//! fields readers filter on. Row bodies are the compact serde encoding of the
//! same typed value, so the R1 exporter can rebuild the exact previous-build
//! document from the rows alone.
//!
//! History rows (phase/failure/audit events) stay queryable here because
//! business code reads them by action id. The append-only JSONL audit is fed
//! by `roster_audit_outbox` in the same transaction; it is an export, never a
//! source for receipts or for rollback.
//!
//! Files: `agent-management/v1/roster.sqlite3` (+ WAL) and `roster-audit.jsonl`.
//!
//! Authority: `roster_meta.verified = 1` makes this database the canonical
//! roster. The legacy JSON is read once by [`import_legacy`] and never
//! modified by it. [`export_previous`] (kit R1 only) writes the current rows
//! back as the previous build's JSON and hands authority back (verified = 0);
//! a later start of this build re-imports that JSON from scratch.


use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::de::DeserializeOwned;
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::{AgentManagementSnapshot, AgentManagementStoreSchema};
use crate::store::metrics::{self, OpCounts};

/// Manifest id of the roster store (058 §7 counters).
pub(crate) const STORE_ID: &str = "agent-management";

fn count(counts: OpCounts) {
    metrics::note(STORE_ID, counts);
}

pub const DATABASE_FILE: &str = "roster.sqlite3";
pub const AUDIT_FILE: &str = "roster-audit.jsonl";
pub const LEGACY_FILE: &str = "agent-management-v1.json";
const FORMAT_VERSION: i64 = 1;
/// Transport history for incremental cache refresh. Older readers fall back to
/// one full reload; business rows are never pruned to bound this table.
const CHANGE_RETENTION: i64 = 20_000;

/// The SQL roster lives in the store directory itself, next to the lock
/// files both builds share; an explicit test/kit root stays self-contained.
pub fn database_dir(v1_root: &Path) -> PathBuf {
    v1_root.to_path_buf()
}

pub fn database_path(v1_root: &Path) -> PathBuf {
    database_dir(v1_root).join(DATABASE_FILE)
}

const SCHEMA: &str = "
PRAGMA journal_mode=WAL;
PRAGMA synchronous=FULL;
CREATE TABLE IF NOT EXISTS roster_meta (
  singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
  format_version INTEGER NOT NULL,
  verified INTEGER NOT NULL CHECK (verified IN (0, 1)),
  schema_name TEXT NOT NULL,
  store_revision INTEGER NOT NULL,
  change_seq INTEGER NOT NULL,
  epoch TEXT NOT NULL,
  source_sha256 TEXT,
  source_bytes INTEGER,
  imported_at TEXT,
  audit_seq INTEGER NOT NULL DEFAULT 0,
  audit_offset INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS roster_agents (
  row_key TEXT PRIMARY KEY, project_id TEXT, name TEXT NOT NULL, retired INTEGER NOT NULL, body BLOB NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS roster_agents_project ON roster_agents(project_id);
CREATE INDEX IF NOT EXISTS roster_agents_name ON roster_agents(name);
CREATE TABLE IF NOT EXISTS roster_projects (row_key TEXT PRIMARY KEY, body BLOB NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS roster_project_states (row_key TEXT PRIMARY KEY, body BLOB NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS roster_project_tombstones (row_key TEXT PRIMARY KEY, body BLOB NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS roster_presentations (row_key TEXT PRIMARY KEY, body BLOB NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS roster_memberships (
  row_key TEXT PRIMARY KEY, project_id TEXT, body BLOB NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS roster_memberships_project ON roster_memberships(project_id);
-- One row per project: the complete Operator grant set, CAS'd as a set by
-- operator_grant_revisions (an empty set stays an explicit empty row).
CREATE TABLE IF NOT EXISTS roster_grants (row_key TEXT PRIMARY KEY, body BLOB NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS roster_grant_revisions (row_key TEXT PRIMARY KEY, body BLOB NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS roster_actions (
  row_key TEXT PRIMARY KEY, project_id TEXT, phase TEXT, operation TEXT, caller TEXT, body BLOB NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS roster_actions_project ON roster_actions(project_id);
CREATE INDEX IF NOT EXISTS roster_actions_phase ON roster_actions(phase);
CREATE INDEX IF NOT EXISTS roster_actions_legacy_close ON roster_actions(operation,phase);
CREATE INDEX IF NOT EXISTS roster_actions_external_message ON roster_actions(json_extract(CAST(body AS TEXT),'$.external_message_id'),row_key);
CREATE TABLE IF NOT EXISTS roster_receipts (
  kind TEXT NOT NULL, row_key TEXT NOT NULL, body BLOB NOT NULL, PRIMARY KEY (kind, row_key)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS roster_events (
  kind TEXT NOT NULL, row_key TEXT NOT NULL, action_id TEXT, project_id TEXT, body BLOB NOT NULL,
  PRIMARY KEY (kind, row_key)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS roster_events_action ON roster_events(kind, action_id);
CREATE TABLE IF NOT EXISTS roster_extra (row_key TEXT PRIMARY KEY, body BLOB NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS roster_changes (
  seq INTEGER PRIMARY KEY AUTOINCREMENT, collection TEXT NOT NULL, row_key TEXT NOT NULL,
  deleted INTEGER NOT NULL, store_revision INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS roster_audit_outbox (
  seq INTEGER PRIMARY KEY, collection TEXT NOT NULL, row_key TEXT NOT NULL, store_revision INTEGER NOT NULL,
  deleted INTEGER NOT NULL, body BLOB
);
";

/// One roster collection: where its rows live and which columns are indexed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Spec {
    /// The JSON key in the legacy document; also the change-log collection.
    pub field: &'static str,
    table: &'static str,
    /// Discriminator for shared tables (`roster_receipts`, `roster_events`).
    kind: Option<&'static str>,
    idx: &'static [&'static str],
    /// History collections also go to the JSONL audit through the outbox.
    audited: bool,
}

const fn spec(field: &'static str, table: &'static str, idx: &'static [&'static str]) -> Spec {
    Spec { field, table, kind: None, idx, audited: false }
}
const fn receipt(field: &'static str) -> Spec {
    Spec { field, table: "roster_receipts", kind: Some(field), idx: &[], audited: false }
}
const fn event(field: &'static str) -> Spec {
    Spec { field, table: "roster_events", kind: Some(field), idx: &["action_id", "project_id"], audited: true }
}

const AGENTS: Spec = spec("agents", "roster_agents", &["project_id", "name", "retired"]);
const PROJECTS: Spec = spec("projects", "roster_projects", &[]);
const PROJECT_STATES: Spec = spec("project_states", "roster_project_states", &[]);
const PROJECT_TOMBSTONES: Spec = spec("project_tombstones", "roster_project_tombstones", &[]);
const PRESENTATIONS: Spec = spec("project_presentations", "roster_presentations", &[]);
const MEMBERSHIPS: Spec = spec("current_project_memberships", "roster_memberships", &["project_id"]);
const GRANTS: Spec = spec("operator_grants", "roster_grants", &[]);
const GRANT_REVISIONS: Spec = spec("operator_grant_revisions", "roster_grant_revisions", &[]);
const ACTIONS: Spec = spec("actions", "roster_actions", &["project_id", "phase", "operation", "caller"]);
const EXTRA: Spec = spec("extra", "roster_extra", &[]);

/// A row as encoded for SQL: key(s), indexed columns in `Spec::idx` order and
/// the compact JSON body.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Row {
    pub spec: Spec,
    pub key: String,
    pub idx: Vec<SqlValue>,
    pub body: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Change {
    Put(Row),
    Delete { spec: Spec, key: String },
}

fn text<T: Serialize>(value: &T) -> SqlValue {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => SqlValue::Text(s),
        Ok(serde_json::Value::Null) | Err(_) => SqlValue::Null,
        Ok(other) => SqlValue::Text(other.to_string()),
    }
}

fn opt_text<T: Serialize>(value: Option<&T>) -> SqlValue {
    value.map(text).unwrap_or(SqlValue::Null)
}

fn map_key<K: Serialize>(key: &K) -> anyhow::Result<String> {
    match serde_json::to_value(key)? {
        serde_json::Value::String(s) => Ok(s),
        other => anyhow::bail!("roster map key is not a string: {other}"),
    }
}

fn parse_key<K: DeserializeOwned>(key: &str) -> anyhow::Result<K> {
    serde_json::from_value(serde_json::Value::String(key.to_owned()))
        .with_context(|| format!("invalid roster key {key:?}"))
}

fn no_idx<V>(_: &V) -> Vec<SqlValue> {
    Vec::new()
}

fn idx_agent(v: &super::ManagedAgentRecord) -> Vec<SqlValue> {
    vec![
        opt_text(v.project_id.as_ref()),
        SqlValue::Text(v.spec.name.clone()),
        SqlValue::Integer(i64::from(v.retired_at.is_some())),
    ]
}

fn idx_membership(v: &super::CurrentProjectMembership) -> Vec<SqlValue> {
    vec![opt_text(v.project_id.as_ref())]
}

fn idx_action(v: &super::AgentActionRecord) -> Vec<SqlValue> {
    vec![text(&v.project_id), text(&v.phase), text(&v.operation), text(&v.caller_cutex_session)]
}

fn idx_phase(v: &super::AgentManagementPhaseEvent) -> Vec<SqlValue> {
    vec![text(&v.action_id), text(&v.project_id)]
}
fn idx_failure(v: &super::AgentManagementFailureEvent) -> Vec<SqlValue> {
    vec![text(&v.action_id), text(&v.project_id)]
}
fn idx_operator_audit(v: &super::AgentOperatorAuditEvent) -> Vec<SqlValue> {
    vec![text(&v.action_id), text(&v.project_id)]
}
fn idx_project_audit(v: &super::ProjectAuditEvent) -> Vec<SqlValue> {
    vec![text(&v.action_id), text(&v.project_id)]
}
fn idx_reconciliation(v: &super::AgentReservationReconciliationEvent) -> Vec<SqlValue> {
    vec![text(&v.action_id), text(&v.project_id)]
}
fn idx_durable_audit(v: &super::DurableImportAuditEvent) -> Vec<SqlValue> {
    vec![text(&v.action_id), SqlValue::Null]
}
fn idx_archive_audit(v: &super::AgentArchiveReceipt) -> Vec<SqlValue> {
    vec![text(&v.request.action_id), SqlValue::Null]
}

/// Every single-key collection of the snapshot with its spec and index
/// extractor.
macro_rules! roster_collections {
    ($callback:ident!($($args:tt)*)) => {
        $callback!($($args)*;
            agents => AGENTS, idx_agent;
            projects => PROJECTS, no_idx;
            project_states => PROJECT_STATES, no_idx;
            project_tombstones => PROJECT_TOMBSTONES, no_idx;
            project_presentations => PRESENTATIONS, no_idx;
            current_project_memberships => MEMBERSHIPS, idx_membership;
            operator_grants => GRANTS, no_idx;
            operator_grant_revisions => GRANT_REVISIONS, no_idx;
            actions => ACTIONS, idx_action;
            bootstrap_intents => receipt("bootstrap_intents"), no_idx;
            agent_archive_actions => receipt("agent_archive_actions"), no_idx;
            durable_import_actions => receipt("durable_import_actions"), no_idx;
            human_management_operator_actions => receipt("human_management_operator_actions"), no_idx;
            human_management_project_mutations => receipt("human_management_project_mutations"), no_idx;
            authority_receipts => receipt("authority_receipts"), no_idx;
            legacy_director_ownership_import_receipts => receipt("legacy_director_ownership_import_receipts"), no_idx;
            reservation_reconciliation_receipts => receipt("reservation_reconciliation_receipts"), no_idx;
            sparse_requests => receipt("sparse_requests"), no_idx;
            phase_events => event("phase_events"), idx_phase;
            failure_events => event("failure_events"), idx_failure;
            agent_archive_audit => event("agent_archive_audit"), idx_archive_audit;
            durable_import_audit => event("durable_import_audit"), idx_durable_audit;
            operator_audit_events => event("operator_audit_events"), idx_operator_audit;
            project_audit_events => event("project_audit_events"), idx_project_audit;
            reservation_reconciliation_events => event("reservation_reconciliation_events"), idx_reconciliation;
            tool_action_receipts => receipt("tool_action_receipts"), no_idx;
            extra => EXTRA, no_idx;
        )
    };
}

/// Every spec, for schema checks and full loads.
pub(crate) fn all_specs() -> Vec<Spec> {
    macro_rules! list {
        (; $($field:ident => $spec:expr, $idx:path;)*) => { vec![$($spec,)*] };
    }
    roster_collections!(list!())
}

fn spec_for(field: &str) -> Option<Spec> {
    all_specs().into_iter().find(|spec| spec.field == field)
}

fn encode_entry<K: Serialize, V: Serialize>(
    spec: Spec,
    key: &K,
    value: &V,
    idx: impl Fn(&V) -> Vec<SqlValue>,
) -> anyhow::Result<Row> {
    let body = serde_json::to_vec(value)?;
    count(OpCounts { bytes_encoded: body.len() as u64, ..OpCounts::default() });
    Ok(Row {
        spec,
        key: map_key(key)?,
        idx: idx(value),
        body,
    })
}

/// All rows of a snapshot (import and tests). `reversible_archive_projection`
/// is `serde(skip)` and never persisted, exactly like the legacy file.
pub(crate) fn snapshot_rows(snapshot: &AgentManagementSnapshot) -> anyhow::Result<Vec<Row>> {
    let mut rows = Vec::new();
    macro_rules! encode {
        (; $($field:ident => $spec:expr, $idx:path;)*) => {$(
            for (key, value) in &snapshot.$field {
                rows.push(encode_entry($spec, key, value, $idx)?);
            }
        )*};
    }
    roster_collections!(encode!());
    Ok(rows)
}

/// Row changes turning `old` into `new`. Collections still sharing their
/// persistent tree are skipped outright; otherwise the diff walks only the
/// subtrees an edit copied, so the work follows the change, not the history.
pub(crate) fn diff(
    old: &AgentManagementSnapshot,
    new: &AgentManagementSnapshot,
) -> anyhow::Result<Vec<Change>> {
    use super::roster_map::Delta;
    let mut changes = Vec::new();
    macro_rules! diff_maps {
        (; $($field:ident => $spec:expr, $idx:path;)*) => {$(
            for delta in new.$field.diff(&old.$field) {
                changes.push(match delta {
                    Delta::Put(key, value) => Change::Put(encode_entry($spec, key, value, $idx)?),
                    Delta::Delete(key) => Change::Delete { spec: $spec, key: map_key(key)? },
                });
            }
        )*};
    }
    roster_collections!(diff_maps!());
    Ok(changes)
}

/// Apply one stored row (or its deletion) to an in-memory snapshot; used by
/// the incremental cache refresh.
pub(crate) fn apply_row(
    snapshot: &mut AgentManagementSnapshot,
    field: &str,
    key: &str,
    body: Option<&[u8]>,
) -> anyhow::Result<()> {
    macro_rules! apply {
        (; $($name:ident => $spec:expr, $idx:path;)*) => {
            match field {
                $(stringify!($name) => {
                    let key = parse_key(key)?;
                    match body {
                        Some(body) => {
                            count(OpCounts { rows_materialized: 1, bytes_decoded: body.len() as u64, ..OpCounts::default() });
                            snapshot.$name.insert(key, serde_json::from_slice(body)?);
                        }
                        None => { snapshot.$name.remove(&key); }
                    }
                    return Ok(());
                })*
                _ => {}
            }
        };
    }
    roster_collections!(apply!());
    anyhow::bail!("unknown roster collection {field}")
}

// ---------------------------------------------------------------- SQL access

fn open_rw(path: &Path) -> anyhow::Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        // Owner-private like the legacy store directory and file.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    let db = Connection::open(path)
        .with_context(|| format!("open roster database: {}", path.display()))?;
    // SQLite creates -wal/-shm with the database file's mode.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    db.busy_timeout(Duration::from_secs(5))?;
    db.execute_batch(SCHEMA)?;
    Ok(db)
}

/// An existing roster database for commits, export and audit drain: no
/// schema batch or permission work on the write path.
fn open_existing(path: &Path) -> anyhow::Result<Connection> {
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open roster database: {}", path.display()))?;
    db.busy_timeout(Duration::from_secs(5))?;
    Ok(db)
}

pub(crate) fn open_ro(path: &Path) -> anyhow::Result<Connection> {
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open roster database read-only: {}", path.display()))?;
    db.busy_timeout(Duration::from_secs(5))?;
    Ok(db)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RosterMeta {
    pub verified: bool,
    pub schema: String,
    pub store_revision: u64,
    pub change_seq: i64,
    pub epoch: String,
    pub source_sha256: Option<String>,
}

pub(crate) fn meta(db: &Connection) -> anyhow::Result<Option<RosterMeta>> {
    count(OpCounts { sql_statements: 2, ..OpCounts::default() });
    let exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='roster_meta')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(None);
    }
    db.query_row(
        "SELECT format_version, verified, schema_name, store_revision, change_seq, epoch, source_sha256
         FROM roster_meta WHERE singleton = 1",
        [],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                RosterMeta {
                    verified: row.get::<_, i64>(1)? != 0,
                    schema: row.get(2)?,
                    store_revision: row.get::<_, i64>(3)? as u64,
                    change_seq: row.get(4)?,
                    epoch: row.get(5)?,
                    source_sha256: row.get(6)?,
                },
            ))
        },
    )
    .optional()?
    .map(|(format, meta)| {
        anyhow::ensure!(format == FORMAT_VERSION, "unsupported roster format {format}");
        Ok(meta)
    })
    .transpose()
}

/// Whether the SQL roster is the canonical store at `path`.
pub fn is_authoritative(path: &Path) -> anyhow::Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    Ok(meta(&open_ro(path)?)?.is_some_and(|meta| meta.verified))
}

fn schema_name(schema: &AgentManagementStoreSchema) -> anyhow::Result<String> {
    match serde_json::to_value(schema)? {
        serde_json::Value::String(s) => Ok(s),
        other => anyhow::bail!("unexpected roster schema encoding {other}"),
    }
}

fn insert_row(tx: &Connection, row: &Row) -> anyhow::Result<()> {
    let mut columns: Vec<&str> = Vec::new();
    let mut values: Vec<SqlValue> = Vec::new();
    if let Some(kind) = row.spec.kind {
        columns.push("kind");
        values.push(SqlValue::Text(kind.to_owned()));
    }
    columns.push("row_key");
    values.push(SqlValue::Text(row.key.clone()));
    anyhow::ensure!(row.idx.len() == row.spec.idx.len(), "roster index arity for {}", row.spec.field);
    columns.extend(row.spec.idx.iter().copied());
    values.extend(row.idx.iter().cloned());
    columns.push("body");
    values.push(SqlValue::Blob(row.body.clone()));
    let placeholders = (1..=columns.len()).map(|n| format!("?{n}")).collect::<Vec<_>>().join(",");
    let sql = format!(
        "INSERT OR REPLACE INTO {} ({}) VALUES ({})",
        row.spec.table,
        columns.join(","),
        placeholders
    );
    tx.prepare_cached(&sql)?.execute(params_from_iter(values))?;
    Ok(())
}

fn key_filter(spec: Spec, key: &str) -> (String, Vec<SqlValue>) {
    let mut clauses = Vec::new();
    let mut values = Vec::new();
    if let Some(kind) = spec.kind {
        clauses.push(format!("kind = ?{}", values.len() + 1));
        values.push(SqlValue::Text(kind.to_owned()));
    }
    clauses.push(format!("row_key = ?{}", values.len() + 1));
    values.push(SqlValue::Text(key.to_owned()));
    (clauses.join(" AND "), values)
}

fn delete_row(tx: &Connection, spec: Spec, key: &str) -> anyhow::Result<()> {
    let (filter, values) = key_filter(spec, key);
    tx.prepare_cached(&format!("DELETE FROM {} WHERE {filter}", spec.table))?
        .execute(params_from_iter(values))?;
    Ok(())
}

/// One stored body by key (incremental refresh, keyed reads).
pub(crate) fn get_body(
    db: &Connection,
    spec: Spec,
    key: &str,
) -> anyhow::Result<Option<Vec<u8>>> {
    let (filter, values) = key_filter(spec, key);
    let body: Option<Vec<u8>> = db
        .prepare_cached(&format!("SELECT body FROM {} WHERE {filter}", spec.table))?
        .query_row(params_from_iter(values), |row| row.get(0))
        .optional()?;
    count(OpCounts {
        sql_statements: 1,
        rows_visited: 1,
        bytes_read: body.as_ref().map_or(0, |body| body.len() as u64),
        ..OpCounts::default()
    });
    Ok(body)
}

fn log_change(
    tx: &Connection,
    spec: Spec,
    key: &str,
    deleted: bool,
    revision: u64,
    body: Option<&[u8]>,
) -> anyhow::Result<i64> {
    tx.prepare_cached(
        "INSERT INTO roster_changes(collection, row_key, deleted, store_revision) VALUES (?1,?2,?3,?4)",
    )?
    .execute(params![spec.field, key, deleted, revision as i64])?;
    let seq = tx.last_insert_rowid();
    if spec.audited {
        tx.prepare_cached(
            "INSERT INTO roster_audit_outbox(seq, collection, row_key, store_revision, deleted, body) VALUES (?1,?2,?3,?4,?5,?6)",
        )?
        .execute(params![seq, spec.field, key, revision as i64, deleted, body])?;
    }
    Ok(seq)
}

/// Commit row changes with a store-revision CAS. Returns the new meta.
pub(crate) fn commit(
    path: &Path,
    expected_revision: u64,
    schema: &AgentManagementStoreSchema,
    new_revision: u64,
    changes: &[Change],
) -> anyhow::Result<RosterMeta> {
    let mut db = open_existing(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current = meta(&tx)?.context("roster database has no metadata")?;
    anyhow::ensure!(current.verified, "roster database is not authoritative");
    anyhow::ensure!(
        current.store_revision == expected_revision,
        "roster store_revision changed from {expected_revision} to {}",
        current.store_revision
    );
    let mut seq = current.change_seq;
    for change in changes {
        match change {
            Change::Put(row) => {
                insert_row(&tx, row)?;
                seq = log_change(&tx, row.spec, &row.key, false, new_revision, Some(&row.body))?;
            }
            Change::Delete { spec, key } => {
                delete_row(&tx, *spec, key)?;
                seq = log_change(&tx, *spec, key, true, new_revision, None)?;
            }
        }
    }
    tx.execute(
        "UPDATE roster_meta SET schema_name = ?1, store_revision = ?2, change_seq = ?3 WHERE singleton = 1",
        params![schema_name(schema)?, new_revision as i64, seq],
    )?;
    let pruned = seq % 512 == 0;
    if pruned {
        tx.execute("DELETE FROM roster_changes WHERE seq <= ?1", [seq - CHANGE_RETENTION])?;
    }
    tx.commit()?;
    let audited = changes.iter().filter(|change| match change {
        Change::Put(row) => row.spec.audited,
        Change::Delete { spec, .. } => spec.audited,
    }).count() as u64;
    count(OpCounts {
        // meta read, row write + change row (+ outbox row) each, meta update.
        sql_statements: 2 + 2 * changes.len() as u64 + audited + u64::from(pruned),
        rows_visited: changes.len() as u64,
        bytes_written: changes.iter().map(|change| match change {
            Change::Put(row) => row.body.len() as u64,
            Change::Delete { .. } => 0,
        }).sum(),
        ..OpCounts::default()
    });
    Ok(RosterMeta { schema: schema_name(schema)?, store_revision: new_revision, change_seq: seq, ..current })
}

/// Changed keys after `after_seq`, or `None` when the log no longer reaches
/// back that far (caller reloads once).
pub(crate) fn changes_since(
    db: &Connection,
    after_seq: i64,
) -> anyhow::Result<Option<Vec<(String, String)>>> {
    let oldest: Option<i64> = db.query_row("SELECT MIN(seq) FROM roster_changes", [], |row| row.get(0))?;
    if let Some(oldest) = oldest {
        if oldest > after_seq + 1 {
            return Ok(None);
        }
    }
    let mut statement = db.prepare_cached(
        "SELECT collection, row_key FROM roster_changes WHERE seq > ?1 ORDER BY seq",
    )?;
    let rows = statement.query_map([after_seq], |row| Ok((row.get(0)?, row.get(1)?)))?;
    let mut changed = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    count(OpCounts { sql_statements: 2, rows_visited: changed.len() as u64, ..OpCounts::default() });
    changed.sort();
    changed.dedup();
    Ok(Some(changed))
}

/// Full decode of every row (cold load, export). Allowlisted whole-state
/// read: the migration/export/cold-cache paths only.
pub(crate) fn load_all(db: &Connection) -> anyhow::Result<(RosterMeta, AgentManagementSnapshot)> {
    metrics::note_whole_state_copy(STORE_ID);
    load_view(db, false)
}

/// Directory/authority readers never need action payloads or event history.
/// Keep these collections in SQL; complete snapshots/export still use load_all.
pub(crate) fn directory_field(field: &str) -> bool {
    !matches!(field, "actions" | "phase_events" | "failure_events"
        | "sparse_requests" | "tool_action_receipts")
}

pub(crate) fn load_directory(db: &Connection) -> anyhow::Result<(RosterMeta, AgentManagementSnapshot)> {
    load_view(db, true)
}

fn load_view(db: &Connection, directory: bool) -> anyhow::Result<(RosterMeta, AgentManagementSnapshot)> {
    let meta = meta(db)?.context("roster database has no metadata")?;
    // Cold load / export / epoch change only (allowlisted whole-state read).
    let mut snapshot = AgentManagementSnapshot::empty();
    snapshot.schema = serde_json::from_value(serde_json::Value::String(meta.schema.clone()))?;
    snapshot.store_revision = meta.store_revision;
    for spec in all_specs().into_iter().filter(|spec| !directory || directory_field(spec.field)) {
        let mut sql = String::from("SELECT row_key, body FROM ");
        sql.push_str(spec.table);
        let mut values = Vec::new();
        if let Some(kind) = spec.kind {
            sql.push_str(" WHERE kind = ?1");
            values.push(SqlValue::Text(kind.to_owned()));
        }
        let mut statement = db.prepare(&sql)?;
        let mut rows = statement.query(params_from_iter(values))?;
        while let Some(row) = rows.next()? {
            let key: String = row.get(0)?;
            let body: Vec<u8> = row.get(1)?;
            apply_row(&mut snapshot, spec.field, &key, Some(&body))?;
        }
    }
    if directory {
        snapshot.legacy_close_projection = Some(std::sync::Arc::new(legacy_close_evidence(db)?));
    }
    Ok((meta, snapshot))
}

// ----------------------------------------------------------- import / export

/// What [`import_legacy`] did, for the kit log.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ImportReport {
    pub source_bytes: u64,
    pub source_sha256: String,
    pub rows: u64,
    pub store_revision: u64,
    pub millis: u128,
    pub already_authoritative: bool,
}

/// Validate legacy bytes exactly like the previous build's reader
/// (`store::read_snapshot_file`) minus its in-memory project-state migration,
/// which the SQL read path keeps applying.
pub(crate) fn decode_legacy(bytes: &[u8]) -> anyhow::Result<AgentManagementSnapshot> {
    let mut state: AgentManagementSnapshot =
        serde_json::from_slice(bytes).context("legacy roster JSON does not parse")?;
    super::store::validate_snapshot(&state).map_err(|_| anyhow::anyhow!("legacy roster fails validation"))?;
    split_tool_receipts(&mut state);
    Ok(state)
}

/// The previous build's additive key holding every `agent_create` receipt.
pub(crate) const TOOL_RECEIPTS_EXTENSION: &str = "tool_action_receipts_v1";

/// Previous layout -> rows: each receipt of the `extra` object becomes its
/// own `tool_action_receipts` row and the key leaves `extra` (the export puts
/// it back in the same sorted position). An empty object or a non-object
/// value has no receipts and stays as it is.
fn split_tool_receipts(state: &mut AgentManagementSnapshot) {
    if !state
        .extra
        .get(TOOL_RECEIPTS_EXTENSION)
        .and_then(serde_json::Value::as_object)
        .is_some_and(|receipts| !receipts.is_empty())
    {
        return;
    }
    if let Some(serde_json::Value::Object(receipts)) = state.extra.remove(TOOL_RECEIPTS_EXTENSION) {
        for (action_id, receipt) in receipts {
            state.tool_action_receipts.insert(action_id, receipt);
        }
    }
}

/// Rows -> previous layout (R1): receipt rows merged into the `extra` object.
fn fold_tool_receipts(state: &AgentManagementSnapshot) -> anyhow::Result<AgentManagementSnapshot> {
    let mut folded = state.clone();
    if folded.tool_action_receipts.is_empty() {
        return Ok(folded);
    }
    let rows = std::mem::take(&mut folded.tool_action_receipts);
    let object = folded
        .extra
        .entry(TOOL_RECEIPTS_EXTENSION.to_owned())
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    let serde_json::Value::Object(receipts) = object else {
        anyhow::bail!("{TOOL_RECEIPTS_EXTENSION} is not an object; receipt rows cannot be exported");
    };
    for (action_id, receipt) in rows {
        anyhow::ensure!(
            receipts.insert(action_id.clone(), receipt).is_none(),
            "tool receipt {action_id} stored twice"
        );
    }
    Ok(folded)
}

/// One-shot import of the legacy JSON into a fresh authoritative database.
/// Caller holds the roster's exclusive store lock and all writers are stopped
/// (058 §4). The legacy file is only read. An unverified leftover database
/// (crash, or an R1 handoff) is discarded and rebuilt from the JSON.
pub fn import_legacy(v1_root: &Path) -> anyhow::Result<ImportReport> {
    let started = std::time::Instant::now();
    let path = database_path(v1_root);
    if is_authoritative(&path)? {
        return Ok(ImportReport { already_authoritative: true, ..Default::default() });
    }
    let legacy_path = v1_root.join(LEGACY_FILE);
    let bytes = match std::fs::read(&legacy_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error).with_context(|| format!("read {}", legacy_path.display())),
    };
    let snapshot = if bytes.is_empty() {
        AgentManagementSnapshot::empty()
    } else {
        decode_legacy(&bytes)?
    };
    let digest = format!("{:x}", Sha256::digest(&bytes));
    let rows = snapshot_rows(&snapshot)?;
    // Unverified leftovers are never trusted or completed: start clean.
    for suffix in ["", "-wal", "-shm"] {
        let leftover = PathBuf::from(format!("{}{suffix}", path.display()));
        if leftover.exists() {
            std::fs::remove_file(&leftover).with_context(|| format!("remove {}", leftover.display()))?;
        }
    }
    // A new database starts a new sequence space: seal the previous audit
    // file as a segment (renamed, never deleted) so its record ids stay
    // unambiguous and the new epoch appends to a fresh file.
    let audit = database_dir(v1_root).join(AUDIT_FILE);
    if audit.exists() {
        let sealed = database_dir(v1_root).join(format!(
            "roster-audit-sealed-{}.jsonl",
            chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ")
        ));
        std::fs::rename(&audit, &sealed).with_context(|| format!("seal {}", audit.display()))?;
    }
    let mut db = open_rw(&path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT INTO roster_meta(singleton, format_version, verified, schema_name, store_revision, change_seq, epoch, source_sha256, source_bytes, imported_at)
         VALUES (1, ?1, 0, ?2, ?3, 0, ?4, ?5, ?6, ?7)",
        params![
            FORMAT_VERSION,
            schema_name(&snapshot.schema)?,
            snapshot.store_revision as i64,
            uuid::Uuid::new_v4().to_string(),
            digest,
            bytes.len() as i64,
            chrono::Utc::now().to_rfc3339(),
        ],
    )?;
    for row in &rows {
        insert_row(&tx, row)?;
        // Existing history goes to the audit log once, in commit order.
        if row.spec.audited {
            log_change(&tx, row.spec, &row.key, false, snapshot.store_revision, Some(&row.body))?;
        }
    }
    // The change log is transport for cache refresh; the import is epoch 0.
    // AUTOINCREMENT keeps sequences monotonic across this clear, so the
    // audit outbox and later change seqs never fall under a checkpoint.
    tx.execute(
        "UPDATE roster_meta SET change_seq = (SELECT COALESCE(MAX(seq), 0) FROM roster_changes) WHERE singleton = 1",
        [],
    )?;
    tx.execute("DELETE FROM roster_changes", [])?;
    // Verify inside the same transaction: every row decodes back to the source.
    let (_, reloaded) = load_all(&tx)?;
    anyhow::ensure!(reloaded == snapshot, "roster import verification failed");
    tx.execute("UPDATE roster_meta SET verified = 1 WHERE singleton = 1", [])?;
    tx.commit()?;
    // Fold the import into the main file so the WAL does not keep a second
    // copy of every page until the next automatic checkpoint.
    db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    Ok(ImportReport {
        source_bytes: bytes.len() as u64,
        source_sha256: digest,
        rows: rows.len() as u64,
        store_revision: snapshot.store_revision,
        millis: started.elapsed().as_millis(),
        already_authoritative: false,
    })
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ExportReport {
    pub bytes: u64,
    pub sha256: String,
    pub store_revision: u64,
    pub millis: u128,
}

/// R1 (kit only): write the current rows as the exact previous-build JSON and
/// hand authority back to it. Caller has stopped every writer and holds the
/// roster store lock. Order: staging write -> re-read + typed equality ->
/// atomic replace -> `verified = 0`. A crash before the replace changes
/// nothing; after it both builds read identical data.
pub fn export_previous(v1_root: &Path) -> anyhow::Result<ExportReport> {
    let started = std::time::Instant::now();
    let path = database_path(v1_root);
    let mut db = open_existing(&path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (meta, snapshot) = load_all(&tx)?;
    anyhow::ensure!(meta.verified, "roster database is not authoritative; nothing to export");
    let previous = fold_tool_receipts(&snapshot)?;
    let bytes = serde_json::to_vec_pretty(&previous)?;
    // The previous build's own reader must accept the bytes and see the same
    // state (compared in its layout: receipts inside `extra`).
    anyhow::ensure!(fold_tool_receipts(&decode_legacy(&bytes)?)? == previous, "exported roster does not round-trip");
    let staging = v1_root.join(format!("{LEGACY_FILE}.r1-staging"));
    crate::config::atomic::write_private_bytes_atomic(&staging, &bytes)?;
    anyhow::ensure!(std::fs::read(&staging)? == bytes, "R1 staging readback differs");
    std::fs::rename(&staging, v1_root.join(LEGACY_FILE)).context("replace legacy roster")?;
    super::store::forget_cached(v1_root);
    tx.execute("UPDATE roster_meta SET verified = 0 WHERE singleton = 1", [])?;
    tx.commit()?;
    Ok(ExportReport {
        bytes: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        store_revision: snapshot.store_revision,
        millis: started.elapsed().as_millis(),
    })
}

// ------------------------------------------------------------------- audit

/// Drain the transactional outbox into the append-only JSONL audit. Records
/// carry the outbox sequence as their id; a crash between append and the
/// checkpoint is repaired by skipping ids already present after the last
/// checkpointed offset, and a torn final line past the checkpoint is cut.
pub(crate) fn drain_audit(v1_root: &Path, max_rows: usize) -> anyhow::Result<usize> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let path = database_path(v1_root);
    let audit_path = database_dir(v1_root).join(AUDIT_FILE);
    let mut db = open_existing(&path)?;
    let (checkpoint_seq, checkpoint_offset, epoch): (i64, i64, String) = db.query_row(
        "SELECT audit_seq, audit_offset, epoch FROM roster_meta WHERE singleton = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let pending = {
        let mut statement = db.prepare(
            "SELECT seq, collection, row_key, store_revision, deleted, body FROM roster_audit_outbox
             WHERE seq > ?1 ORDER BY seq LIMIT ?2",
        )?;
        let rows = statement.query_map(params![checkpoint_seq, max_rows as i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, bool>(4)?,
                row.get::<_, Option<Vec<u8>>>(5)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    if pending.is_empty() {
        return Ok(0);
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&audit_path)
        .with_context(|| format!("open {}", audit_path.display()))?;
    let length = file.metadata()?.len() as i64;
    anyhow::ensure!(length >= checkpoint_offset, "roster audit log shorter than its checkpoint");
    // Only the unchecked tail is read: ids already appended by a crashed drain.
    let mut tail = Vec::new();
    file.seek(SeekFrom::Start(checkpoint_offset as u64))?;
    file.read_to_end(&mut tail)?;
    let complete = tail.iter().rposition(|byte| *byte == b'\n').map_or(0, |at| at + 1);
    if complete < tail.len() {
        file.set_len((checkpoint_offset as usize + complete) as u64)?;
    }
    let mut present = std::collections::BTreeSet::new();
    for line in tail[..complete].split(|byte| *byte == b'\n').filter(|line| !line.is_empty()) {
        if let Ok(record) = serde_json::from_slice::<serde_json::Value>(line) {
            if let Some(seq) = record.get("seq").and_then(|seq| seq.as_i64()) {
                present.insert(seq);
            }
        }
    }
    file.seek(SeekFrom::End(0))?;
    let mut buffer = Vec::new();
    let mut last = checkpoint_seq;
    for (seq, collection, key, revision, deleted, body) in &pending {
        last = *seq;
        if present.contains(seq) {
            continue;
        }
        let value = match body {
            Some(body) => serde_json::from_slice::<serde_json::Value>(body)?,
            None => serde_json::Value::Null,
        };
        serde_json::to_writer(
            &mut buffer,
            &serde_json::json!({"epoch": epoch, "seq": seq, "collection": collection, "key": key,
                "store_revision": revision, "deleted": deleted, "record": value}),
        )?;
        buffer.push(b'\n');
    }
    file.write_all(&buffer)?;
    file.sync_all()?;
    let offset = file.metadata()?.len() as i64;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "UPDATE roster_meta SET audit_seq = ?1, audit_offset = ?2 WHERE singleton = 1",
        params![last, offset],
    )?;
    tx.execute("DELETE FROM roster_audit_outbox WHERE seq <= ?1", [last])?;
    tx.commit()?;
    Ok(pending.len())
}

/// The committed raw roster under `v1_root` for offline readers that must
/// not open the store (cleanup, kit checks): the SQL rows while they are
/// authoritative, else the legacy JSON. `None` when neither exists. No locks,
/// no directory permission changes, never imports.
pub fn read_committed(v1_root: &Path) -> anyhow::Result<Option<AgentManagementSnapshot>> {
    let path = database_path(v1_root);
    if is_authoritative(&path)? {
        return Ok(Some(load_all(&open_ro(&path)?)?.1));
    }
    match std::fs::read(v1_root.join(LEGACY_FILE)) {
        Ok(bytes) => decode_legacy(&bytes).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("read legacy roster"),
    }
}

/// Read-only status for kit check-only: never imports, never locks.
pub fn status(v1_root: &Path) -> anyhow::Result<serde_json::Value> {
    let path = database_path(v1_root);
    let legacy = v1_root.join(LEGACY_FILE);
    let file_len = |p: &Path| std::fs::metadata(p).map(|m| m.len()).ok();
    let mut out = serde_json::json!({
        "database": path.display().to_string(),
        "databaseBytes": file_len(&path),
        "walBytes": file_len(Path::new(&format!("{}-wal", path.display()))),
        "legacyBytes": file_len(&legacy),
        "auditBytes": file_len(&database_dir(v1_root).join(AUDIT_FILE)),
        "authority": "none",
    });
    if path.exists() {
        let db = open_ro(&path)?;
        if let Some(meta) = meta(&db)? {
            out["authority"] = serde_json::json!(if meta.verified { "sqlite" } else { "legacy-json" });
            out["storeRevision"] = serde_json::json!(meta.store_revision);
            out["changeSeq"] = serde_json::json!(meta.change_seq);
            out["epoch"] = serde_json::json!(meta.epoch);
            out["sourceSha256"] = serde_json::json!(meta.source_sha256);
            let mut rows = serde_json::Map::new();
            for spec in all_specs() {
                let (sql, values) = match spec.kind {
                    Some(kind) => (format!("SELECT COUNT(*) FROM {} WHERE kind = ?1", spec.table), vec![SqlValue::Text(kind.into())]),
                    None => (format!("SELECT COUNT(*) FROM {}", spec.table), Vec::new()),
                };
                let count: i64 = db.query_row(&sql, params_from_iter(values), |row| row.get(0))?;
                rows.insert(spec.field.into(), serde_json::json!(count));
            }
            out["rows"] = serde_json::Value::Object(rows);
            out["auditPending"] = serde_json::json!(db.query_row("SELECT COUNT(*) FROM roster_audit_outbox", [], |row| row.get::<_, i64>(0))?);
        }
    } else if legacy.exists() {
        out["authority"] = serde_json::json!("legacy-json");
    }
    Ok(out)
}

/// Guards for direct exports to builds older than the exact previous one:
/// they read the legacy JSON, which is stale while SQL is authoritative.
pub fn ensure_legacy_authoritative(v1_root: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(
        !is_authoritative(&database_path(v1_root))?,
        "the agent roster is stored in SQLite (roster.sqlite3); run the kit's R1 roster export to the previous build first"
    );
    Ok(())
}

/// The committed rows of one event collection that belong to `action_id`,
/// in key order (the in-memory map's order): one indexed page read, never a
/// scan of the collection's history.
pub(crate) fn events_for_action<T: DeserializeOwned>(
    db: &Connection,
    field: &str,
    action_id: &str,
) -> anyhow::Result<Vec<T>> {
    let spec = spec_for(field).context("unknown roster collection")?;
    anyhow::ensure!(spec.table == "roster_events", "{field} is not an event collection");
    // Without statistics SQLite picks the (kind, row_key) primary key and
    // walks the whole collection; force the action index, sort the page here.
    let mut rows = db
        .prepare_cached("SELECT row_key, body FROM roster_events INDEXED BY roster_events_action WHERE kind = ?1 AND action_id = ?2")?
        .query_map(params![spec.kind, action_id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let bodies = rows.into_iter().map(|(_, body)| body).collect::<Vec<_>>();
    let bytes = bodies.iter().map(|body| body.len() as u64).sum::<u64>();
    count(OpCounts {
        sql_statements: 1,
        rows_visited: bodies.len() as u64,
        rows_materialized: bodies.len() as u64,
        bytes_read: bytes,
        bytes_decoded: bytes,
        ..OpCounts::default()
    });
    bodies.iter().map(|body| Ok(serde_json::from_slice(body)?)).collect()
}

/// Collections whose spec is known; exposed for manifest/tests.
pub(crate) fn spec_by_field(field: &str) -> Option<Spec> {
    spec_for(field)
}

#[cfg(test)]
#[path = "roster_sql_tests.rs"]
mod tests;

pub(crate) fn directory_index_ready(db: &Connection) -> anyhow::Result<bool> {
    Ok(db.query_row("SELECT count(*)=2 FROM sqlite_master WHERE type='index' AND name IN ('roster_actions_legacy_close','roster_actions_external_message')", [], |row| row.get(0))?)
}

pub(crate) fn prepare_directory_index(path: &Path) -> anyhow::Result<()> {
    open_existing(path)?.execute_batch("CREATE INDEX IF NOT EXISTS roster_actions_legacy_close ON roster_actions(operation,phase);
        CREATE INDEX IF NOT EXISTS roster_actions_external_message ON roster_actions(json_extract(CAST(body AS TEXT),'$.external_message_id'),row_key)")?;
    Ok(())
}

// CROSS JOIN keeps the selected action index outermost. An ordinary JOIN can
// choose the phase-kind index first and visit every unrelated phase event.
const LEGACY_CLOSE_SQL: &str = "SELECT e.body FROM roster_actions a INDEXED BY roster_actions_legacy_close CROSS JOIN roster_events e INDEXED BY roster_events_action ON e.kind='phase_events' AND e.action_id=a.row_key WHERE a.operation IN (?1,?3)
    AND ((a.operation=?1 AND a.phase!=?2) OR (a.operation=?3 AND a.phase IN (?4,?5)))";

/// The original legacy_archive_ambiguous predicate, reduced to session IDs in
/// the same WAL read snapshot. Never decode/store unrelated action bodies.
pub(crate) fn legacy_close_evidence(db: &Connection) -> anyhow::Result<std::collections::BTreeSet<String>> {
    use super::{AgentActionPhase as Phase, AgentOperationKind as Operation};
    let mut statement = db.prepare_cached(LEGACY_CLOSE_SQL)?;
    let mut rows = statement.query(params![text(&Operation::Close),text(&Phase::NoWrite),text(&Operation::Replace),text(&Phase::PredecessorClosing),text(&Phase::PredecessorClosed)])?;
    let mut ids = std::collections::BTreeSet::new();
    while let Some(row) = rows.next()? {
        let body: Vec<u8> = row.get(0)?;
        let event: super::AgentManagementPhaseEvent = serde_json::from_slice(&body)?;
        count(OpCounts { rows_visited:1, rows_materialized:1, bytes_read:body.len() as u64, bytes_decoded:body.len() as u64, ..OpCounts::default() });
        ids.extend(event.subject_cutex_session_id.into_iter().chain(event.predecessor_cutex_session_id).map(|id|id.as_str().to_owned()));
    }
    Ok(ids)
}

/// Only historical rotations need the removed-operation recovery path.
pub(crate) fn has_unfinished_rotation(db: &Connection) -> anyhow::Result<bool> {
    let mut query = db.prepare_cached("SELECT body FROM roster_actions INDEXED BY roster_actions_legacy_close WHERE operation=?1")?;
    let mut rows = query.query([text(&super::AgentOperationKind::DirectorRotate)])?;
    while let Some(row) = rows.next()? {
        let body: Vec<u8> = row.get(0)?;
        count(OpCounts {rows_visited:1,rows_materialized:1,bytes_read:body.len() as u64,bytes_decoded:body.len() as u64,..OpCounts::default()});
        let action: super::AgentActionRecord = serde_json::from_slice(&body)?;
        if action.response.is_none() {return Ok(true);}
    }
    Ok(false)
}

/// Two matches preserve the original ambiguity refusal for an external ID.
pub(crate) fn action_rows_for_starts(db: &Connection, ids: &[String]) -> anyhow::Result<std::collections::BTreeMap<String,Vec<u8>>> {
    let mut selected = std::collections::BTreeMap::new();
    if ids.is_empty() {return Ok(selected);}
    let mut query = db.prepare_cached("SELECT row_key,body FROM roster_actions INDEXED BY roster_actions_external_message WHERE json_extract(CAST(body AS TEXT),'$.external_message_id')=?1 ORDER BY row_key LIMIT 2")?;
    for id in ids {
        let mut rows = query.query([id])?;
        while let Some(row) = rows.next()? {
            let key: String = row.get(0)?;let body: Vec<u8> = row.get(1)?;
            count(OpCounts {sql_statements:1,rows_visited:1,bytes_read:body.len() as u64,..OpCounts::default()});
            selected.insert(key,body);
        }
    }
    Ok(selected)
}
