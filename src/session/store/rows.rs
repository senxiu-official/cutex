//! Row-level session store writes (session store Phase 2b,
//! `docs/research/session-store-sqlite.md` in cutex-desktop).
//!
//! [`with_locked_session_store`](super::with_locked_session_store) hands its
//! closure an owned copy of the whole store and commits by comparing every
//! session and receipt with the stored revision: 40-130 ms per update at
//! large-host size, almost all of it copying and comparing launch receipts.
//! [`with_locked_session_rows`] keeps the same fence (the store file lock,
//! then a `BEGIN IMMEDIATE` compare-and-swap on the store revision) but the
//! closure reads the shared snapshot and only the rows it touches are
//! copied, compared and written. Receipts written with a session change are
//! committed in the same transaction.

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use fs2::FileExt;

use super::sqlite;
use super::sqlite::ReceiptMap;
use super::sqlite::TouchedRows;
use crate::session::model::CutexSessionRecord;
use crate::session::model::CutexSessionStore;

pub(crate) use super::sqlite::AgentArchive as AgentArchiveReceipts;
pub(crate) use super::sqlite::ExplicitLaunch as ExplicitLaunchReceipts;
pub(crate) use super::sqlite::FormalName as FormalNameReceipts;
pub(crate) use super::sqlite::HumanAdoption as HumanAdoptionReceipts;
pub(crate) use super::sqlite::LaunchTombstone as ExplicitLaunchTombstones;
pub(crate) use super::sqlite::NativeStop as NativeStopReceipts;

/// The store as seen by one row-level writer: the committed snapshot at
/// [`SessionRows::revision`] plus the rows this writer changed since its last
/// [`SessionRows::commit`].
pub struct SessionRows<'a> {
    path: &'a Path,
    revision: u64,
    /// Always `Some` outside `commit` (taken so the cache can be patched
    /// without copying the store).
    base: Option<Arc<CutexSessionStore>>,
    touched: TouchedRows,
}

impl<'a> SessionRows<'a> {
    fn base(&self) -> &CutexSessionStore {
        self.base.as_deref().expect("session row snapshot")
    }

    /// The store revision the snapshot (and the next commit's
    /// compare-and-swap) is at.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The committed store at [`Self::revision`], without this writer's
    /// uncommitted changes. For whole-store validation and receipt reads.
    pub fn committed(&self) -> &CutexSessionStore {
        self.base()
    }

    /// One session (with this writer's uncommitted change, if any).
    pub fn session(&self, key: &str) -> Option<&CutexSessionRecord> {
        if self.touched.sessions.contains(key) {
            self.touched.values.sessions.get(key)
        } else {
            self.base().sessions.get(key)
        }
    }

    /// Every session (with this writer's uncommitted changes), unordered.
    pub fn sessions(&self) -> impl Iterator<Item = (&String, &CutexSessionRecord)> {
        let touched = &self.touched.sessions;
        self.base()
            .sessions
            .iter()
            .filter(move |(key, _)| !touched.contains(key.as_str()))
            .chain(self.touched.values.sessions.iter())
    }

    /// A writable copy of one session row; the change is written by the next
    /// [`Self::commit`] (only if the row then differs from the stored one).
    pub fn session_mut(&mut self, key: &str) -> Option<&mut CutexSessionRecord> {
        if !self.touched.sessions.contains(key) {
            let record = self.base().sessions.get(key)?.clone();
            self.touched.sessions.insert(key.to_string());
            self.touched.values.sessions.insert(key.to_string(), record);
        }
        self.touched.values.sessions.get_mut(key)
    }

    pub fn insert_session(&mut self, key: impl Into<String>, record: CutexSessionRecord) {
        let key = key.into();
        self.touched.sessions.insert(key.clone());
        self.touched.values.sessions.insert(key, record);
    }

