//! One-time load migration `session-cwd-normalize`: session records whose
//! `cwd` / `managedCwd` was stored with a non-normalized Windows spelling
//! (seen on a Windows host: a JSON-escaped `E:\\Projects (Orchid)\\waveline-backend`
//! with two literal backslashes per separator) get the spelling of
//! [`normalize_session_cwd`]. Input paths normalize since this release, so
//! nothing new is written that needs it; the migration repairs what is
//! already stored.
//!
//! - A value changes only when its normalized spelling differs AND (the
//!   normalized path exists OR the stored one does not), so a path is never
//!   moved away from a directory that exists to one that does not.
//! - The launch cwd's default bus group `project:<path key>` (7 hex digits
//!   of the cwd hash, `runtime_agent_registration`) is renamed to the key of
//!   the normalized cwd, so the session joins the group its siblings in the
//!   same directory are in. Other groups are left alone.
//! - Changed records get a new durable revision and `updated_at`; the
//!   Management v2 session projection is derived from the record and follows
//!   on its next read.
//! - It runs at the start of `cutex management serve` and of the agent bus,
//!   under the session store lock. A start with nothing to change writes
//!   nothing, so it is idempotent; every run that changed something is
//!   appended to `archive/session-cwd-normalize/migration.json` BEFORE the
//!   store commit (old and new values: the backup), and each change is
//!   logged to stderr as `session <name>: <field> <old> -> <new>`.
//! - Errors never fail startup; the migration is retried on the next start.
//!
//! No read compatibility is kept for older releases (they read the
//! normalized values as ordinary paths).

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;

use anyhow::Context;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::agent_bus::identity::fnv1a_hex;
use crate::session::metadata::cutex_session_presented_name;
use crate::session::metadata::cutex_session_launch_cwd;
use crate::session::metadata::normalize_session_cwd;
use crate::session::model::CutexSessionRecord;
use crate::session::model::CutexSessionStore;

pub const RECORD_SCHEMA: &str = "cutex/session-cwd-normalize-migration/v1";
/// Record directory, relative to the cutex dir.
const ARCHIVE_ROOT: &str = "archive/session-cwd-normalize";
const LOCK_FILE: &str = ".migration.lock";
const RECORD_FILE: &str = "migration.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValueChange<T> {
    pub old: T,
    pub new: T,
}

/// One changed session, as recorded in `migration.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCwdChange {
    pub key: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<ValueChange<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_cwd: Option<ValueChange<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub groups: Option<ValueChange<Vec<String>>>,
    /// Durable revision and `updated_at` before the change.
    pub previous_revision: u64,
    pub previous_updated_at: String,
}

#[derive(Serialize, Deserialize)]
struct MigrationRecord {
    schema: String,
    /// Raw JSON, so runs written by a later release keep their fields.
    runs: Vec<serde_json::Value>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MigrationRun<'a> {
    at: &'a str,
    component: &'a str,
    /// False only when the store commit after writing this run failed (the
    /// next start plans again).
    committed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    sessions: &'a [SessionCwdChange],
}

/// Daemon entry points call this. Never fails or panics.
pub fn run_on_startup(component: &str) {
    let result = std::panic::catch_unwind(|| {
        let cutex_dir = crate::config::paths::config_dir()?;
        let store = crate::session::store::cutex_sessions_path()?;
        run_at(&cutex_dir, &store, component, Utc::now())
    });
    match result {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => eprintln!(
            "cutex session cwd migration: {component}: not run on this start: {error:#}"
        ),
        Err(_) => {
            eprintln!("cutex session cwd migration: {component}: panicked, not run on this start")
        }
    }
}

/// Normalizes the store at `store_path` (`cutex-sessions.json`, the logical
/// path of the SQLite store) of the cutex dir `cutex_dir`. Returns the
/// committed changes (empty when there was nothing to do).
pub fn run_at(
    cutex_dir: &Path,
    store_path: &Path,
    component: &str,
    now: DateTime<Utc>,
) -> anyhow::Result<Vec<SessionCwdChange>> {
    // Almost every start has nothing to do: check on the shared snapshot
    // before taking any lock or creating anything.
    let pending = crate::session::store::with_cutex_session_store_at(store_path, |store| {
        !plan(store).is_empty()
    })?;
    if !pending {
        return Ok(Vec::new());
    }
    let archive = cutex_dir.join(ARCHIVE_ROOT);
    crate::management::client_tokens::ensure_private_dir(&cutex_dir.join("archive"))?;
    crate::management::client_tokens::ensure_private_dir(&archive)?;
    let _lock = lock_exclusive(&archive.join(LOCK_FILE))?;
    let record_path = archive.join(RECORD_FILE);
    // An unreadable record stops the migration before anything changes.
    let mut record = read_record(&record_path)?;
    let at = now.to_rfc3339_opts(SecondsFormat::Secs, true);
    crate::session::store::with_locked_session_store(store_path, |store| {
        // Re-planned under the store lock: a concurrent start may have done it.
        let changes = plan(store);
        if changes.is_empty() {
            return Ok(Vec::new());
        }
        apply(store, &changes, &at)?;
        let run = |committed: bool, error: Option<String>| {
            serde_json::to_value(MigrationRun {
                at: &at,
                component,
                committed,
                error,
                sessions: &changes,
            })
        };
        record.runs.push(run(true, None)?);
        write_record(&record_path, &record)?;
        if let Err(error) = crate::session::store::save_locked_session_store(store_path, store) {
            let message = format!("{error:#}");
            *record.runs.last_mut().expect("run just pushed") = run(false, Some(message))?;
            let _ = write_record(&record_path, &record);
            return Err(error);
        }
        for change in &changes {
            log_change(change);
        }
        Ok(changes)
    })
}

