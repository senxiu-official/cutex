use std::collections::BTreeMap;

pub(crate) use super::roster_map::RosterMap;
#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256 as Sha256Digest};

use anyhow::Context as _;
use crate::role_revision::{Rfc3339, Sha256};

use super::{
    AgentActionId, AgentActionRecord, AgentManagementError, AgentManagementFailureEvent,
    AgentManagementPhaseEvent, AgentManagementStoreSchema, AgentOperatorAuditEvent,
    AgentOperatorGrant, AgentReservationReconciliationEvent, AgentReservationReconciliationReceipt,
    CurrentProjectMembership, LegacyDirectorOwnershipImportReceipt, ManagedAgentRecord,
    ProjectAuditEvent, ProjectAuthority, ProjectAuthorityReceipt, ProjectId, ProjectStateRecord,
    ProjectTombstone,
};
use crate::management::control_plane::{
    HumanManagementOperatorActionRecord, HumanManagementProjectMutationActionRecord,
};

const STORE_FILE: &str = "agent-management-v1.json";
const LOCK_FILE: &str = "agent-management-v1.lock";
const MUTATION_LOCK_FILE: &str = "agent-management-mutation-v1.lock";

/// Debug builds panic on a lane holder waiting for the fence (r271 guard
/// (a)); `CUTEX_LOCK_ORDER_GUARD=log` makes them log like release builds, so
/// a whole suite or a private instance can count the warnings instead.
fn lock_order_guard_logs() -> bool {
    static LOGS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *LOGS.get_or_init(|| std::env::var("CUTEX_LOCK_ORDER_GUARD").is_ok_and(|mode| mode == "log"))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentManagementSnapshot {
    /// Read-only legacy-close proof set for directory projections. Full snapshots
    /// keep None and use their original phase/action evidence. Never persisted.
    #[serde(skip)]
    pub legacy_close_projection: Option<Arc<std::collections::BTreeSet<String>>>,

    #[serde(default, skip_serializing_if = "RosterMap::is_empty")]
    pub bootstrap_intents: RosterMap<AgentActionId, super::BootstrapIntentReview>,
    /// Read-only durable projection, never persisted into roster history.
    #[serde(skip)]
    pub reversible_archive_projection: RosterMap<crate::role_revision::CutexSessionId, bool>,
    #[serde(default)]
    pub agent_archive_actions: RosterMap<AgentActionId, super::AgentArchiveReceipt>,
    #[serde(default)]
    pub agent_archive_audit: RosterMap<String, super::AgentArchiveReceipt>,
    pub schema: AgentManagementStoreSchema,
    pub store_revision: u64,
    #[serde(default)]
    pub durable_import_actions: RosterMap<AgentActionId, super::DurableImportReceipt>,
    #[serde(default)]
    pub durable_import_audit: RosterMap<String, super::DurableImportAuditEvent>,
    pub projects: RosterMap<ProjectId, ProjectAuthority>,
    /// Exact project/session keyed Agent Operator grants. Missing on legacy v1
    /// stores and therefore migrated as an empty grant set.
    #[serde(default)]
    pub operator_grants:
        RosterMap<ProjectId, BTreeMap<crate::role_revision::CutexSessionId, AgentOperatorGrant>>,
    /// Per-project CAS revision for the complete Operator grant set.
    #[serde(default)]
    pub operator_grant_revisions: RosterMap<ProjectId, u64>,
    #[serde(default)]
    pub operator_audit_events: RosterMap<String, AgentOperatorAuditEvent>,
    /// Idempotency receipts for the separate Human/Management action domain.
    /// These records never impersonate an Agent Management action caller.
    #[serde(default)]
    pub human_management_operator_actions:
        RosterMap<AgentActionId, HumanManagementOperatorActionRecord>,
    /// Idempotent Human/Management Project structure mutations.
    #[serde(default)]
    pub human_management_project_mutations:
        RosterMap<AgentActionId, HumanManagementProjectMutationActionRecord>,
    /// Additive current membership overrides. Missing entries retain the
    /// historical ManagedAgentRecord.project_id interpretation for active
    /// legacy Agents; writes never rewrite that historical provenance.
    #[serde(default)]
    pub current_project_memberships:
        RosterMap<crate::role_revision::CutexSessionId, CurrentProjectMembership>,
    /// Lifecycle/CAS records for Project structure. Missing legacy records are
    /// projected as active at revision zero without forcing a migration write.
    #[serde(default)]
    pub project_states: RosterMap<ProjectId, ProjectStateRecord>,
    #[serde(default)]
    pub project_tombstones: RosterMap<ProjectId, ProjectTombstone>,
    #[serde(default)]
    pub project_audit_events: RosterMap<String, ProjectAuditEvent>,
    /// Non-authoritative UI metadata. Its map key is always the canonical
    /// provider-owned project identity; none of these values participate in
    /// authorization.
    #[serde(default)]
    pub project_presentations: RosterMap<ProjectId, super::ProjectPresentationSettings>,
    pub agents: RosterMap<crate::role_revision::CutexSessionId, ManagedAgentRecord>,
    pub actions: RosterMap<AgentActionId, AgentActionRecord>,
    #[serde(default)]
    pub phase_events: RosterMap<String, AgentManagementPhaseEvent>,
    pub authority_receipts: RosterMap<AgentActionId, ProjectAuthorityReceipt>,
    #[serde(default)]
    pub legacy_director_ownership_import_receipts:
        RosterMap<AgentActionId, LegacyDirectorOwnershipImportReceipt>,
    #[serde(default)]
    pub reservation_reconciliation_receipts:
        RosterMap<AgentActionId, AgentReservationReconciliationReceipt>,
    #[serde(default)]
    pub reservation_reconciliation_events: RosterMap<String, AgentReservationReconciliationEvent>,
    pub failure_events: RosterMap<String, AgentManagementFailureEvent>,
    /// Sparse `agent_create` / `member_replace` calls and the full request
    /// each resolved to (`sparse.rs`), keyed by action id.
    #[serde(default, skip_serializing_if = "RosterMap::is_empty")]
    pub sparse_requests: RosterMap<AgentActionId, super::SparseRequestRecord>,
    /// Preserve additive provider fields across presentation-only writes so a
    /// newer store is not silently downgraded by this reader.
    #[serde(default, flatten)]
    pub extra: RosterMap<String, serde_json::Value>,
    /// `agent_create` full-reply receipts by action id (owned by
    /// `agent_bus::action_receipts`). The previous build keeps them as one
    /// object in `extra.tool_action_receipts_v1`; SQL stores one row each,
    /// the import splits that object and the R1 export folds it back.
    #[serde(skip)]
    pub tool_action_receipts: RosterMap<String, serde_json::Value>,
}

impl AgentManagementSnapshot {
    /// The agent row of one durable session id: a keyed lookup (rows are
    /// stored under their own id) instead of scanning every agent.
    pub fn agent_by_session(&self, cutex_session_id: &str) -> Option<&ManagedAgentRecord> {
        let id = crate::role_revision::CutexSessionId::new(cutex_session_id).ok()?;
        self.agents.get(&id).filter(|agent| agent.cutex_session_id == id)
    }

    /// The stored tool receipt of one action: its own row, else (a snapshot
    /// built in memory from the previous layout) the legacy extension object.
    pub fn tool_action_receipt(&self, action_id: &str) -> Option<&serde_json::Value> {
        self.tool_action_receipts.get(action_id).or_else(|| {
            self.extra.get(super::roster_sql::TOOL_RECEIPTS_EXTENSION).and_then(|receipts| receipts.get(action_id))
        })
    }

    pub(crate) fn empty() -> Self {
        Self {
            legacy_close_projection: None,
            bootstrap_intents: RosterMap::new(),
            reversible_archive_projection: RosterMap::new(),
            agent_archive_actions: RosterMap::new(),
            agent_archive_audit: RosterMap::new(),
            schema: AgentManagementStoreSchema::V1,
            store_revision: 0,
            durable_import_actions: RosterMap::new(),
            durable_import_audit: RosterMap::new(),
            projects: RosterMap::new(),
            operator_grants: RosterMap::new(),
            operator_grant_revisions: RosterMap::new(),
            operator_audit_events: RosterMap::new(),
            human_management_operator_actions: RosterMap::new(),
            human_management_project_mutations: RosterMap::new(),
            current_project_memberships: RosterMap::new(),
            project_states: RosterMap::new(),
            project_tombstones: RosterMap::new(),
            project_audit_events: RosterMap::new(),
            project_presentations: RosterMap::new(),
            agents: RosterMap::new(),
            actions: RosterMap::new(),
            phase_events: RosterMap::new(),
            authority_receipts: RosterMap::new(),
            legacy_director_ownership_import_receipts: RosterMap::new(),
            reservation_reconciliation_receipts: RosterMap::new(),
            reservation_reconciliation_events: RosterMap::new(),
            failure_events: RosterMap::new(),
            sparse_requests: RosterMap::new(),
            extra: RosterMap::new(),
            tool_action_receipts: RosterMap::new(),
        }
    }
}

#[derive(Clone)]
pub struct AgentManagementStore {
    root: Arc<PathBuf>,
    process_lock: Arc<Mutex<()>>,
}

impl AgentManagementStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, AgentManagementError> {
        let root = root.into();
        prepare_private_root(&root)?;
        Ok(Self {
            root: Arc::new(root),
            process_lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn open_default() -> anyhow::Result<Self> {
        Self::open(
            crate::config::paths::runtime_dir()?
                .join("agent-management")
                .join("v1"),
        )
        .map_err(anyhow::Error::new)
    }

    /// The store directory (receipt files live next to the store).
    pub(crate) fn root(&self) -> &std::path::Path {
        &self.root
    }

    pub fn snapshot(&self) -> Result<AgentManagementSnapshot, AgentManagementError> {
        // Compatibility for callers that need an owned mutable projection.
        Ok((*self.snapshot_shared()?).clone())
    }

    /// Immutable, committed snapshot; hot authority checks share the parsed
    /// generation instead of deep-cloning historical actions on every read.
    pub fn snapshot_shared(&self) -> Result<Arc<AgentManagementSnapshot>, AgentManagementError> {
        self.snapshot_view(false)
    }

    /// Explicit read projection for directory, scope and archive-authority readers.
    /// Omits actions, phase/failure events, sparse requests and tool receipts.
    /// Never use this projection as a mutation/export base.
    pub fn directory_snapshot_shared(&self) -> Result<Arc<AgentManagementSnapshot>, AgentManagementError> {
        self.snapshot_view(true)
    }

    pub fn directory_snapshot(&self) -> Result<AgentManagementSnapshot, AgentManagementError> {
        Ok((*self.directory_snapshot_shared()?).clone())
    }

    /// Read-only directory plus selected action witnesses from the same WAL view.
    pub(crate) fn directory_for_actions(&self, ids: &[AgentActionId]) -> Result<Arc<AgentManagementSnapshot>, AgentManagementError> {
        self.snapshot_selection(true, ids, &[])
    }

    pub(crate) fn directory_for_starts(&self, external_ids: &[String]) -> Result<Arc<AgentManagementSnapshot>, AgentManagementError> {
        self.snapshot_selection(true, &[], external_ids)
    }

    pub(crate) fn has_unfinished_rotation(&self) -> Result<bool, AgentManagementError> {
        // Prepares old SQL homes' indexes without warming a full snapshot.
        let _directory = self.directory_snapshot_shared()?;
        let database = super::roster_sql::database_path(&self.root);
        if !database.exists() {return Ok(false);}
        with_read_connection(&database, super::roster_sql::has_unfinished_rotation)
    }

    fn snapshot_view(&self, directory: bool) -> Result<Arc<AgentManagementSnapshot>, AgentManagementError> {
        self.snapshot_selection(directory, &[], &[])
    }

    fn snapshot_selection(&self, directory: bool, action_ids: &[AgentActionId], external_ids: &[String]) -> Result<Arc<AgentManagementSnapshot>, AgentManagementError> {
        let mut options=OpenOptions::new();options.read(true).write(true);
        set_private_open_options(&mut options);
        let lock=match options.open(self.root.join(LOCK_FILE)) {
            Ok(lock)=>lock,
            Err(e) if e.kind()==io::ErrorKind::NotFound=>return Ok(Arc::new(AgentManagementSnapshot::empty())),
            Err(_)=>return Err(AgentManagementError::PersistenceUnavailable),
        };
        FileExt::lock_shared(&lock).map_err(|_|AgentManagementError::PersistenceUnavailable)?;
        if !sql_authoritative(&self.root)? {
            // First access after an upgrade (or after an R1 handoff): the
            // one-shot import needs the exclusive store lock.
            drop(lock);
            self.import_under_exclusive_lock()?;
            return self.snapshot_selection(directory, action_ids, external_ids);
        }
        if directory && !with_read_connection(&super::roster_sql::database_path(&self.root), super::roster_sql::directory_index_ready)? {
            drop(lock);
            self.import_under_exclusive_lock()?;
            return self.snapshot_selection(directory, action_ids, external_ids);
        }
        read_snapshot_selection(&self.root, directory, action_ids, external_ids)
    }

    /// One committed agent row, without initializing the full roster cache.
    pub fn agent(
        &self,
        id: &crate::role_revision::CutexSessionId,
    ) -> Result<Option<ManagedAgentRecord>, AgentManagementError> {
        Ok(self.read_row("agents", id.as_str())?.agents.remove(id))
    }

    /// One action row of the committed roster (keyed).
    pub fn action(&self, id: &AgentActionId) -> Result<Option<AgentActionRecord>, AgentManagementError> {
        Ok(self.read_row("actions", id.as_str())?.actions.remove(id))
    }

    fn read_row(&self, field: &str, key: &str) -> Result<AgentManagementSnapshot, AgentManagementError> {
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        set_private_open_options(&mut options);
        let lock = match options.open(self.root.join(LOCK_FILE)) {
            Ok(lock) => lock,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(AgentManagementSnapshot::empty()),
            Err(_) => return Err(AgentManagementError::PersistenceUnavailable),
        };
        FileExt::lock_shared(&lock).map_err(|_| AgentManagementError::PersistenceUnavailable)?;
        if !sql_authoritative(&self.root)? {
            drop(lock);
            self.import_under_exclusive_lock()?;
            return self.read_row(field, key);
        }
        with_read_connection(&super::roster_sql::database_path(&self.root), |db| {
            let spec = super::roster_sql::spec_by_field(field).context("roster field")?;
            let mut selected = AgentManagementSnapshot::empty();
            if let Some(body) = super::roster_sql::get_body(db, spec, key)? {
                super::roster_sql::apply_row(&mut selected, field, key, Some(&body))?;
            }
            Ok(selected)
        })
    }

    /// Committed phase events of one action in event-id order: an indexed
    /// page read instead of filtering the whole phase history.
    pub fn phase_events_for_action(
        &self,
        action_id: &AgentActionId,
    ) -> Result<Vec<AgentManagementPhaseEvent>, AgentManagementError> {
        self.events_for_action("phase_events", action_id)
    }

    /// Committed failure events of one action in event-id order (indexed).
    pub fn failure_events_for_action(
        &self,
        action_id: &AgentActionId,
    ) -> Result<Vec<AgentManagementFailureEvent>, AgentManagementError> {
        self.events_for_action("failure_events", action_id)
    }

    fn events_for_action<T: serde::de::DeserializeOwned>(
        &self,
        field: &str,
        action_id: &AgentActionId,
    ) -> Result<Vec<T>, AgentManagementError> {
        // Same admission as `snapshot_shared`: absent store = empty, first
        // access after an upgrade imports first.
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        set_private_open_options(&mut options);
        let lock = match options.open(self.root.join(LOCK_FILE)) {
            Ok(lock) => lock,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err(AgentManagementError::PersistenceUnavailable),
        };
        FileExt::lock_shared(&lock).map_err(|_| AgentManagementError::PersistenceUnavailable)?;
        if !sql_authoritative(&self.root)? {
            drop(lock);
            self.import_under_exclusive_lock()?;
            return self.events_for_action(field, action_id);
        }
        with_read_connection(&super::roster_sql::database_path(&self.root), |db| {
            super::roster_sql::events_for_action(db, field, action_id.as_str())
        })
    }

    /// Change token of the committed roster: `(epoch, change_seq)`. Watchers
    /// compare it instead of the legacy JSON file stamp. `None` before the
    /// first import / for an absent store.
    pub fn change_token(&self) -> Option<RosterStamp> {
        roster_stamp(&self.root)
    }

    /// Kit step: import now under the exclusive store lock (idempotent).
    pub fn import_now(&self) -> anyhow::Result<super::roster_sql::ImportReport> {
        let _guard = self.exclusive_store_lock()?;
        forget_cached(&self.root);
        *READ_CONNECTION.lock().unwrap_or_else(|e| e.into_inner()) = None;
        super::roster_sql::import_legacy(&self.root)
    }

    /// Kit R1 step: export to the previous build's JSON under the exclusive
    /// store lock. Writers must already be stopped.
    pub fn export_previous_now(&self) -> anyhow::Result<super::roster_sql::ExportReport> {
        let _guard = self.exclusive_store_lock()?;
        *READ_CONNECTION.lock().unwrap_or_else(|e| e.into_inner()) = None;
        super::roster_sql::export_previous(&self.root)
    }

    fn exclusive_store_lock(&self) -> anyhow::Result<(std::sync::MutexGuard<'_, ()>, std::fs::File)> {
        let process = self
            .process_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("roster process lock poisoned"))?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        set_private_open_options(&mut options);
        let lock = options.open(self.root.join(LOCK_FILE))?;
        FileExt::lock_exclusive(&lock)?;
        Ok((process, lock))
    }

    fn import_under_exclusive_lock(&self) -> Result<(), AgentManagementError> {
        let _process = self
            .process_lock
            .lock()
            .map_err(|_| AgentManagementError::PersistenceUnavailable)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        set_private_open_options(&mut options);
        let lock = options
            .open(self.root.join(LOCK_FILE))
            .map_err(|_| AgentManagementError::PersistenceUnavailable)?;
        FileExt::lock_exclusive(&lock).map_err(|_| AgentManagementError::PersistenceUnavailable)?;
        ensure_imported(&self.root)?;
        super::roster_sql::prepare_directory_index(&super::roster_sql::database_path(&self.root)).map_err(sql_error)
    }

    /// Holds the provider-wide mutation fence across authorization checks and
    /// external lifecycle effects. The process mutex in the provider covers
    /// threads; this OS lock extends the same ordering across processes.
    #[track_caller]
    pub(crate) fn lock_mutations(&self) -> Result<crate::platform::lock_accounting::FileGuard, AgentManagementError> {
        if crate::asks::pause_admission::worker_fence_held() {
            return self.worker_fence_view();
        }
        // r271 lock protocol: never wait for the host fence while holding a
        // session lane. Lifecycle legitimately takes lanes AFTER this fence
        // (native stop), so the reverse order could deadlock.
        if crate::management::v2::session_coordination::thread_holds_lane() {
            // The caller names the site even in unsymbolized release logs.
            let caller = std::panic::Location::caller();
            let at = std::backtrace::Backtrace::force_capture();
            if cfg!(debug_assertions) && !lock_order_guard_logs() {
                panic!("r271 lock protocol: blocking on the host authority fence while holding a session lane at {caller}\n{at}");
            }
            eprintln!("cutex-lock-order-warning: blocking on the host authority fence while holding a session lane at {caller}\n{at}");
        }
        let path = self.root.join(MUTATION_LOCK_FILE);
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        set_private_open_options(&mut options);
        let lock = options
            .open(path)
            .map_err(|_| AgentManagementError::PersistenceUnavailable)?;
        crate::platform::lock_accounting::FileGuard::acquire(lock, crate::platform::lock_accounting::Lock::Authority)
            .map_err(|_| AgentManagementError::PersistenceUnavailable)
    }

    /// Delivery must not wait behind a lifecycle action which is joining its
    /// bridge worker. Return pending instead; the next occurrence reconciles.
    #[track_caller]
    pub(crate) fn try_lock_delivery_mutations(&self) -> Result<Option<crate::platform::lock_accounting::FileGuard>, AgentManagementError> {
        if crate::asks::pause_admission::worker_fence_held() {
            return self.worker_fence_view().map(Some);
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        set_private_open_options(&mut options);
        let lock = options
            .open(self.root.join(MUTATION_LOCK_FILE))
            .map_err(|_| AgentManagementError::PersistenceUnavailable)?;
        crate::platform::lock_accounting::FileGuard::try_acquire(lock, crate::platform::lock_accounting::Lock::Authority)
            .map_err(|_| AgentManagementError::PersistenceUnavailable)
    }

    /// The owner admission worker already holds the fence on this thread
    /// (r271): a second handle would conflict with it (Windows locks are per
    /// handle), so the caller gets an unlocked handle as its guard.
    /// Not a second lock attempt, so it is not counted again: the worker's own
    /// accounted guard covers the whole hold.
    fn worker_fence_view(&self) -> Result<crate::platform::lock_accounting::FileGuard, AgentManagementError> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        set_private_open_options(&mut options);
        options
            .open(self.root.join(MUTATION_LOCK_FILE))
            .map(crate::platform::lock_accounting::FileGuard::nested)
            .map_err(|_| AgentManagementError::PersistenceUnavailable)
    }

    pub(crate) fn with_state<T>(
        &self,
        create: bool,
        operation: impl FnOnce(
            AgentManagementSnapshot,
        )
            -> Result<(AgentManagementSnapshot, T, bool), AgentManagementError>,
    ) -> Result<T, AgentManagementError> {
        let _process = self
            .process_lock
            .lock()
            .map_err(|_| AgentManagementError::PersistenceUnavailable)?;
        let lock_path = self.root.join(LOCK_FILE);
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(create);
        set_private_open_options(&mut options);
        let lock = match options.open(&lock_path) {
            Ok(lock) => lock,
            Err(error) if !create && error.kind() == io::ErrorKind::NotFound => {
                let (state, value, write) = operation(AgentManagementSnapshot::empty())?;
                debug_assert!(!write);
                let _ = state;
                return Ok(value);
            }
            Err(_) => return Err(AgentManagementError::PersistenceUnavailable),
        };
        FileExt::lock_exclusive(&lock).map_err(|_| AgentManagementError::PersistenceUnavailable)?;
        ensure_imported(&self.root)?;
        let base = read_snapshot_shared(&self.root)?;
        // O(collections): every map is persistent, the operation's edits copy
        // only their own paths and the commit diff skips shared subtrees.
        let (mut state, value, write) = operation((*base).clone())?;
        if write {
            state.store_revision = state
                .store_revision
                .checked_add(1)
                .filter(|revision| *revision <= crate::role_revision::MAX_JSON_SAFE_INTEGER)
                .ok_or(AgentManagementError::Conflict("store_revision_overflow"))?;
            if !state.bootstrap_intents.ptr_eq(&base.bootstrap_intents) || state.schema != base.schema {
                validate_snapshot(&state)?;
            }
            write_snapshot(&self.root, &base, state)?;
        }
        Ok(value)
    }
}

pub(crate) fn request_sha256<T: Serialize>(value: &T) -> Result<Sha256, AgentManagementError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|_| AgentManagementError::InvalidRequest("request_not_serializable"))?;
    Sha256::new(format!("{:x}", Sha256Digest::digest(bytes)))
        .map_err(|_| AgentManagementError::InvalidStore)
}