    pub fn remove_session(&mut self, key: &str) -> Option<CutexSessionRecord> {
        let previous = self.session(key).cloned();
        self.touched.sessions.insert(key.to_string());
        self.touched.values.sessions.remove(key);
        previous
    }

    /// One receipt (with this writer's uncommitted change, if any).
    #[allow(dead_code)] // API completeness; tests use it.
    pub(crate) fn receipt<M: ReceiptMap>(&self, key: &str) -> Option<&M::Value> {
        if self.touched.receipts.contains(&(M::NAME, key.to_string())) {
            M::map(&self.touched.values).get(key)
        } else {
            M::map(self.base()).get(key)
        }
    }

    /// Writes one receipt row with the next commit (same transaction as the
    /// session rows of that commit).
    pub(crate) fn insert_receipt<M: ReceiptMap>(&mut self, key: impl Into<String>, value: M::Value) {
        let key = key.into();
        self.touched.receipts.insert((M::NAME, key.clone()));
        M::map_mut(&mut self.touched.values).insert(key, value);
    }

    #[allow(dead_code)] // API completeness; tests use it.
    pub(crate) fn remove_receipt<M: ReceiptMap>(&mut self, key: &str) {
        self.touched.receipts.insert((M::NAME, key.to_string()));
        M::map_mut(&mut self.touched.values).remove(key);
    }

    /// The sessions only (with uncommitted changes), as an owned store with
    /// empty receipt maps, for reconciliation code written against a whole
    /// `CutexSessionStore`. Copies every session record (≈1.4 KB each), never
    /// the receipts. Hand the result back with [`Self::replace_sessions`].
    pub fn sessions_copy(&self) -> CutexSessionStore {
        let store = CutexSessionStore {
            sessions: self
                .sessions()
                .map(|(key, record)| (key.clone(), record.clone()))
                .collect(),
            ..CutexSessionStore::default()
        };
        store.store_revision.set(self.revision);
        store
    }

    /// Takes over the sessions of `store` (from [`Self::sessions_copy`]):
    /// records that differ from the current view become touched rows,
    /// missing ones are deleted. Receipts cannot be changed this way.
    pub fn replace_sessions(&mut self, store: &CutexSessionStore) -> anyhow::Result<()> {
        anyhow::ensure!(
            store.native_stop_receipts.is_empty()
                && store.explicit_launch_receipts.is_empty()
                && store.human_adoption_receipts.is_empty()
                && store.agent_archive_receipts.is_empty()
                && store.formal_name_receipts.is_empty()
                && store.explicit_launch_tombstones.is_empty(),
            "a sessions-only copy cannot carry receipts; use insert_receipt"
        );
        let removed = self
            .sessions()
            .filter(|(key, _)| !store.sessions.contains_key(key.as_str()))
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in removed {
            self.remove_session(&key);
        }
        for (key, record) in &store.sessions {
            if self.session(key) != Some(record) {
                self.insert_session(key.clone(), record.clone());
            }
        }
        Ok(())
    }

    /// True when this writer has uncommitted changes.
    pub fn is_dirty(&self) -> bool {
        !self.touched.is_empty()
    }

    /// Commits the touched rows if the store is still at [`Self::revision`]
    /// (otherwise [`super::CutexSessionStoreRevisionConflict`]; the store
    /// lock makes that impossible between cutex writers). Rows equal to the
    /// stored ones are skipped and a commit that changes nothing writes
    /// nothing and keeps the revision. Afterwards the view is at the new
    /// revision with no uncommitted changes. Returns the store revision.
    #[track_caller]
    pub fn commit(&mut self) -> anyhow::Result<u64> {
        commit_at(self, std::panic::Location::caller())
    }
}