/// The changes [`run_at`] would make to `store` (no writes; reads the
/// filesystem only to check which spellings exist).
pub fn plan(store: &CutexSessionStore) -> Vec<SessionCwdChange> {
    store
        .sessions
        .iter()
        .filter_map(|(key, record)| plan_record(key, record))
        .collect()
}

fn plan_record(key: &str, record: &CutexSessionRecord) -> Option<SessionCwdChange> {
    let cwd = normalized_change(&record.cwd);
    let managed_cwd = record.managed_cwd.as_deref().and_then(normalized_change);
    if cwd.is_none() && managed_cwd.is_none() {
        return None;
    }
    let mut next = record.clone();
    if let Some(change) = &cwd {
        next.cwd = change.new.clone();
    }
    if let Some(change) = &managed_cwd {
        next.managed_cwd = Some(change.new.clone());
    }
    let groups = renamed_default_group(
        &record.agent_groups,
        cutex_session_launch_cwd(record),
        cutex_session_launch_cwd(&next),
    );
    Some(SessionCwdChange {
        key: key.to_string(),
        name: cutex_session_presented_name(record),
        cwd,
        managed_cwd,
        groups,
        previous_revision: record.durable_revision(),
        previous_updated_at: record.updated_at.clone(),
    })
}

/// `Some` when `raw` normalizes differently and the change is safe: the
/// normalized path exists, or the stored one does not either.
fn normalized_change(raw: &str) -> Option<ValueChange<String>> {
    let normalized = normalize_session_cwd(raw);
    if normalized == raw {
        return None;
    }
    if !Path::new(&normalized).exists() && Path::new(raw).exists() {
        return None;
    }
    Some(ValueChange {
        old: raw.to_string(),
        new: normalized,
    })
}

fn default_group(launch_cwd: &str) -> String {
    format!("project:{}", &fnv1a_hex(launch_cwd)[..7])
}

fn renamed_default_group(
    groups: &[String],
    old_launch_cwd: &str,
    new_launch_cwd: &str,
) -> Option<ValueChange<Vec<String>>> {
    if old_launch_cwd == new_launch_cwd {
        return None;
    }
    let old_group = default_group(old_launch_cwd);
    let new_group = default_group(new_launch_cwd);
    if !groups.contains(&old_group) {
        return None;
    }
    let mut next: Vec<String> = Vec::with_capacity(groups.len());
    for group in groups {
        let group = if *group == old_group { &new_group } else { group };
        if !next.contains(group) {
            next.push(group.clone());
        }
    }
    Some(ValueChange {
        old: groups.to_vec(),
        new: next,
    })
}

fn apply(store: &mut CutexSessionStore, changes: &[SessionCwdChange], at: &str) -> anyhow::Result<()> {
    for change in changes {
        let record = store
            .sessions
            .get_mut(&change.key)
            .context("session disappeared under the store lock")?;
        if let Some(cwd) = &change.cwd {
            record.cwd = cwd.new.clone();
        }
        if let Some(managed_cwd) = &change.managed_cwd {
            record.managed_cwd = Some(managed_cwd.new.clone());
        }
        if let Some(groups) = &change.groups {
            record.agent_groups = groups.new.clone();
        }
        record.bump_durable_revision()?;
        record.updated_at = at.to_string();
    }
    Ok(())
}

fn log_change(change: &SessionCwdChange) {
    let name = &change.name;
    if let Some(cwd) = &change.cwd {
        eprintln!("cutex session cwd migration: session {name}: cwd {} -> {}", cwd.old, cwd.new);
    }
    if let Some(cwd) = &change.managed_cwd {
        eprintln!(
            "cutex session cwd migration: session {name}: managedCwd {} -> {}",
            cwd.old, cwd.new
        );
    }
    if let Some(groups) = &change.groups {
        eprintln!(
            "cutex session cwd migration: session {name}: groups {:?} -> {:?}",
            groups.old, groups.new
        );
    }
}

fn read_record(path: &Path) -> anyhow::Result<MigrationRecord> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(MigrationRecord {
                schema: RECORD_SCHEMA.to_owned(),
                runs: Vec::new(),
            })
        }
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let record: MigrationRecord =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    anyhow::ensure!(
        record.schema == RECORD_SCHEMA,
        "unexpected schema {:?} in {}",
        record.schema,
        path.display()
    );
    Ok(record)
}

fn write_record(path: &Path, record: &MigrationRecord) -> anyhow::Result<()> {
    crate::config::atomic::write_private_pretty_json_atomic(
        path,
        record,
        "session cwd migration record",
    )
}

fn lock_exclusive(path: &Path) -> anyhow::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let lock = options
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    fs2::FileExt::lock_exclusive(&lock).with_context(|| format!("lock {}", path.display()))?;
    Ok(lock)
}

#[cfg(test)]
#[path = "cwd_migration_tests.rs"]
mod tests;