pub(crate) fn now() -> Rfc3339 {
    canonical_timestamp(chrono::Utc::now())
}

fn canonical_timestamp(instant: chrono::DateTime<chrono::Utc>) -> Rfc3339 {
    Rfc3339::new(instant.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true))
        .expect("UTC timestamp is RFC3339")
}

fn prepare_private_root(root: &Path) -> Result<(), AgentManagementError> {
    match fs::symlink_metadata(root) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(AgentManagementError::InvalidStore)
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(root).map_err(|_| AgentManagementError::PersistenceUnavailable)?
        }
        Err(_) => return Err(AgentManagementError::PersistenceUnavailable),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))
            .map_err(|_| AgentManagementError::PersistenceUnavailable)?;
    }
    Ok(())
}

/// Commit identity of the SQL roster (`epoch`, `change_seq`). Watchers
/// compare it where they used to stamp the legacy JSON file.
pub type RosterStamp = (String, i64);

/// The roster stamp for the `agent-management/v1` directory `v1_root`;
/// `None` while no authoritative database exists.
pub fn roster_stamp(v1_root: &Path) -> Option<RosterStamp> {
    let path = super::roster_sql::database_path(v1_root);
    if !path.exists() {
        return None;
    }
    with_read_connection(&path, |db| {
        Ok(super::roster_sql::meta(db)?
            .filter(|meta| meta.verified)
            .map(|meta| (meta.epoch, meta.change_seq)))
    })
    .ok()
    .flatten()
}

