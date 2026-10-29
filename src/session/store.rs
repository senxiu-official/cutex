//! Durable cutex session store persistence.

use std::fmt;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use fs2::FileExt;

use crate::config::paths::config_dir;
use crate::session::model::CutexSessionStore;

mod rows;
mod sqlite;

pub use rows::compare_and_swap_session_row;
pub use rows::load_session_row;
pub use rows::update_session_row;
pub use rows::with_locked_session_rows;
pub use rows::SessionRowConflict;
pub use rows::SessionRows;
pub(crate) use rows::update_receipt_row;
// Every receipt map, for row-level writers (not all have one yet).
#[allow(unused_imports)]
pub(crate) use rows::{
    AgentArchiveReceipts, ExplicitLaunchReceipts, ExplicitLaunchTombstones, FormalNameReceipts,
    HumanAdoptionReceipts, NativeStopReceipts,
};

pub use sqlite::is_sentinel as is_session_store_sentinel;
pub use sqlite::OpenOutcome as SessionStoreOpenOutcome;
pub use sqlite::SENTINEL_MARKER as SESSION_STORE_SENTINEL_MARKER;
pub use sqlite::WRITTEN_ROW_BYTES as SESSION_STORE_WRITTEN_ROW_BYTES;

const CUTEX_SESSIONS_LOCK_FILE: &str = "cutex-sessions.lock";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CutexSessionStoreRevisionConflict {
    pub expected: u64,
    pub actual: u64,
}

impl fmt::Display for CutexSessionStoreRevisionConflict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "cutex session store revision conflict: expected {}, current {}; reload before retrying",
            self.expected, self.actual
        )
    }
}

impl std::error::Error for CutexSessionStoreRevisionConflict {}

/// The logical store path. Since session store Phase 2 the records live in
/// the SQLite database next to it ([`session_database_path`]); this path
/// holds the sentinel that keeps older cutex off an imported home (see
/// `sqlite.rs`). Callers keep passing this path to the store functions.
pub fn cutex_sessions_path() -> anyhow::Result<PathBuf> {
    Ok(config_dir()?.join("cutex-sessions.json"))
}

/// The SQLite database behind the logical store path `path`
/// (`cutex-sessions.json` -> `cutex-sessions.sqlite3`).
pub fn session_database_path(path: &Path) -> PathBuf {
    sqlite::database_path(path)
}

/// The default store at its latest revision. Returns an owned copy: callers
/// that only read should use [`load_cutex_session_store_shared`] or
/// [`with_cutex_session_store`], which do not copy the whole store.
pub fn load_cutex_session_store() -> anyhow::Result<CutexSessionStore> {
    with_cutex_session_store(|store| {
        note_whole_store_copy();
        store.clone()
    })
}

