//! Permanent, reversible cold rows for completed delegations.
//!
//! Callers hold the asks file lock while planning and applying a move. SQL
//! generation CAS additionally rejects writers which do not share that lock.
//! Only explicit history reads combine cold rows with the live AskState.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;

use super::model::{DeliveryState, QuestionState};
use super::store::AskState;
use crate::management::v2::hot_store::{RowKey, Rows};

pub const FORMAT_VERSION: i64 = 2;
const ARCHIVE_SCHEMA: i64 = 1;

/// Explicit overrides are RFC3339 instants. The default is the activation
/// day's local midnight, converted once to UTC and persisted by move_rows.
pub fn activation_cutoff(
    now: DateTime<Utc>,
    override_value: Option<&str>,
) -> anyhow::Result<DateTime<Utc>> {
    use chrono::TimeZone;
    if let Some(value) = override_value {
        return DateTime::parse_from_rfc3339(value)
            .map(|at| at.with_timezone(&Utc))
            .context("CUTEX_ASKS_ARCHIVE_BEFORE must be an RFC3339 cutoff instant");
    }
    let midnight = now
        .with_timezone(&chrono::Local)
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .context("activation date")?;
    chrono::Local
        .from_local_datetime(&midnight)
        .single()
        .map(|at| at.with_timezone(&Utc))
        .context(
            "activation midnight is ambiguous/nonexistent; configure an explicit archive cutoff",
        )
}

/// Only a missing archive policy uses today's default. Restarts must never
/// silently advance the cutoff and archive an additional day's records.
pub fn persisted_cutoff(path: &Path) -> anyhow::Result<Option<DateTime<Utc>>> {
    if !path.exists() {
        return Ok(None);
    }
    let db = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    db.busy_timeout(Duration::from_secs(5))?;
    let exists:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='asks_archive_meta')",[],|r|r.get(0))?;
    if !exists {
        require_no_unmarked_archive(&db)?;
        return Ok(None);
    }
    let policy: Option<(i64, String)> = db
        .query_row(
            "SELECT schema_version,cutoff FROM asks_archive_meta WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if policy.is_none() {
        require_no_unmarked_archive(&db)?;
    }
    policy
        .map(|(version, value)| {
            anyhow::ensure!(version == ARCHIVE_SCHEMA, "unsupported asks archive schema");
            super::parse_time(&value).context("corrupt persisted asks archive cutoff")
        })
        .transpose()
}

fn require_no_unmarked_archive(db: &Connection) -> anyhow::Result<()> {
    let cold_table:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='asks_archive_rows')",[],|r|r.get(0))?;
    if cold_table {
        let records: bool =
            db.query_row("SELECT EXISTS(SELECT 1 FROM asks_archive_rows)", [], |r| {
                r.get(0)
            })?;
        anyhow::ensure!(!records,"asks archive policy missing while cold records remain; refusing legacy cleanup/rollback");
    }
    let hot_table: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='hot_store_meta')",
        [],
        |r| r.get(0),
    )?;
    if hot_table {
        let format: Option<i64> = db
            .query_row(
                "SELECT format_version FROM hot_store_meta WHERE store='asks'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        anyhow::ensure!(
            format != Some(FORMAT_VERSION),
            "asks archive policy missing for format 2; reconcile before any cleanup/rollback"
        );
    }
    Ok(())
}