/// The committed roster generation this process last decoded, keyed by
/// database path and import epoch. A reader whose token is current shares the
/// `Arc`; after another writer's commit only the changed rows are decoded
/// from the change log (058 S4). Project authority/state changes reload in
/// full because `migrate_project_states` derives rows from both.
struct CachedSnapshot {
    path: PathBuf,
    directory: bool,
    epoch: String,
    change_seq: i64,
    /// `project_states` keys that are read-time materialisations, not rows;
    /// the next write persists them (legacy semantics). Usually empty.
    unstored_project_states: std::collections::BTreeSet<ProjectId>,
    state: Arc<AgentManagementSnapshot>,
}

/// One entry per roster directory (production has one; tests open many).
static SNAPSHOT_CACHE: Mutex<Vec<CachedSnapshot>> = Mutex::new(Vec::new());
const SNAPSHOT_CACHE_ENTRIES: usize = 8;

fn remember(cache: &mut Vec<CachedSnapshot>, entry: CachedSnapshot) {
    cache.retain(|old| old.path != entry.path || old.directory != entry.directory);
    if cache.len() >= SNAPSHOT_CACHE_ENTRIES {
        cache.remove(0);
    }
    cache.push(entry);
}
static READ_CONNECTION: Mutex<Option<(PathBuf, rusqlite::Connection)>> = Mutex::new(None);