/// Runs `operation` under the store file lock with a row-level view of the
/// store at its latest revision. Same lock (and lock order: file lock, then
/// the SQLite write lock inside [`SessionRows::commit`]) as
/// [`super::with_locked_session_store`], so row-level and whole-store
/// writers serialise with each other across processes. `operation` runs
/// outside any database transaction; changes not committed when it returns
/// are discarded.
pub fn with_locked_session_rows<T>(
    path: &Path,
    operation: impl FnOnce(&mut SessionRows<'_>) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let parent = path.parent().context("session store has no parent")?;
    std::fs::create_dir_all(parent)?;
    sqlite::ensure_ready(path, true, false)?;
    let lock = super::open_store_lock(&parent.join(super::CUTEX_SESSIONS_LOCK_FILE))?;
    FileExt::lock_exclusive(&lock).context("Failed to lock cutex session store")?;
    let base = sqlite::shared(path)?;
    let mut rows = SessionRows {
        path,
        revision: base.store_revision.get(),
        base: Some(base),
        touched: TouchedRows::default(),
    };
    operation(&mut rows)
}

/// One session read straight from the database by store key, or else by
/// `cutex_session_id` (no snapshot load: a short-lived process that needs
/// one record does not parse the whole store). Returns `(key, record)`.
pub fn load_session_row(
    path: &Path,
    key_or_id: &str,
) -> anyhow::Result<Option<(String, CutexSessionRecord)>> {
    sqlite::read_session_row(path, key_or_id)
}

/// Updates one existing session row under the store lock; `None` when the
/// row does not exist. The row is written only if `update` changed it.
#[track_caller]
pub fn update_session_row<T>(
    path: &Path,
    key: &str,
    update: impl FnOnce(&mut CutexSessionRecord) -> anyhow::Result<T>,
) -> anyhow::Result<Option<T>> {
    let site = std::panic::Location::caller();
    with_locked_session_rows(path, |rows| {
        let Some(record) = rows.session_mut(key) else {
            return Ok(None);
        };
        let value = update(record)?;
        commit_at(rows, site)?;
        Ok(Some(value))
    })
}

/// Compare-and-swap of one session row: writes `new` (or deletes the row
/// for `None`) only if the stored row still equals `expected` (`None`: the
/// row must not exist). Fails with [`SessionRowConflict`] otherwise.
#[track_caller]
pub fn compare_and_swap_session_row(
    path: &Path,
    key: &str,
    expected: Option<&CutexSessionRecord>,
    new: Option<CutexSessionRecord>,
) -> anyhow::Result<u64> {
    let site = std::panic::Location::caller();
    with_locked_session_rows(path, |rows| {
        if rows.session(key) != expected {
            return Err(SessionRowConflict {
                key: key.to_string(),
            }
            .into());
        }
        match new {
            Some(record) => rows.insert_session(key, record),
            None => {
                rows.remove_session(key);
            }
        }
        commit_at(rows, site)
    })
}

/// Writes (or with `None` removes) one receipt row under the store lock.
#[track_caller]
pub(crate) fn update_receipt_row<M: ReceiptMap>(
    path: &Path,
    key: &str,
    value: Option<M::Value>,
) -> anyhow::Result<u64> {
    let site = std::panic::Location::caller();
    with_locked_session_rows(path, |rows| {
        match value {
            Some(value) => rows.insert_receipt::<M>(key, value),
            None => rows.remove_receipt::<M>(key),
        }
        commit_at(rows, site)
    })
}

fn commit_at(rows: &mut SessionRows<'_>, site: sqlite::Site) -> anyhow::Result<u64> {
    if rows.touched.is_empty() {
        return Ok(rows.revision);
    }
    let next = sqlite::commit_rows_locked(rows.path, rows.revision, &mut rows.base, &rows.touched, site)?;
    rows.revision = next;
    rows.touched = TouchedRows::default();
    Ok(next)
}

/// [`compare_and_swap_session_row`] found the row changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRowConflict {
    pub key: String,
}

impl std::fmt::Display for SessionRowConflict {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "cutex session {} changed concurrently; reload before retrying",
            self.key
        )
    }
}

impl std::error::Error for SessionRowConflict {}

#[cfg(test)]
mod tests;