static WHOLE_STORE_COPIES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
thread_local! {
    static THREAD_WHOLE_STORE_COPIES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn note_whole_store_copy() {
    WHOLE_STORE_COPIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    #[cfg(test)]
    THREAD_WHOLE_STORE_COPIES.with(|count| count.set(count.get() + 1));
}

/// Whole-store copies made on the calling thread (tests run in parallel, so
/// assertions use this instead of the process-wide count).
#[cfg(test)]
pub(crate) fn whole_store_copies_on_this_thread() -> u64 {
    THREAD_WHOLE_STORE_COPIES.with(|count| count.get())
}

/// How many owned whole-store copies ([`load_cutex_session_store`],
/// [`load_cutex_session_store_from_path`]) this process made so far. A cheap
/// diagnostic counter for performance tests: hot read paths should stay on
/// the shared snapshot and leave it unchanged.
pub fn whole_store_copy_count() -> u64 {
    WHOLE_STORE_COPIES.load(std::sync::atomic::Ordering::Relaxed)
}

/// The default store as a shared, read-only snapshot (no copy). Hot read
/// paths that poll (Agent Bus completion fences, notification states) use
/// this: cloning the parsed store cost ~100 ms per call on the Linux host with a
/// 35 MB file (2026-10-09).
pub fn load_cutex_session_store_shared() -> anyhow::Result<Arc<CutexSessionStore>> {
    sqlite::shared(&cutex_sessions_path()?)
}

/// Runs `read` on the default store without copying it. Each process keeps
/// one snapshot per database and brings it up to date by parsing only the
/// rows written since (every commit stamps its rows with the new store
/// revision); an unchanged store costs one small query. `read` runs on a
/// shared snapshot outside every lock, so nested reads and writes from
/// inside `read` are fine.
pub fn with_cutex_session_store<T>(read: impl FnOnce(&CutexSessionStore) -> T) -> anyhow::Result<T> {
    let path = cutex_sessions_path()?;
    with_cutex_session_store_at(&path, read)
}

/// [`with_cutex_session_store`] for an explicit store path. Read-only
/// callers that poll, such as ingress fences and bridge workers, use this.
pub(crate) fn with_cutex_session_store_at<T>(
    path: &Path,
    read: impl FnOnce(&CutexSessionStore) -> T,
) -> anyhow::Result<T> {
    let store = sqlite::shared(path)?;
    Ok(read(&store))
}

/// One record of the store at `path` (key lookup on the shared snapshot,
/// copying only that record).
pub(crate) fn session_record_at(
    path: &Path,
    key: &str,
) -> anyhow::Result<Option<crate::session::model::CutexSessionRecord>> {
    with_cutex_session_store_at(path, |store| store.sessions.get(key).cloned())
}

#[track_caller]
pub fn save_cutex_session_store(store: &CutexSessionStore) -> anyhow::Result<()> {
    let path = cutex_sessions_path()?;
    save_cutex_session_store_to_path(&path, store)
}

/// An owned copy of the store at `path` (from the same per-process snapshot
/// as [`with_cutex_session_store_at`]). A home without any store reads as
/// empty and nothing is created.
pub(crate) fn load_cutex_session_store_from_path(path: &Path) -> anyhow::Result<CutexSessionStore> {
    let shared = sqlite::shared(path)?;
    note_whole_store_copy();
    Ok((*shared).clone())
}

/// Whole-store compare-and-swap: commits `store` only if the store is still
/// at `store.store_revision` ([`CutexSessionStoreRevisionConflict`]
/// otherwise), writing just the rows that differ from that revision.
#[track_caller]
pub(crate) fn save_cutex_session_store_to_path(
    path: &Path,
    store: &CutexSessionStore,
) -> anyhow::Result<()> {
    let site = std::panic::Location::caller();
    sqlite::ensure_ready(path, true, false)?;
    let parent = path
        .parent()
        .context("cutex session store path has no parent directory")?;
    let lock = open_store_lock(&parent.join(CUTEX_SESSIONS_LOCK_FILE))?;
    FileExt::lock_exclusive(&lock).context("Failed to lock cutex session store")?;
    let result = sqlite::commit_locked(path, store, site);
    let unlock_result = FileExt::unlock(&lock).context("Failed to unlock cutex session store");
    match (result, unlock_result) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

/// Commit one prepared record against its own previous value, preserving
/// unrelated heartbeat updates and receipts in the latest store snapshot.
pub fn save_cutex_session_record(
    before: &CutexSessionStore,
    after: &CutexSessionStore,
    key: &str,
) -> anyhow::Result<()> {
    save_cutex_session_record_to_path(&cutex_sessions_path()?, before, after, key)
}

fn save_cutex_session_record_to_path(
    path: &Path,
    before: &CutexSessionStore,
    after: &CutexSessionStore,
    key: &str,
) -> anyhow::Result<()> {
    let replacement = after
        .sessions
        .get(key)
        .context("prepared session missing")?;
    with_locked_session_rows(path, |rows| {
        anyhow::ensure!(
            serde_json::to_value(rows.session(key))?
                == serde_json::to_value(before.sessions.get(key))?,
            "target session changed while preparing update; reload before retrying"
        );
        for (other_key, other) in rows.sessions() {
            if other_key == key {
                continue;
            }
            anyhow::ensure!(
                replacement.codex_session_id.is_none()
                    || other.codex_session_id != replacement.codex_session_id,
                "native session was concurrently bound to another record"
            );
            // Names are unique among live records only: an archived
            // (retired) Agent keeps its name for history, and the name may
            // be reused once it is archived (r244: recreating a closed
            // Agent's name failed here after the native thread existed).
            // Restoring an archived record checks the name again.
            anyhow::ensure!(
                replacement.formal_agent_name.is_none()
                    || replacement.is_retired()
                    || other.is_retired()
                    || other.formal_agent_name != replacement.formal_agent_name,
                "agent name was concurrently assigned to another record"
            );
        }
        rows.insert_session(key, replacement.clone());
        rows.commit().map(drop)
    })
}

/// Permanently remove one record (Management `cutex/session/delete`). The
/// record must still equal `expected` (same durable revision and runtime
/// generation), otherwise nothing is written. Returns the removed record.
pub fn remove_cutex_session_record(
    expected: &crate::session::model::CutexSessionRecord,
) -> anyhow::Result<crate::session::model::CutexSessionRecord> {
    remove_cutex_session_record_at(&cutex_sessions_path()?, expected)
}

pub(crate) fn remove_cutex_session_record_at(
    path: &Path,
    expected: &crate::session::model::CutexSessionRecord,
) -> anyhow::Result<crate::session::model::CutexSessionRecord> {
    let key = expected.cutex_session_id.clone();
    with_locked_session_rows(path, |rows| {
        let record = rows
            .session(&key)
            .context("cutex session no longer exists")?;
        anyhow::ensure!(
            record.durable_revision() == expected.durable_revision()
                && record.runtime_generation == expected.runtime_generation,
            "cutex session changed while preparing delete; reload before retrying"
        );
        let removed = rows.remove_session(&key).expect("record checked above");
        rows.commit()?;
        Ok(removed)
    })
}

/// The store at `path` as one JSON document in the shape of the pre-SQLite
/// file, sessions sorted by key so equal stores give equal bytes. For tests
/// and diagnostics that compare whole stores (they used to read the file).
pub fn session_store_json_bytes(path: &Path) -> anyhow::Result<Vec<u8>> {
    let store = sqlite::shared(path)?;
    let mut value = serde_json::to_value(&*store)?;
    if let Some(sessions) = value.get_mut("sessions").and_then(|value| value.as_object_mut()) {
        let mut sorted = std::mem::take(sessions).into_iter().collect::<Vec<_>>();
        sorted.sort_by(|left, right| left.0.cmp(&right.0));
        *sessions = sorted.into_iter().collect();
    }
    Ok(serde_json::to_vec(&value)?)
}

/// Cleanup already prepared its read-only evidence outside lifecycle locks.
/// Open the same writer lock file without loading/reparsing the database.
/// The caller acquires it only around the bounded recheck and rename.
pub(crate) fn cleanup_lock_file(path: &Path) -> anyhow::Result<File> {
    let parent=path.parent().context("session store parent")?;
    let file=open_store_lock(&parent.join(CUTEX_SESSIONS_LOCK_FILE))?;
    Ok(file)
}

fn open_store_lock(path: &Path) -> anyhow::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    options.open(path).with_context(|| {
        format!(
            "Failed to open cutex session store lock: {}",
            path.display()
        )
    })
}

/// Holds the same cross-process fence used by ordinary durable session saves.
/// The callback may commit a related provider receipt before releasing it.
/// Lock order: this file lock, then the SQLite write lock (taken only inside
/// [`save_locked_session_store`]); the callback runs outside any database
/// transaction.
pub(crate) fn with_locked_session_store<T>(
    path: &Path,
    operation: impl FnOnce(&mut CutexSessionStore) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let parent = path.parent().context("session store has no parent")?;
    fs::create_dir_all(parent)?;
    // Import / create first: that takes the store lock itself.
    sqlite::ensure_ready(path, true, false)?;
    let lock = open_store_lock(&parent.join(CUTEX_SESSIONS_LOCK_FILE))?;
    FileExt::lock_exclusive(&lock)?;
    let mut store = load_cutex_session_store_from_path(path)?;
    operation(&mut store)
}

/// Holds the store file lock (the cross-process fence of every session store
/// writer) while `operation` runs, without loading the store. For callers
/// that only need the fence, e.g. to commit a related record elsewhere
/// while no session write can interleave.
pub(crate) fn with_session_store_lock<T>(
    path: &Path,
    operation: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let parent = path.parent().context("session store has no parent")?;
    fs::create_dir_all(parent)?;
    sqlite::ensure_ready(path, true, false)?;
    let lock = open_store_lock(&parent.join(CUTEX_SESSIONS_LOCK_FILE))?;
    FileExt::lock_exclusive(&lock)?;
    operation()
}

/// Caller must hold `with_locked_session_store`; never reacquire its lock.
/// Still a compare-and-swap on the store revision (the lock makes a
/// conflict impossible between cutex writers).
#[track_caller]
pub(crate) fn save_locked_session_store(
    path: &Path,
    store: &CutexSessionStore,
) -> anyhow::Result<()> {
    sqlite::commit_locked(path, store, std::panic::Location::caller())
}

/// Persist the deterministic repair of a stale permission alias (see
/// [`crate::runtime::args::inconsistent_permission_alias_repair`]) before a
/// launch, so the stored record matches what the runtime is started with.
/// Returns `(old, new)` alias when the record changed.
pub fn repair_cutex_session_permission_alias(
    key: &str,
) -> anyhow::Result<Option<(String, String)>> {
    repair_cutex_session_permission_alias_at(&cutex_sessions_path()?, key)
}

pub(crate) fn repair_cutex_session_permission_alias_at(
    path: &Path,
    key: &str,
) -> anyhow::Result<Option<(String, String)>> {
    with_locked_session_rows(path, |rows| {
        if rows.session(key).is_none_or(|record| record.is_retired()) {
            return Ok(None);
        }
        let record = rows.session_mut(key).expect("record checked above");
        let Some((old, new)) = crate::runtime::args::repair_inconsistent_permission_alias(record)
        else {
            return Ok(None);
        };
        record.bump_durable_revision()?;
        record.updated_at = chrono::Utc::now().to_rfc3339();
        let sandbox = record.sandbox_mode.clone().unwrap_or_default();
        rows.commit()?;
        eprintln!(
            "cutex: repaired inconsistent permission alias for {key}: {old} -> {new} (explicit sandbox {sandbox} is authoritative)"
        );
        Ok(Some((old, new)))
    })
}

#[cfg(test)]
mod bench;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::model::CutexSessionRecord;

    #[test]
    fn cached_store_follows_commits_of_other_processes() {
        let dir = std::env::temp_dir().join(format!("cutex-session-cache-{}", uuid::Uuid::new_v4()));
        let path = dir.join("cutex-sessions.json");
        let names = |path: &Path| {
            with_cutex_session_store_at(path, |store| {
                let mut names = store.sessions.keys().cloned().collect::<Vec<_>>();
                names.sort();
                names
            })
            .unwrap()
        };
        assert!(names(&path).is_empty(), "missing store reads as empty");
        assert!(!session_database_path(&path).exists(), "a read creates nothing");
        with_locked_session_store(&path, |store| {
            store.sessions.insert("cutex.a".into(), record("a"));
            save_locked_session_store(&path, store)
        })
        .unwrap();
        assert_eq!(names(&path), vec!["cutex.a".to_string()]);
        assert_eq!(names(&path), vec!["cutex.a".to_string()], "cache hit");
        // Another process commits (no in-process cache update): a new row
        // stamped with the next revision, exactly as its commit writes it.
        let connection = rusqlite::Connection::open(session_database_path(&path)).unwrap();
        connection
            .execute(
                "INSERT INTO sessions (key, cutex_session_id, archive_state, rev, record_json) VALUES ('cutex.b', 'cutex.b', 'active', 2, ?1)",
                [serde_json::to_string(&record("b")).unwrap()],
            )
            .unwrap();
        connection
            .execute("UPDATE meta SET value = '2' WHERE key = 'store_revision'", [])
            .unwrap();
        drop(connection);
        assert_eq!(names(&path), vec!["cutex.a".to_string(), "cutex.b".to_string()]);
        // And deletes one.
        let connection = rusqlite::Connection::open(session_database_path(&path)).unwrap();
        connection.execute("DELETE FROM sessions WHERE key = 'cutex.a'", []).unwrap();
        connection
            .execute("UPDATE meta SET value = '3' WHERE key = 'store_revision'", [])
            .unwrap();
        drop(connection);
        assert_eq!(names(&path), vec!["cutex.b".to_string()]);
        assert_eq!(load_cutex_session_store_from_path(&path).unwrap().store_revision.get(), 3);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn shared_snapshots_are_not_copied_and_outlive_a_replacement() {
        let dir = std::env::temp_dir().join(format!("cutex-session-shared-{}", uuid::Uuid::new_v4()));
        let path = dir.join("cutex-sessions.json");
        with_locked_session_store(&path, |store| {
            store.sessions.insert("cutex.a".into(), record("a"));
            save_locked_session_store(&path, store)
        })
        .unwrap();
        let first = sqlite::shared(&path).unwrap();
        let again = sqlite::shared(&path).unwrap();
        assert!(Arc::ptr_eq(&first, &again), "an unchanged store is served from the cache");
        with_locked_session_store(&path, |store| {
            store.sessions.insert("cutex.b".into(), record("b"));
            save_locked_session_store(&path, store)
        })
        .unwrap();
        let replaced = sqlite::shared(&path).unwrap();
        assert_eq!(replaced.sessions.len(), 2);
        // A reader holding the old snapshot keeps it intact.
        assert_eq!(first.sessions.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_reads_hit_the_cache_and_nested_reads_and_writes_do_not_deadlock() {
        let dir = std::env::temp_dir().join(format!("cutex-session-nested-{}", uuid::Uuid::new_v4()));
        let path = dir.join("cutex-sessions.json");
        with_locked_session_store(&path, |store| {
            store.sessions.insert("cutex.a".into(), record("a"));
            save_locked_session_store(&path, store)
        })
        .unwrap();
        let count = |path: &Path| with_cutex_session_store_at(path, |store| store.sessions.len()).unwrap();
        assert_eq!(count(&path), 1);
        let first = sqlite::shared(&path).unwrap();
        let full_reads = sqlite::FULL_READS.with(|count| count.get());
        for _ in 0..5 {
            assert!(Arc::ptr_eq(&first, &sqlite::shared(&path).unwrap()), "no re-read");
        }
        assert_eq!(sqlite::FULL_READS.with(|count| count.get()), full_reads);
        // A read inside a read (same thread) does not deadlock.
        let nested = with_cutex_session_store_at(&path, |outer| {
            (outer.sessions.len(), count(&path))
        })
        .unwrap();
        assert_eq!(nested, (1, 1));
        // A write inside a read neither deadlocks nor leaves a stale entry.
        with_cutex_session_store_at(&path, |_| {
            with_locked_session_store(&path, |store| {
                store.sessions.insert("cutex.b".into(), record("b"));
                save_locked_session_store(&path, store)
            })
        })
        .unwrap()
        .unwrap();
        assert_eq!(count(&path), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_permission_alias_repair_is_persisted_once() {
        let dir =
            std::env::temp_dir().join(format!("cutex-stale-alias-{}", uuid::Uuid::new_v4()));
        let path = dir.join("cutex-sessions.json");
        let mut stale = record("stale-alias");
        stale.permission_defaults = Some("workspace-write".to_string());
        stale.sandbox_mode = Some("danger-full-access".to_string());
        stale.approval_policy = Some("never".to_string());
        let before = stale.durable_revision();
        with_locked_session_store(&path, |store| {
            store.sessions.insert("cutex.stale-alias".into(), stale);
            save_locked_session_store(&path, store)
        })
        .unwrap();
        assert_eq!(
            repair_cutex_session_permission_alias_at(&path, "cutex.stale-alias").unwrap(),
            Some(("workspace-write".to_string(), "danger-full-access".to_string()))
        );
        let store = load_cutex_session_store_from_path(&path).unwrap();
        let repaired = &store.sessions["cutex.stale-alias"];
        assert_eq!(repaired.permission_defaults.as_deref(), Some("danger-full-access"));
        assert_eq!(repaired.sandbox_mode.as_deref(), Some("danger-full-access"));
        assert_eq!(repaired.approval_policy.as_deref(), Some("never"));
        assert_eq!(repaired.durable_revision(), before + 1);
        assert_eq!(repair_cutex_session_permission_alias_at(&path, "cutex.stale-alias").unwrap(), None);
        assert_eq!(repair_cutex_session_permission_alias_at(&path, "cutex.missing").unwrap(), None);
        let _ = fs::remove_dir_all(&dir);
    }

    fn record(name: &str) -> CutexSessionRecord {
        CutexSessionRecord::new_at(
            format!("cutex.{name}"),
            Some(format!("thread-{name}")),
            "devbox".to_string(),
            format!("/tmp/{name}"),
            None,
            "2026-08-10T00:00:00Z".to_string(),
        )
        .expect("record")
    }

    #[test]
    fn pretty_and_compact_json_stores_are_imported_and_rows_are_written_compact() {
        for pretty in [true, false] {
            let root =
                std::env::temp_dir().join(format!("cutex-store-import-{}", uuid::Uuid::new_v4()));
            let path = root.join("cutex-sessions.json");
            // A store written by older cutex (pretty before r216, compact since).
            let mut legacy = CutexSessionStore::default();
            legacy.sessions.insert("cutex.a".into(), record("a"));
            legacy.store_revision.set(41);
            fs::create_dir_all(&root).unwrap();
            let bytes = if pretty {
                serde_json::to_vec_pretty(&legacy).unwrap()
            } else {
                serde_json::to_vec(&legacy).unwrap()
            };
            fs::write(&path, &bytes).unwrap();
            let loaded = load_cutex_session_store_from_path(&path).unwrap();
            assert_eq!(
                serde_json::to_value(&loaded).unwrap(),
                serde_json::to_value(&legacy).unwrap()
            );
            assert!(is_session_store_sentinel(&fs::read(&path).unwrap()));
            with_locked_session_store(&path, |store| {
                store.sessions.insert("cutex.b".into(), record("b"));
                save_locked_session_store(&path, store)
            })
            .unwrap();
            let reread = load_cutex_session_store_from_path(&path).unwrap();
            assert_eq!(reread.sessions.len(), 2);
            assert_eq!(reread.store_revision.get(), 42);
            let connection = rusqlite::Connection::open(session_database_path(&path)).unwrap();
            let row: String = connection
                .query_row("SELECT record_json FROM sessions WHERE key = 'cutex.b'", [], |row| row.get(0))
                .unwrap();
            assert!(!row.contains('\n'), "compact rows");
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&row).unwrap(),
                serde_json::to_value(&reread.sessions["cutex.b"]).unwrap()
            );
            drop(connection);
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn delete_removes_only_the_unchanged_target_record() {
        let root =
            std::env::temp_dir().join(format!("cutex-record-delete-{}", uuid::Uuid::new_v4()));
        let path = root.join("cutex-sessions.json");
        let mut store = CutexSessionStore::default();
        store.sessions.insert("cutex.gone".into(), record("gone"));
        store.sessions.insert("cutex.kept".into(), record("kept"));
        save_cutex_session_store_to_path(&path, &store).unwrap();
        let loaded = load_cutex_session_store_from_path(&path).unwrap();
        let mut stale = loaded.sessions["cutex.gone"].clone();
        stale.revision += 1;
        assert!(remove_cutex_session_record_at(&path, &stale).is_err());
        assert_eq!(load_cutex_session_store_from_path(&path).unwrap().sessions.len(), 2);
        let removed =
            remove_cutex_session_record_at(&path, &loaded.sessions["cutex.gone"]).unwrap();
        assert_eq!(removed.cutex_session_id, "cutex.gone");
        let after = load_cutex_session_store_from_path(&path).unwrap();
        assert!(after.sessions.contains_key("cutex.kept"));
        assert!(!after.sessions.contains_key("cutex.gone"));
        assert!(remove_cutex_session_record_at(&path, &loaded.sessions["cutex.gone"]).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prepared_record_commit_preserves_other_writes_and_rejects_target_changes() {
        let root =
            std::env::temp_dir().join(format!("cutex-record-merge-{}", uuid::Uuid::new_v4()));
        let path = root.join("cutex-sessions.json");
        let initial = CutexSessionStore::default();
        save_cutex_session_store_to_path(&path, &initial).unwrap();
        let before = load_cutex_session_store_from_path(&path).unwrap();
        let mut prepared = before.clone();
        prepared.sessions.insert("cutex.new".into(), record("new"));
        let mut other = load_cutex_session_store_from_path(&path).unwrap();
        other.sessions.insert("cutex.other".into(), record("other"));
        save_cutex_session_store_to_path(&path, &other).unwrap();
        save_cutex_session_record_to_path(&path, &before, &prepared, "cutex.new").unwrap();
        let current = load_cutex_session_store_from_path(&path).unwrap();
        assert!(current.sessions.contains_key("cutex.other"));
        assert!(current.sessions.contains_key("cutex.new"));
        assert!(save_cutex_session_record_to_path(&path, &before, &prepared, "cutex.new").is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_archived_agents_name_may_be_reused_but_a_live_one_may_not() {
        // r244: `agent_create` of a closed Agent's name created the native
        // thread, then adoption failed here with `agent name was
        // concurrently assigned to another record` (outcome unknown), which
        // left the name reserved.
        let root =
            std::env::temp_dir().join(format!("cutex-record-name-{}", uuid::Uuid::new_v4()));
        let path = root.join("cutex-sessions.json");
        let mut initial = CutexSessionStore::default();
        let mut archived = record("old");
        archived.formal_agent_name = Some("r244-jia".into());
        archived.archive_state = crate::session::model::CutexSessionArchiveState::Retired;
        archived.retired_at = Some("2026-10-18T00:00:00Z".into());
        initial.sessions.insert("cutex.old".into(), archived);
        save_cutex_session_store_to_path(&path, &initial).unwrap();
        let before = load_cutex_session_store_from_path(&path).unwrap();
        let mut prepared = before.clone();
        let mut successor = record("new");
        successor.formal_agent_name = Some("r244-jia".into());
        prepared.sessions.insert("cutex.new".into(), successor.clone());
        save_cutex_session_record_to_path(&path, &before, &prepared, "cutex.new").unwrap();
        // A second live record with the same name is still refused.
        let before = load_cutex_session_store_from_path(&path).unwrap();
        let mut prepared = before.clone();
        let mut duplicate = record("dup");
        duplicate.formal_agent_name = Some("r244-jia".into());
        prepared.sessions.insert("cutex.dup".into(), duplicate);
        let error = save_cutex_session_record_to_path(&path, &before, &prepared, "cutex.dup")
            .unwrap_err();
        assert!(format!("{error:#}").contains("agent name was concurrently assigned"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_target_update_cannot_be_overwritten() {
        let root =
            std::env::temp_dir().join(format!("cutex-session-store-cas-{}", uuid::Uuid::new_v4()));
        let path = root.join("cutex-sessions.json");
        let mut initial = CutexSessionStore::default();
        initial
            .sessions
            .insert("cutex.target".to_string(), record("target"));
        save_cutex_session_store_to_path(&path, &initial).expect("save initial store");
        assert_eq!(initial.store_revision.get(), 1);

        let mut first = load_cutex_session_store_from_path(&path).expect("load first writer");
        let mut stale = load_cutex_session_store_from_path(&path).expect("load stale writer");
        let first_target = first
            .sessions
            .get_mut("cutex.target")
            .expect("first target");
        first_target.display_name_hint = Some("newer-name".to_string());
        first_target
            .bump_durable_revision()
            .expect("bump first target");
        let stale_target = stale
            .sessions
            .get_mut("cutex.target")
            .expect("stale target");
        stale_target.archive_state = crate::session::model::CutexSessionArchiveState::Retired;
        stale_target.retired_at = Some("2026-08-10T00:01:00Z".to_string());
        stale_target
            .bump_durable_revision()
            .expect("bump stale target");

        save_cutex_session_store_to_path(&path, &first).expect("save first writer");
        let error = save_cutex_session_store_to_path(&path, &stale)
            .expect_err("stale writer must conflict");
        let conflict = error
            .downcast_ref::<CutexSessionStoreRevisionConflict>()
            .expect("typed store conflict");
        assert_eq!(
            *conflict,
            CutexSessionStoreRevisionConflict {
                expected: 1,
                actual: 2,
            }
        );
        assert_eq!(stale.store_revision.get(), 1);

        let persisted = load_cutex_session_store_from_path(&path).expect("reload persisted store");
        assert_eq!(persisted.store_revision.get(), 2);
        let target = persisted
            .sessions
            .get("cutex.target")
            .expect("persisted target");
        assert_eq!(target.display_name_hint.as_deref(), Some("newer-name"));
        assert!(target.is_active());
        assert_eq!(target.durable_revision(), 2);
        fs::remove_dir_all(root).expect("remove test store");
    }
}