pub fn require_legacy_compatible(path: &Path) -> anyhow::Result<()> {
    if persisted_cutoff(path)?.is_none() {
        return Ok(());
    }
    let db = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    let format: Option<i64> = db
        .query_row(
            "SELECT format_version FROM hot_store_meta WHERE store='asks'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let count: i64 = db.query_row("SELECT count(*) FROM asks_archive_rows", [], |r| r.get(0))?;
    anyhow::ensure!(format!=Some(FORMAT_VERSION)&&count==0,"asks archive rollback required: stop all cutex writers and run the outgoing r262 binary's storage asks-archive-restore --target <preceding-release> --daemon-stopped before an older export/binary; history must not be omitted");
    Ok(())
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveReport {
    pub rows: usize,
    pub bytes: usize,
    pub asks: usize,
    pub generation: u64,
}

/// No timestamps are inferred from IDs, file mtimes, or malformed dates.
fn before(value: &str, cutoff: DateTime<Utc>) -> bool {
    super::parse_time(value).is_some_and(|at| at < cutoff)
}

fn strings(value: &Value, visit: &mut impl FnMut(&str)) {
    match value {
        Value::String(value) => visit(value),
        Value::Array(values) => values.iter().for_each(|v| strings(v, visit)),
        Value::Object(values) => values.values().for_each(|v| strings(v, visit)),
        _ => {}
    }
}

/// Build a conservative transitive protection graph, not just a closedAt
/// filter. Unknown/active references stay hot. Actions are retained hot but
/// their completed receipts are history, not operational dependencies.
pub fn plan(state: &AskState, cutoff: DateTime<Utc>) -> anyhow::Result<BTreeSet<RowKey>> {
    plan_with_protected_roots(state, cutoff, &BTreeSet::new())
}

pub fn sidecar_roots(state: &crate::director_address::OwnershipState) -> BTreeSet<String> {
    state
        .asks
        .values()
        .filter(|item| item.pending_reconcile || item.owner_hold || item.summary_pending)
        .map(|item| item.ask_id.clone())
        .chain(
            state
                .reviews
                .values()
                .filter(|review| review.state == "pending")
                .map(|review| review.ask_id.clone()),
        )
        .collect()
}

pub fn plan_with_protected_roots(
    state: &AskState,
    cutoff: DateTime<Utc>,
    sidecar: &BTreeSet<String>,
) -> anyhow::Result<BTreeSet<RowKey>> {
    let rows = super::store::ask_state_rows(state)?;
    let mut candidates = BTreeSet::new();
    for (id, ask) in &state.asks {
        let complete = !ask.is_open()
            && ask.execution.is_terminal()
            && ask.hold.is_none()
            && ask.awaiting.is_none()
            && ask.resume.is_none()
            && ask.jobs.iter().all(|job| job.is_terminal())
            && ask
                .current_attempt()
                .is_none_or(|attempt| attempt.state.is_terminal())
            && !ask
                .remote
                .as_ref()
                .is_some_and(|r| !r.sync.pending_ops.is_empty())
            && !ask
                .peer_inbound
                .as_ref()
                .is_some_and(|r| !r.peer_deliveries.is_empty());
        if complete
            && ask
                .closed_at
                .as_deref()
                .is_some_and(|at| before(at, cutoff))
        {
            candidates.insert(("asks".into(), id.clone()));
        }
    }
    for (id, answer) in &state.answers {
        if candidates.contains(&("asks".into(), answer.ask_id.clone()))
            && before(&answer.created_at, cutoff)
        {
            candidates.insert(("answers".into(), id.clone()));
        }
    }
    for (id, question) in &state.questions {
        if question.state != QuestionState::Open
            && before(&question.created_at, cutoff)
            && candidates.contains(&("asks".into(), question.ask_id.clone()))
        {
            candidates.insert(("questions".into(), id.clone()));
        }
    }
    for (id, delivery) in &state.deliveries {
        if delivery.state == DeliveryState::Acknowledged
            && before(&delivery.updated_at, cutoff)
            && candidates.contains(&("asks".into(), delivery.ask_id.clone()))
        {
            candidates.insert(("deliveries".into(), id.clone()));
        }
    }
    for (id, event) in &state.events {
        if event.acknowledged
            && !event.compaction_reminder_pending
            && before(&event.created_at, cutoff)
            && !event.delivery_ids.is_empty()
            && event
                .delivery_ids
                .iter()
                .all(|id| candidates.contains(&("deliveries".into(), id.clone())))
        {
            candidates.insert(("events".into(), id.clone()));
        }
    }
    for (id, artifact) in &state.artifacts {
        if before(&artifact.created_at, cutoff)
            && artifact
                .asks
                .iter()
                .all(|id| candidates.contains(&("asks".into(), id.clone())))
        {
            candidates.insert(("artifacts".into(), id.clone()));
        }
    }

    // Record identities are namespaced on disk. If an imported malformed
    // store reuses an ID across kinds, fail closed instead of choosing one.
    let mut identities = BTreeMap::new();
    for key in rows.keys().filter(|(kind, _)| {
        matches!(
            kind.as_str(),
            "asks" | "answers" | "questions" | "deliveries" | "events" | "artifacts"
        )
    }) {
        anyhow::ensure!(
            identities.insert(key.1.clone(), key.clone()).is_none(),
            "ambiguous ask archive identity"
        );
    }
    let mut edges: BTreeMap<RowKey, BTreeSet<RowKey>> = BTreeMap::new();
    for (key, bytes) in &rows {
        if key.0 == "actions" || key.0 == "artifacts"
            || (key.0 == "state" && matches!(key.1.as_str(), "lineage" | "routing")) {
            // These retained indices contain historical identity/stop/audit
            // references, not runnable work. Open asks already protect their
            // authenticated parent chains; completed indices must not pin all
            // closed history in hot storage forever.
            continue;
        }
        let value: Value = serde_json::from_slice(bytes)?;
        let targets = edges.entry(key.clone()).or_default();
        strings(&value, &mut |value| {
            if let Some(target) = identities.get(value) {
                targets.insert(target.clone());
            }
        });
    }
    // A retained ask needs its children even though only children name the
    // parent. This also keeps newer answers and unanswered questions hot.
    for (id, answer) in &state.answers {
        edges
            .entry(("asks".into(), answer.ask_id.clone()))
            .or_default()
            .insert(("answers".into(), id.clone()));
    }
    for (id, question) in &state.questions {
        edges
            .entry(("asks".into(), question.ask_id.clone()))
            .or_default()
            .insert(("questions".into(), id.clone()));
    }
    for (id, delivery) in &state.deliveries {
        edges
            .entry(("asks".into(), delivery.ask_id.clone()))
            .or_default()
            .insert(("deliveries".into(), id.clone()));
    }
    // Artifact.asks is an ACL reverse association: a retained ask protects
    // its artifacts, not every closed ask that happens to share one blob.
    for (id, artifact) in &state.artifacts {
        for ask in &artifact.asks {
            edges
                .entry(("asks".into(), ask.clone()))
                .or_default()
                .insert(("artifacts".into(), id.clone()));
        }
    }
    let mut protected: BTreeSet<RowKey> = rows
        .keys()
        .filter(|key| !candidates.contains(*key) && key.0 != "actions")
        .cloned()
        .collect();
    protected.extend(sidecar.iter().map(|id| ("asks".into(), id.clone())));
    let mut pending: VecDeque<_> = protected.iter().cloned().collect();
    while let Some(key) = pending.pop_front() {
        if let Some(targets) = edges.get(&key) {
            for target in targets {
                if protected.insert(target.clone()) {
                    pending.push_back(target.clone());
                }
            }
        }
    }
    candidates.retain(|key| !protected.contains(key));
    Ok(candidates)
}

fn open(path: &Path) -> anyhow::Result<Connection> {
    // Never creates a store. The ordinary, verified SQLite migration owns it.
    let db = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.pragma_update(None, "foreign_keys", true)?;
    db.pragma_update(None, "synchronous", "FULL")?;
    Ok(db)
}

fn schema(db: &Connection) -> anyhow::Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS asks_archive_meta (
        singleton INTEGER PRIMARY KEY CHECK(singleton=1), schema_version INTEGER NOT NULL,
        cutoff TEXT NOT NULL, active INTEGER NOT NULL CHECK(active IN(0,1)));
        CREATE TABLE IF NOT EXISTS asks_archive_rows (
        collection TEXT NOT NULL, row_key TEXT NOT NULL, value_json BLOB NOT NULL,
        sha256 TEXT NOT NULL, archived_at TEXT NOT NULL, ask_group TEXT,
        PRIMARY KEY(collection,row_key)) WITHOUT ROWID;
        CREATE INDEX IF NOT EXISTS asks_archive_group ON asks_archive_rows(ask_group);
        CREATE INDEX IF NOT EXISTS asks_archive_identity ON asks_archive_rows(row_key);
        CREATE INDEX IF NOT EXISTS asks_archive_action ON asks_archive_rows(json_extract(CAST(value_json AS TEXT),'$.actionId')) WHERE collection='asks';")?;
    Ok(())
}

pub fn initialize_empty_policy(path: &Path, cutoff: DateTime<Utc>) -> anyhow::Result<()> {
    let mut db = open(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    schema(&tx)?;
    tx.execute("INSERT INTO asks_archive_meta(singleton,schema_version,cutoff,active) VALUES(1,?1,?2,1) ON CONFLICT(singleton) DO NOTHING",
        params![ARCHIVE_SCHEMA,super::now_rfc3339(cutoff)])?;
    let value: String = tx.query_row(
        "SELECT cutoff FROM asks_archive_meta WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    anyhow::ensure!(
        super::parse_time(&value) == Some(cutoff),
        "ask archive cutoff changed"
    );
    tx.commit()?;
    Ok(())
}

/// Explicit offline configuration, never a restart-time moving default.
/// Existing cold records are preserved; this controls subsequent moves only.
pub fn configure_cutoff(path: &Path, cutoff: DateTime<Utc>) -> anyhow::Result<()> {
    let mut db = open(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    schema(&tx)?;
    let version: Option<i64> = tx
        .query_row(
            "SELECT schema_version FROM asks_archive_meta WHERE singleton=1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    anyhow::ensure!(
        version.is_none_or(|version| version == ARCHIVE_SCHEMA),
        "unsupported archive policy schema"
    );
    tx.execute(
        "INSERT INTO asks_archive_meta(singleton,schema_version,cutoff,active) VALUES(1,?1,?2,1)
        ON CONFLICT(singleton) DO UPDATE SET cutoff=excluded.cutoff",
        params![ARCHIVE_SCHEMA, super::now_rfc3339(cutoff)],
    )?;
    tx.commit()?;
    Ok(())
}

fn generation(db: &Connection, expected: u64) -> anyhow::Result<i64> {
    let (format, current, verified): (i64, i64, bool) = db.query_row(
        "SELECT format_version,generation,verified FROM hot_store_meta WHERE store='asks'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    anyhow::ensure!(
        verified && (format == 1 || format == FORMAT_VERSION),
        "ask archive requires verified compatible hot store"
    );
    anyhow::ensure!(
        u64::try_from(current)? == expected,
        "ask archive generation changed"
    );
    Ok(current)
}

fn bump(db: &Connection, current: i64, format: i64) -> anyhow::Result<u64> {
    let next = current
        .checked_add(1)
        .context("ask archive generation overflow")?;
    // Storage membership changed, not individual ask/answer revisions.
    let bytes: Option<Vec<u8>> = db
        .query_row(
            "SELECT value_json FROM hot_store_rows
        WHERE store='asks' AND collection='state' AND row_key='revision'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let revision: u64 = bytes
        .as_deref()
        .map(serde_json::from_slice)
        .transpose()?
        .unwrap_or(0);
    let revision = revision
        .checked_add(1)
        .context("ask archive revision overflow")?;
    db.execute("INSERT INTO hot_store_rows(store,collection,row_key,value_json) VALUES('asks','state','revision',?1)
        ON CONFLICT(store,collection,row_key) DO UPDATE SET value_json=excluded.value_json", [serde_json::to_vec(&revision)?])?;
    db.execute(
        "UPDATE hot_store_meta SET generation=?1,format_version=?2 WHERE store='asks'",
        params![next, format],
    )?;
    Ok(u64::try_from(next)?)
}

/// Copy-verify-remove in ONE database transaction. Old binaries fail closed
/// on format 2 until an explicit stopped-service restore has completed.
pub fn move_rows(
    path: &Path,
    expected: u64,
    keys: &BTreeSet<RowKey>,
    cutoff: DateTime<Utc>,
    now: DateTime<Utc>,
) -> anyhow::Result<MoveReport> {
    let mut db = open(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current = generation(&tx, expected)?;
    schema(&tx)?;
    let persisted: Option<(i64, String)> = tx
        .query_row(
            "SELECT schema_version,cutoff FROM asks_archive_meta WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((version, value)) = persisted {
        anyhow::ensure!(
            version == ARCHIVE_SCHEMA && super::parse_time(&value) == Some(cutoff),
            "ask archive cutoff/schema mismatch; explicit reconfiguration required"
        );
    } else {
        tx.execute("INSERT INTO asks_archive_meta(singleton,schema_version,cutoff,active) VALUES(1,?1,?2,1)",
            params![ARCHIVE_SCHEMA, super::now_rfc3339(cutoff)])?;
    }
    let mut report = MoveReport {
        generation: expected,
        ..Default::default()
    };
    for (kind, key) in keys {
        anyhow::ensure!(
            matches!(
                kind.as_str(),
                "asks" | "answers" | "questions" | "deliveries" | "events" | "artifacts"
            ),
            "unsupported archive collection"
        );
        let bytes: Vec<u8> = tx.query_row("SELECT value_json FROM hot_store_rows WHERE store='asks' AND collection=?1 AND row_key=?2",
            params![kind,key], |r| r.get(0))?;
        let existing: Option<Vec<u8>> = tx
            .query_row(
                "SELECT value_json FROM asks_archive_rows WHERE collection=?1 AND row_key=?2",
                params![kind, key],
                |r| r.get(0),
            )
            .optional()?;
        anyhow::ensure!(
            existing.is_none(),
            "ask archive identity already exists; refusing overwrite"
        );
        let value: Value = serde_json::from_slice(&bytes)?;
        let ask_group = if kind == "asks" {
            Some(key.as_str())
        } else {
            value.get("askId").and_then(Value::as_str)
        };
        tx.execute("INSERT INTO asks_archive_rows(collection,row_key,value_json,sha256,archived_at,ask_group) VALUES(?1,?2,?3,?4,?5,?6)",
            params![kind,key,bytes,super::sha256_hex(&bytes),super::now_rfc3339(now),ask_group])?;
        // SQL insertion succeeded in this same transaction; original bytes
        // remain permanent in archive, never a time-based deletion target.
        tx.execute(
            "DELETE FROM hot_store_rows WHERE store='asks' AND collection=?1 AND row_key=?2",
            params![kind, key],
        )?;
        report.rows += 1;
        report.bytes += bytes.len();
        report.asks += usize::from(kind == "asks");
    }
    if report.rows != 0 {
        report.generation = bump(&tx, current, FORMAT_VERSION)?;
        tx.execute(
            "UPDATE asks_archive_meta SET active=1 WHERE singleton=1",
            [],
        )?;
    }
    tx.commit()?;
    Ok(report)
}

/// Complete history snapshot for explicit history/detail paths only. One
/// read transaction prevents a concurrent move from dropping/duplicating a row.
pub fn history_rows(path: &Path) -> anyhow::Result<Rows> {
    let db = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    db.busy_timeout(Duration::from_secs(5))?;
    let tx = db.unchecked_transaction()?;
    let mut result = Rows::new();
    {
        let mut query = tx.prepare(
            "SELECT collection,row_key,value_json FROM hot_store_rows WHERE store='asks'",
        )?;
        for row in query.query_map([], |r| {
            Ok((
                (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
                r.get::<_, Vec<u8>>(2)?,
            ))
        })? {
            let (key, bytes) = row?;
            result.insert(key, bytes);
        }
    }
    if tx
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='asks_archive_rows'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some()
    {
        let mut query =
            tx.prepare("SELECT collection,row_key,value_json,sha256 FROM asks_archive_rows")?;
        for row in query.query_map([], |r| {
            Ok((
                (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, String>(3)?,
            ))
        })? {
            let (key, bytes, digest) = row?;
            anyhow::ensure!(
                digest == super::sha256_hex(&bytes),
                "ask archive digest mismatch"
            );
            anyhow::ensure!(
                result.insert(key, bytes).is_none(),
                "ask exists in both hot and archive; refusing ambiguity"
            );
        }
    }
    tx.commit()?;
    Ok(result)
}

/// Cheap namespace inventory for short-ID collision detection. It contains
/// no brief/answer text; the existing resolver still applies visibility.
pub fn identities(path: &Path) -> anyhow::Result<Vec<RowKey>> {
    let db = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    db.busy_timeout(Duration::from_secs(5))?;
    let mut query = db.prepare("SELECT collection,row_key FROM asks_archive_rows")?;
    let rows = query
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Reference accounting must observe both locations in one SQL snapshot.
/// Separate hot and cold reads can miss a concurrent promotion entirely.
pub fn all_ask_ids(path: &Path) -> anyhow::Result<BTreeSet<String>> {
    let db = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    db.busy_timeout(Duration::from_secs(5))?;
    let mut query=db.prepare("SELECT row_key FROM hot_store_rows WHERE store='asks' AND collection='asks' UNION SELECT row_key FROM asks_archive_rows WHERE collection='asks'")?;
    let result = query
        .query_map([], |r| r.get(0))?
        .collect::<Result<BTreeSet<_>, _>>()?;
    Ok(result)
}

pub fn list_asks(path: &Path) -> anyhow::Result<Vec<(super::model::AskRecord, String)>> {
    let db = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    db.busy_timeout(Duration::from_secs(5))?;
    let mut query = db.prepare(
        "SELECT value_json,sha256,archived_at FROM asks_archive_rows WHERE collection='asks'",
    )?;
    let mut result = Vec::new();
    for row in query.query_map([], |r| {
        Ok((
            r.get::<_, Vec<u8>>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (bytes, digest, archived_at) = row?;
        anyhow::ensure!(
            digest == super::sha256_hex(&bytes),
            "archived ask digest mismatch"
        );
        result.push((serde_json::from_slice(&bytes)?, archived_at));
    }
    Ok(result)
}

pub fn record<T: DeserializeOwned>(path: &Path, kind: &str, id: &str) -> anyhow::Result<Option<T>> {
    let db = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    db.busy_timeout(Duration::from_secs(5))?;
    let row: Option<(Vec<u8>, String)> = db
        .query_row(
            "SELECT value_json,sha256 FROM asks_archive_rows WHERE collection=?1 AND row_key=?2",
            params![kind, id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    row.map(|(bytes, digest)| {
        anyhow::ensure!(
            digest == super::sha256_hex(&bytes),
            "archived record digest mismatch"
        );
        serde_json::from_slice(&bytes).map_err(Into::into)
    })
    .transpose()
}

pub fn archived_at(path: &Path, id: &str) -> anyhow::Result<Option<String>> {
    let db = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    db.busy_timeout(Duration::from_secs(5))?;
    Ok(db
        .query_row(
            "SELECT archived_at FROM asks_archive_rows WHERE collection='asks' AND row_key=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn records<T: DeserializeOwned>(
    path: &Path,
    kind: &str,
    ids: &[&str],
) -> anyhow::Result<Vec<T>> {
    let db = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    db.busy_timeout(Duration::from_secs(5))?;
    let mut query = db.prepare(
        "SELECT value_json,sha256 FROM asks_archive_rows WHERE collection=?1 AND row_key=?2",
    )?;
    let mut result = Vec::new();
    for id in ids {
        let row: Option<(Vec<u8>, String)> = query
            .query_row(params![kind, id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        if let Some((bytes, digest)) = row {
            anyhow::ensure!(
                digest == super::sha256_hex(&bytes),
                "archived record digest mismatch"
            );
            result.push(serde_json::from_slice(&bytes)?);
        }
    }
    Ok(result)
}

pub fn find_by_action(
    path: &Path,
    requester: &super::model::Requester,
    action: &str,
) -> anyhow::Result<Option<super::model::AskRecord>> {
    let db = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    db.busy_timeout(Duration::from_secs(5))?;
    let mut query=db.prepare("SELECT value_json,sha256 FROM asks_archive_rows WHERE collection='asks' AND json_extract(CAST(value_json AS TEXT),'$.actionId')=?1")?;
    for row in query.query_map([action], |r| {
        Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, String>(1)?))
    })? {
        let (bytes, digest) = row?;
        anyhow::ensure!(
            digest == super::sha256_hex(&bytes),
            "archived ask digest mismatch"
        );
        let ask: super::model::AskRecord = serde_json::from_slice(&bytes)?;
        if crate::peers::asks::same_requester(&ask.requester, requester) {
            return Ok(Some(ask));
        }
    }
    Ok(None)
}

/// Read just the cold connected components named by an explicit resource
/// argument. Do not rebuild all historical answers to resolve an agent name.
pub fn matching_rows(path: &Path, inputs: &[&str]) -> anyhow::Result<Rows> {
    let mut seeds = VecDeque::new();
    for (kind, id) in identities(path)? {
        if inputs.iter().any(|input| identity_matches(input, &id)) {
            if kind == "artifacts" {
                if let Some(artifact) =
                    record::<super::model::ArtifactRecord>(path, "artifacts", &id)?
                {
                    seeds.extend(artifact.asks);
                }
            }
            seeds.push_back(id);
        }
    }
    let db = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    db.busy_timeout(Duration::from_secs(5))?;
    let tx = db.unchecked_transaction()?;
    let rows = connected_rows(&tx, seeds, false)?;
    tx.commit()?;
    Ok(rows)
}

pub(crate) fn identity_matches(input: &str, id: &str) -> bool {
    if input == id {
        return true;
    }
    let candidate = crate::short_ids::query(id);
    let q = crate::short_ids::query(input);
    q.valid_hex
        && !q.hex.is_empty()
        && q.kind.is_none_or(|kind| candidate.kind == Some(kind))
        && ((!q.suffix_only && candidate.hex.starts_with(&q.hex))
            || candidate.hex.ends_with(&q.hex))
}

fn connected_rows(
    db: &Connection,
    mut pending: VecDeque<String>,
    artifact_owners: bool,
) -> anyhow::Result<Rows> {
    let mut visited = BTreeSet::new();
    let mut selected = Rows::new();
    while let Some(id) = pending.pop_front() {
        if !visited.insert(id.clone()) {
            continue;
        }
        let mut query=db.prepare("SELECT collection,row_key,value_json,sha256 FROM asks_archive_rows WHERE row_key=?1 OR ask_group=?1")?;
        for row in query.query_map([&id], |r| {
            Ok((
                (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, String>(3)?,
            ))
        })? {
            let (key, bytes, digest) = row?;
            anyhow::ensure!(
                digest == super::sha256_hex(&bytes),
                "ask archive digest mismatch; lookup refused"
            );
            let value: Value = serde_json::from_slice(&bytes)?;
            // Artifact.asks is a reverse ACL association, not a scheduling
            // dependency: uploading a common "Done" must not promote every
            // historical delegation which shares that content hash.
            if artifact_owners || key.0 != "artifacts" {
                strings(&value, &mut |value| {
                    let q = crate::short_ids::query(value);
                    if q.valid_hex && q.kind.is_some() && !visited.contains(value) {
                        pending.push_back(value.to_owned());
                    }
                });
            }
            selected.insert(key, bytes);
        }
    }
    Ok(selected)
}

/// Reverse the split without discarding writes made since activation.
/// Caller must prove every cutex writer sharing this home is stopped.
pub fn restore_all(path: &Path, expected: u64, daemon_stopped: bool) -> anyhow::Result<MoveReport> {
    anyhow::ensure!(
        daemon_stopped,
        "stop every cutex writer before restoring archived asks for r261/r260 rollback"
    );
    let mut db = open(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current = generation(&tx, expected)?;
    schema(&tx)?;
    let mut rows = Vec::new();
    {
        let mut query = tx.prepare("SELECT collection,row_key,value_json,sha256 FROM asks_archive_rows ORDER BY collection,row_key")?;
        for row in query.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, String>(3)?,
            ))
        })? {
            rows.push(row?);
        }
    }
    let mut report = MoveReport {
        generation: expected,
        ..Default::default()
    };
    for (kind, key, bytes, digest) in rows {
        anyhow::ensure!(
            digest == super::sha256_hex(&bytes),
            "ask archive digest mismatch; rollback refused"
        );
        let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM hot_store_rows WHERE store='asks' AND collection=?1 AND row_key=?2)", params![kind,key], |r| r.get(0))?;
        anyhow::ensure!(
            !exists,
            "hot/archive identity conflict; rollback refused without modifying data"
        );
        tx.execute("INSERT INTO hot_store_rows(store,collection,row_key,value_json) VALUES('asks',?1,?2,?3)", params![kind,key,bytes])?;
        report.rows += 1;
        report.bytes += bytes.len();
        report.asks += usize::from(kind == "asks");
    }
    let format: i64 = tx.query_row(
        "SELECT format_version FROM hot_store_meta WHERE store='asks'",
        [],
        |r| r.get(0),
    )?;
    if report.rows != 0 || format == FORMAT_VERSION {
        // All byte digests and collisions were checked within this transaction.
        tx.execute("DELETE FROM asks_archive_rows", [])?;
        report.generation = bump(&tx, current, 1)?;
    }
    tx.execute(
        "UPDATE asks_archive_meta SET active=0 WHERE singleton=1",
        [],
    )?;
    tx.commit()?;
    Ok(report)
}

/// Promote a connected historical record before mutating it. The group
/// index finds its answers/questions/deliveries; reference traversal brings
/// back shared artifacts and related archived asks, never a partial AskState.
pub fn promote(path: &Path, expected: u64, identity: &str) -> anyhow::Result<MoveReport> {
    promote_many(path, expected, &[identity])
}

/// One CAS/commit for the complete selected component set. Rebuilding a
/// progressively larger hot state once per identity would be quadratic.
pub fn promote_many(path: &Path, expected: u64, identities: &[&str]) -> anyhow::Result<MoveReport> {
    let mut db = open(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current = generation(&tx, expected)?;
    let selected = connected_rows(
        &tx,
        identities.iter().map(|id| (*id).to_owned()).collect(),
        false,
    )?;
    let mut report = MoveReport {
        generation: expected,
        ..Default::default()
    };
    for ((kind, key), bytes) in selected {
        tx.execute("INSERT INTO hot_store_rows(store,collection,row_key,value_json) VALUES('asks',?1,?2,?3)",params![kind,key,bytes])
            .context("hot/archive identity conflict; promotion refused")?;
        tx.execute(
            "DELETE FROM asks_archive_rows WHERE collection=?1 AND row_key=?2",
            params![kind, key],
        )?;
        report.rows += 1;
        report.bytes += bytes.len();
        report.asks += usize::from(kind == "asks");
    }
    if report.rows != 0 {
        report.generation = bump(&tx, current, FORMAT_VERSION)?;
    }
    tx.commit()?;
    Ok(report)
}

#[cfg(test)]
mod tests;