fn sql_error(error: impl std::fmt::Display) -> AgentManagementError {
    eprintln!("cutex: agent management roster database: {error}");
    AgentManagementError::PersistenceUnavailable
}

fn with_read_connection<T>(
    path: &Path,
    read: impl FnOnce(&rusqlite::Connection) -> anyhow::Result<T>,
) -> Result<T, AgentManagementError> {
    // Only the process's default roster keeps a read connection open:
    // Windows cannot remove or replace an explicit (test/kit) store directory
    // while a handle to its database is held.
    static DEFAULT_DATABASE: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    let default = DEFAULT_DATABASE.get_or_init(|| {
        crate::config::paths::runtime_dir()
            .ok()
            .map(|runtime| super::roster_sql::database_path(&runtime.join("agent-management").join("v1")))
    });
    if default.as_deref() != Some(path) {
        let db = super::roster_sql::open_ro(path).map_err(sql_error)?;
        return read(&db).map_err(sql_error);
    }
    let mut cached = READ_CONNECTION.lock().unwrap_or_else(|e| e.into_inner());
    if cached.as_ref().is_none_or(|(cached, _)| cached != path) {
        *cached = Some((path.to_path_buf(), super::roster_sql::open_ro(path).map_err(sql_error)?));
    }
    let db = &cached.as_ref().expect("installed above").1;
    read(db).map_err(sql_error)
}

fn sql_authoritative(root: &Path) -> Result<bool, AgentManagementError> {
    let path = super::roster_sql::database_path(root);
    if !path.exists() {
        return Ok(false);
    }
    with_read_connection(&path, |db| {
        Ok(super::roster_sql::meta(db)?.is_some_and(|meta| meta.verified))
    })
}

/// Caller holds the exclusive store lock.
fn ensure_imported(root: &Path) -> Result<(), AgentManagementError> {
    if sql_authoritative(root)? {
        return Ok(());
    }
    forget_cached(root);
    *READ_CONNECTION.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let report = super::roster_sql::import_legacy(root).map_err(|error| {
        eprintln!("cutex: agent management roster import failed: {error:#}");
        AgentManagementError::InvalidStore
    })?;
    if !report.already_authoritative {
        eprintln!(
            "cutex: agent management roster imported into SQLite: {} rows, {} bytes, revision {}, {} ms",
            report.rows, report.source_bytes, report.store_revision, report.millis
        );
    }
    Ok(())
}

/// The read-time `migrate_project_states`, returning the ids it added
/// (projects without a stored state row). O(projects), never history.
fn materialise_project_states(
    state: &mut AgentManagementSnapshot,
) -> std::collections::BTreeSet<ProjectId> {
    let missing = state
        .projects
        .values()
        .filter(|authority| !state.project_states.contains_key(&authority.project_id))
        .map(|authority| authority.project_id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    if !missing.is_empty() {
        super::projects::migrate_project_states(state);
    }
    missing
}

fn decoded(
    path: &Path,
    meta: super::roster_sql::RosterMeta,
    mut raw: AgentManagementSnapshot,
    directory: bool,
) -> CachedSnapshot {
    let unstored_project_states = materialise_project_states(&mut raw);
    CachedSnapshot {
        path: path.to_path_buf(),
        directory,
        epoch: meta.epoch,
        change_seq: meta.change_seq,
        unstored_project_states,
        state: Arc::new(raw),
    }
}

fn read_snapshot_shared(root: &Path) -> Result<Arc<AgentManagementSnapshot>, AgentManagementError> {
    read_snapshot_view(root, false)
}

fn read_snapshot_view(root: &Path, directory: bool) -> Result<Arc<AgentManagementSnapshot>, AgentManagementError> {
    read_snapshot_selection(root, directory, &[], &[])
}

fn read_snapshot_selection(root: &Path, directory: bool, action_ids: &[AgentActionId], external_ids: &[String]) -> Result<Arc<AgentManagementSnapshot>, AgentManagementError> {
    let path = super::roster_sql::database_path(root);
    if !path.exists() {
        return Ok(Arc::new(AgentManagementSnapshot::empty()));
    }
    let mut cache = SNAPSHOT_CACHE.lock().map_err(|_| AgentManagementError::PersistenceUnavailable)?;
    let current = cache.iter().find(|entry| entry.path == path && entry.directory == directory);
    let (refreshed, selected_actions) = with_read_connection(&path, |db| {
        // Metadata and changed rows from one WAL snapshot.
        db.execute_batch("BEGIN DEFERRED")?;
        let result = (|| {
            let refreshed: anyhow::Result<Option<CachedSnapshot>> = (|| {
            let meta = super::roster_sql::meta(db)?.context("roster metadata missing")?;
            anyhow::ensure!(meta.verified, "roster database is not authoritative");
            let Some(entry) = current.filter(|entry| entry.epoch == meta.epoch) else {
                let (meta, raw) = if directory { super::roster_sql::load_directory(db)? } else { super::roster_sql::load_all(db)? };
                return Ok(Some(decoded(&path, meta, raw, directory)));
            };
            if entry.change_seq == meta.change_seq {
                return Ok(None);
            }
            let Some(changed) = super::roster_sql::changes_since(db, entry.change_seq)? else {
                let (meta, raw) = if directory { super::roster_sql::load_directory(db)? } else { super::roster_sql::load_all(db)? };
                return Ok(Some(decoded(&path, meta, raw, directory)));
            };
            // Persistent maps: this clone shares every node; applying the
            // changed rows copies only their paths.
            let mut next = (*entry.state).clone();
            next.store_revision = meta.store_revision;
            next.schema = serde_json::from_value(serde_json::Value::String(meta.schema.clone()))?;
            let projects_changed =
                changed.iter().any(|(field, _)| field == "projects" || field == "project_states");
            if projects_changed {
                // Back to stored rows only, then re-run the read-time migration.
                for id in &entry.unstored_project_states {
                    next.project_states.remove(id);
                }
            }
            for (field, key) in &changed {
                if directory && !super::roster_sql::directory_field(field) { continue; }
                let spec = super::roster_sql::spec_by_field(field).context("unknown roster collection")?;
                let body = super::roster_sql::get_body(db, spec, key)?;
                super::roster_sql::apply_row(&mut next, field, key, body.as_deref())?;
            }
            if directory && changed.iter().any(|(field, _)| field == "actions" || field == "phase_events") {
                next.legacy_close_projection = Some(Arc::new(super::roster_sql::legacy_close_evidence(db)?));
            }
            let unstored_project_states = if projects_changed {
                materialise_project_states(&mut next)
            } else {
                entry.unstored_project_states.clone()
            };
            Ok(Some(CachedSnapshot {
                path: path.clone(),
                directory,
                epoch: meta.epoch,
                change_seq: meta.change_seq,
                unstored_project_states,
                state: Arc::new(next),
            }))
            })();
            let refreshed = refreshed?;
            let mut selected = super::roster_sql::action_rows_for_starts(db, external_ids)?;
            let spec = super::roster_sql::spec_by_field("actions").context("action collection")?;
            for id in action_ids {
                if let Some(body) = super::roster_sql::get_body(db, spec, id.as_str())? {selected.insert(id.as_str().to_owned(), body);}
            }
            Ok((refreshed, selected))
        })();
        db.execute_batch("COMMIT")?;
        result
    })?;
    if let Some(entry) = refreshed {
        remember(&mut cache, entry);
    }
    let base = cache.iter().find(|entry| entry.path == path && entry.directory == directory)
        .expect("cache filled above").state.clone();
    if selected_actions.is_empty() {return Ok(base);}
    let mut projected = (*base).clone();
    for (key, body) in selected_actions {
        super::roster_sql::apply_row(&mut projected, "actions", &key, Some(&body)).map_err(sql_error)?;
    }
    Ok(Arc::new(projected))
}

/// Drop the cached generation of one roster directory only.
pub(super) fn forget_cached(root: &Path) {
    let path = super::roster_sql::database_path(root);
    if let Ok(mut cache) = SNAPSHOT_CACHE.lock() {
        cache.retain(|entry| entry.path != path);
    }
}

/// The previous build's reader checks, shared by the importer, the exporter
/// round-trip and every commit.
pub(crate) fn validate_snapshot(state: &AgentManagementSnapshot) -> Result<(), AgentManagementError> {
    if !state.bootstrap_intents.is_empty() && state.schema != AgentManagementStoreSchema::V2 {
        return Err(AgentManagementError::InvalidStore);
    }
    for (action, intent) in &state.bootstrap_intents {
        if &intent.request.action_id != action || intent.spec().is_err() {
            return Err(AgentManagementError::InvalidStore);
        }
    }
    Ok(())
}

/// Commit only the rows that differ from the generation the caller read.
fn write_snapshot(
    root: &Path,
    base: &AgentManagementSnapshot,
    state: AgentManagementSnapshot,
) -> Result<(), AgentManagementError> {
    let path = super::roster_sql::database_path(root);
    let mut changes = super::roster_sql::diff(base, &state).map_err(sql_error)?;
    let unstored = SNAPSHOT_CACHE
        .lock()
        .ok()
        .and_then(|cache| cache.iter().find(|e| e.path == path && !e.directory).map(|e| e.unstored_project_states.clone()))
        .unwrap_or_default();
    // Legacy writes persisted read-time project-state materialisations
    // (an edited one is already in the diff).
    let materialised = unstored
        .iter()
        .filter_map(|id| Some((id, state.project_states.get(id)?)))
        .filter(|(id, value)| base.project_states.get(*id) == Some(*value))
        .map(|(id, value)| (id.clone(), value.clone()))
        .collect::<RosterMap<_, _>>();
    if !materialised.is_empty() {
        let mut only = AgentManagementSnapshot::empty();
        only.project_states = materialised;
        changes.extend(super::roster_sql::diff(&AgentManagementSnapshot::empty(), &only).map_err(sql_error)?);
    }
    if let Ok(mut cache) = SNAPSHOT_CACHE.lock() {
        cache.retain(|entry| entry.path != path);
    }
    crate::director_address::invalidate_authority_cache();
    let meta = super::roster_sql::commit(&path, base.store_revision, &state.schema, state.store_revision, &changes)
        .map_err(sql_error)?;
    crate::director_address::invalidate_authority_cache();
    // Every key in the written state is now a row; then the same read-time
    // migration a fresh load applies (new projects get their default state),
    // needed only when project authority or state changed.
    let mut state = state;
    let unstored_project_states = if state.projects.ptr_eq(&base.projects)
        && state.project_states.ptr_eq(&base.project_states)
    {
        Default::default()
    } else {
        materialise_project_states(&mut state)
    };
    if let Ok(mut cache) = SNAPSHOT_CACHE.lock() {
        remember(&mut cache, CachedSnapshot {
            path,
            directory: false,
            epoch: meta.epoch,
            change_seq: meta.change_seq,
            unstored_project_states,
            state: Arc::new(state),
        });
    }
    // Audit export is after-commit and retryable: a failure leaves the outbox.
    if let Err(error) = super::roster_sql::drain_audit(root, 4096) {
        eprintln!("cutex: agent management roster audit export pending: {error:#}");
    }
    Ok(())
}

fn set_private_open_options(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
}

/// Tests that hand-edit the previous build's JSON: drop the SQL roster so
/// the next locked access imports that JSON again (like an R1 handoff).
#[cfg(test)]
pub(crate) fn reimport_legacy_for_tests(root: &Path) {
    forget_cached(root);
    *READ_CONNECTION.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let path = super::roster_sql::database_path(root);
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

/// The committed raw roster (no read-time migration), as R1 would export it.
#[cfg(test)]
pub(crate) fn committed_for_tests(root: &Path) -> AgentManagementSnapshot {
    super::roster_sql::read_committed(root).unwrap().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "EVA-sized private roster scale probe"]
    fn waste_scale_a04_roster() {
        let root=std::env::temp_dir().join(format!("scale-roster-{}",uuid::Uuid::new_v4()));
        let store=AgentManagementStore::open(&root).unwrap();
        let fixture=crate::test_support::waste_scale::roster();
        let bytes=serde_json::to_vec_pretty(&fixture).unwrap().len();
        store.with_state(true,|_|Ok((fixture,(),true))).unwrap();
        let _=store.snapshot_shared().unwrap();
        let mut samples=Vec::new();
        for _ in 0..30 {let t=std::time::Instant::now();std::hint::black_box(store.snapshot_shared().unwrap());samples.push(t.elapsed());}
        crate::test_support::waste_scale::emit("a04_roster_snapshot",&samples,30,serde_json::json!({"serialized_bytes":bytes,"eva_target_bytes":crate::test_support::waste_scale::ROSTER_BYTES}));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn waste_guard_roster_shared_read_has_no_deep_copy_and_observes_external_commit(){
        let root=std::env::temp_dir().join(format!("guard-roster-{}",uuid::Uuid::new_v4()));
        let store=AgentManagementStore::open(&root).unwrap();
        store.with_state(true,|_|Ok((crate::test_support::waste_scale::roster_with_agents(),(),true))).unwrap();
        let before=store.snapshot_shared().unwrap();
        for _ in 0..64 {assert!(Arc::ptr_eq(&before,&store.snapshot_shared().unwrap()),"hot reads must share one parsed generation");}
        let other=AgentManagementStore::open(&root).unwrap();
        other.with_state(true,|mut state|{state.extra.insert("another-writer".into(),serde_json::json!(true));Ok((state,(),true))}).unwrap();
        let after=store.snapshot_shared().unwrap();
        assert!(!Arc::ptr_eq(&before,&after));assert_eq!(after.store_revision,before.store_revision+1);
        assert!(!before.extra.contains_key("another-writer"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn whole_second_timestamp_uses_canonical_zero_fraction_format() {
        let instant = chrono::DateTime::parse_from_rfc3339("2026-08-27T12:34:56.000Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            canonical_timestamp(instant).as_str(),
            "2026-08-27T12:34:56Z"
        );
    }

    #[test]
    fn cached_snapshot_follows_own_and_foreign_writes() {
        let root = std::env::temp_dir().join(format!(
            "cutex-agent-management-cache-{}",
            uuid::Uuid::new_v4()
        ));
        let store = AgentManagementStore::open(&root).expect("open");
        let other = AgentManagementStore::open(&root).expect("open again");
        let bump = |store: &AgentManagementStore| {
            store
                .with_state(true, |state| Ok((state, (), true)))
                .expect("write")
        };
        bump(&store);
        assert_eq!(store.snapshot().unwrap().store_revision, 1);
        assert_eq!(other.snapshot().unwrap().store_revision, 1, "cache hit");
        bump(&other);
        assert_eq!(store.snapshot().unwrap().store_revision, 2);
        // A writer in another process commits rows without touching this
        // process's cache: the change log is enough.
        let current = store.snapshot().unwrap();
        let mut foreign = current.clone();
        foreign.store_revision = 7;
        foreign.extra.insert("foreign".into(), serde_json::json!(1));
        let changes = super::super::roster_sql::diff(&current, &foreign).unwrap();
        super::super::roster_sql::commit(
            &super::super::roster_sql::database_path(&root),
            current.store_revision,
            &foreign.schema,
            7,
            &changes,
        )
        .unwrap();
        assert_eq!(store.snapshot().unwrap().store_revision, 7);
        assert_eq!(store.snapshot().unwrap().extra["foreign"], 1);
        // A read-only with_state hands out its own copy: changing it does not
        // leak into the cache.
        store
            .with_state(false, |mut state| {
                state.store_revision = 99;
                Ok((state, (), false))
            })
            .unwrap();
        assert_eq!(store.snapshot().unwrap().store_revision, 7);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn delivery_try_lock_reports_a_held_mutation_lock_as_busy() {
        let root = std::env::temp_dir().join(format!(
            "cutex-agent-management-try-lock-{}",
            uuid::Uuid::new_v4()
        ));
        let store = AgentManagementStore::open(&root).expect("open");
        let held = store.lock_mutations().expect("lock");
        // Windows: ERROR_LOCK_VIOLATION (33) is busy, not PersistenceUnavailable.
        assert!(store.try_lock_delivery_mutations().expect("busy, not an error").is_none());
        drop(held);
        assert!(store.try_lock_delivery_mutations().expect("free").is_some());
        fs::remove_dir_all(root).expect("cleanup");
    }

    /// r271 guard (a): never block on the fence while holding a session lane
    /// (lifecycle takes lanes after the fence: native stop).
    #[cfg(debug_assertions)]
    #[test]
    fn blocking_on_the_fence_while_holding_a_lane_panics() {
        let root = std::env::temp_dir().join(format!("cutex-agent-management-lane-order-{}", uuid::Uuid::new_v4()));
        let store = AgentManagementStore::open(&root).expect("open");
        let lane = crate::management::v2::session_coordination::session_lane(&uuid::Uuid::new_v4().to_string()).unwrap();
        {
            let _lane = lane.reserve().unwrap();
            let blocked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| store.lock_mutations().map(|_| ())));
            // Suite gates count warnings instead (CUTEX_LOCK_ORDER_GUARD=log):
            // this deliberate violation is then their one expected warning.
            assert_eq!(blocked.is_err(), !lock_order_guard_logs(), "fence wait under a lane must trip the guard");
            // Trying is fine: it never waits.
            assert!(store.try_lock_delivery_mutations().unwrap().is_some());
        }
        // Fence first, lane second (lifecycle order) is allowed.
        let _fence = store.lock_mutations().expect("no lane held");
        let _lane = lane.reserve().unwrap();
        drop(_lane);
        drop(_fence);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn empty_store_reopens_with_private_versioned_snapshot() {
        let root = std::env::temp_dir().join(format!(
            "cutex-agent-management-store-{}",
            uuid::Uuid::new_v4()
        ));
        let store = AgentManagementStore::open(&root).expect("open");
        assert_eq!(store.snapshot().expect("snapshot").store_revision, 0);
        store
            .with_state(true, |mut state| {
                state.store_revision = 0;
                Ok((state, (), true))
            })
            .expect("write");
        drop(store);
        let reopened = AgentManagementStore::open(&root).expect("reopen");
        assert_eq!(reopened.snapshot().expect("snapshot").store_revision, 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&root).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(super::super::roster_sql::database_path(&root))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn accepted_single_director_v1_snapshot_migrates_to_empty_operator_grants() {
        let mut value = serde_json::to_value(AgentManagementSnapshot::empty()).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("legacy_director_ownership_import_receipts");
        object.remove("reservation_reconciliation_receipts");
        object.remove("reservation_reconciliation_events");
        object.remove("operator_grants");
        object.remove("operator_grant_revisions");
        object.remove("operator_audit_events");
        object.insert(
            "future_additive_field".to_string(),
            serde_json::json!({"kept": true}),
        );

        let snapshot: AgentManagementSnapshot =
            serde_json::from_value(value).expect("accepted snapshot remains compatible");
        assert!(snapshot
            .legacy_director_ownership_import_receipts
            .is_empty());
        assert!(snapshot.reservation_reconciliation_receipts.is_empty());
        assert!(snapshot.reservation_reconciliation_events.is_empty());
        assert!(snapshot.operator_grants.is_empty());
        assert!(snapshot.operator_grant_revisions.is_empty());
        assert!(snapshot.operator_audit_events.is_empty());
        assert_eq!(snapshot.extra["future_additive_field"]["kept"], true);
    }
}
