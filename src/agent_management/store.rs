use std::collections::BTreeMap;
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256 as Sha256Digest};

use crate::config::atomic::write_private_pretty_json_atomic;
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentManagementSnapshot {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bootstrap_intents: BTreeMap<AgentActionId, super::BootstrapIntentReview>,
    /// Read-only durable projection, never persisted into roster history.
    #[serde(skip)]
    pub reversible_archive_projection: BTreeMap<crate::role_revision::CutexSessionId, bool>,
    #[serde(default)]
    pub agent_archive_actions: BTreeMap<AgentActionId, super::AgentArchiveReceipt>,
    #[serde(default)]
    pub agent_archive_audit: BTreeMap<String, super::AgentArchiveReceipt>,
    pub schema: AgentManagementStoreSchema,
    pub store_revision: u64,
    #[serde(default)]
    pub durable_import_actions: BTreeMap<AgentActionId, super::DurableImportReceipt>,
    #[serde(default)]
    pub durable_import_audit: BTreeMap<String, super::DurableImportAuditEvent>,
    pub projects: BTreeMap<ProjectId, ProjectAuthority>,
    /// Exact project/session keyed Agent Operator grants. Missing on legacy v1
    /// stores and therefore migrated as an empty grant set.
    #[serde(default)]
    pub operator_grants:
        BTreeMap<ProjectId, BTreeMap<crate::role_revision::CutexSessionId, AgentOperatorGrant>>,
    /// Per-project CAS revision for the complete Operator grant set.
    #[serde(default)]
    pub operator_grant_revisions: BTreeMap<ProjectId, u64>,
    #[serde(default)]
    pub operator_audit_events: BTreeMap<String, AgentOperatorAuditEvent>,
    /// Idempotency receipts for the separate Human/Management action domain.
    /// These records never impersonate an Agent Management action caller.
    #[serde(default)]
    pub human_management_operator_actions:
        BTreeMap<AgentActionId, HumanManagementOperatorActionRecord>,
    /// Idempotent Human/Management Project structure mutations.
    #[serde(default)]
    pub human_management_project_mutations:
        BTreeMap<AgentActionId, HumanManagementProjectMutationActionRecord>,
    /// Additive current membership overrides. Missing entries retain the
    /// historical ManagedAgentRecord.project_id interpretation for active
    /// legacy Agents; writes never rewrite that historical provenance.
    #[serde(default)]
    pub current_project_memberships:
        BTreeMap<crate::role_revision::CutexSessionId, CurrentProjectMembership>,
    /// Lifecycle/CAS records for Project structure. Missing legacy records are
    /// projected as active at revision zero without forcing a migration write.
    #[serde(default)]
    pub project_states: BTreeMap<ProjectId, ProjectStateRecord>,
    #[serde(default)]
    pub project_tombstones: BTreeMap<ProjectId, ProjectTombstone>,
    #[serde(default)]
    pub project_audit_events: BTreeMap<String, ProjectAuditEvent>,
    /// Non-authoritative UI metadata. Its map key is always the canonical
    /// provider-owned project identity; none of these values participate in
    /// authorization.
    #[serde(default)]
    pub project_presentations: BTreeMap<ProjectId, super::ProjectPresentationSettings>,
    pub agents: BTreeMap<crate::role_revision::CutexSessionId, ManagedAgentRecord>,
    pub actions: BTreeMap<AgentActionId, AgentActionRecord>,
    #[serde(default)]
    pub phase_events: BTreeMap<String, AgentManagementPhaseEvent>,
    pub authority_receipts: BTreeMap<AgentActionId, ProjectAuthorityReceipt>,
    #[serde(default)]
    pub legacy_director_ownership_import_receipts:
        BTreeMap<AgentActionId, LegacyDirectorOwnershipImportReceipt>,
    #[serde(default)]
    pub reservation_reconciliation_receipts:
        BTreeMap<AgentActionId, AgentReservationReconciliationReceipt>,
    #[serde(default)]
    pub reservation_reconciliation_events: BTreeMap<String, AgentReservationReconciliationEvent>,
    pub failure_events: BTreeMap<String, AgentManagementFailureEvent>,
    /// Sparse `agent_create` / `member_replace` calls and the full request
    /// each resolved to (`sparse.rs`), keyed by action id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sparse_requests: BTreeMap<AgentActionId, super::SparseRequestRecord>,
    /// Preserve additive provider fields across presentation-only writes so a
    /// newer store is not silently downgraded by this reader.
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl AgentManagementSnapshot {
    pub(crate) fn empty() -> Self {
        Self {
            bootstrap_intents: BTreeMap::new(),
            reversible_archive_projection: BTreeMap::new(),
            agent_archive_actions: BTreeMap::new(),
            agent_archive_audit: BTreeMap::new(),
            schema: AgentManagementStoreSchema::V1,
            store_revision: 0,
            durable_import_actions: BTreeMap::new(),
            durable_import_audit: BTreeMap::new(),
            projects: BTreeMap::new(),
            operator_grants: BTreeMap::new(),
            operator_grant_revisions: BTreeMap::new(),
            operator_audit_events: BTreeMap::new(),
            human_management_operator_actions: BTreeMap::new(),
            human_management_project_mutations: BTreeMap::new(),
            current_project_memberships: BTreeMap::new(),
            project_states: BTreeMap::new(),
            project_tombstones: BTreeMap::new(),
            project_audit_events: BTreeMap::new(),
            project_presentations: BTreeMap::new(),
            agents: BTreeMap::new(),
            actions: BTreeMap::new(),
            phase_events: BTreeMap::new(),
            authority_receipts: BTreeMap::new(),
            legacy_director_ownership_import_receipts: BTreeMap::new(),
            reservation_reconciliation_receipts: BTreeMap::new(),
            reservation_reconciliation_events: BTreeMap::new(),
            failure_events: BTreeMap::new(),
            sparse_requests: BTreeMap::new(),
            extra: BTreeMap::new(),
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
        // Read-only: hand out the loaded state itself (no second copy).
        self.with_state(false, |state| Ok((AgentManagementSnapshot::empty(), state, false)))
    }

    /// Holds the provider-wide mutation fence across authorization checks and
    /// external lifecycle effects. The process mutex in the provider covers
    /// threads; this OS lock extends the same ordering across processes.
    pub(crate) fn lock_mutations(&self) -> Result<File, AgentManagementError> {
        let path = self.root.join(MUTATION_LOCK_FILE);
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        set_private_open_options(&mut options);
        let lock = options
            .open(path)
            .map_err(|_| AgentManagementError::PersistenceUnavailable)?;
        FileExt::lock_exclusive(&lock).map_err(|_| AgentManagementError::PersistenceUnavailable)?;
        Ok(lock)
    }

    /// Delivery must not wait behind a lifecycle action which is joining its
    /// bridge worker. Return pending instead; the next occurrence reconciles.
    pub(crate) fn try_lock_delivery_mutations(&self) -> Result<Option<File>, AgentManagementError> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        set_private_open_options(&mut options);
        let lock = options
            .open(self.root.join(MUTATION_LOCK_FILE))
            .map_err(|_| AgentManagementError::PersistenceUnavailable)?;
        match FileExt::try_lock_exclusive(&lock) {
            Ok(()) => Ok(Some(lock)),
            // Windows reports a held lock as ERROR_LOCK_VIOLATION, not WouldBlock.
            Err(error) if crate::platform::file_lock::lock_contended(&error) => Ok(None),
            Err(_) => Err(AgentManagementError::PersistenceUnavailable),
        }
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
        let state = read_snapshot(&self.root)?;
        let (mut state, value, write) = operation(state)?;
        if write {
            state.store_revision = state
                .store_revision
                .checked_add(1)
                .filter(|revision| *revision <= crate::role_revision::MAX_JSON_SAFE_INTEGER)
                .ok_or(AgentManagementError::Conflict("store_revision_overflow"))?;
            write_snapshot(&self.root, &state)?;
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

/// The last parsed and validated store, keyed by path and file identity.
/// Readers poll it (asks ticks, bridge deliveries, session lists): the 7.5 MB
/// file was parsed several times a second on the Linux host (2026-10-09). Every
/// writer replaces the file atomically under the store lock, so a new
/// version has a new stamp; a copy of the cached state is still much cheaper
/// than parsing and validating the file.
struct CachedSnapshot {
    path: PathBuf,
    stamp: crate::platform::file_stamp::FileStamp,
    state: AgentManagementSnapshot,
}

static SNAPSHOT_CACHE: Mutex<Option<CachedSnapshot>> = Mutex::new(None);

fn read_snapshot(root: &Path) -> Result<AgentManagementSnapshot, AgentManagementError> {
    use crate::platform::file_stamp::FileStamp;
    let path = root.join(STORE_FILE);
    let stamp = FileStamp::of(&path);
    let Ok(mut cache) = SNAPSHOT_CACHE.lock() else {
        return read_snapshot_file(&path);
    };
    if let (Some(entry), Some(stamp)) = (cache.as_ref(), stamp.as_ref()) {
        if entry.path == path && entry.stamp == *stamp {
            return Ok(entry.state.clone());
        }
    }
    *cache = None;
    let state = read_snapshot_file(&path)?;
    match (stamp, FileStamp::of(&path)) {
        (Some(before), Some(after)) if before == after => {
            *cache = Some(CachedSnapshot {
                path,
                stamp: before,
                state: state.clone(),
            });
        }
        _ => {}
    }
    Ok(state)
}

pub(super) fn invalidate_snapshot_cache() {
    if let Ok(mut cache) = SNAPSHOT_CACHE.lock() {
        *cache = None;
    }
}

fn read_snapshot_file(path: &Path) -> Result<AgentManagementSnapshot, AgentManagementError> {
    match fs::read(path) {
        Ok(bytes) => {
            let mut state: AgentManagementSnapshot =
                serde_json::from_slice(&bytes).map_err(|_| AgentManagementError::InvalidStore)?;
            super::projects::migrate_project_states(&mut state);
            if !state.bootstrap_intents.is_empty() && state.schema != AgentManagementStoreSchema::V2
            {
                return Err(AgentManagementError::InvalidStore);
            }
            for (action, intent) in &state.bootstrap_intents {
                if &intent.request.action_id != action || intent.spec().is_err() {
                    return Err(AgentManagementError::InvalidStore);
                }
            }
            Ok(state)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Ok(AgentManagementSnapshot::empty())
        }
        Err(_) => Err(AgentManagementError::PersistenceUnavailable),
    }
}

fn write_snapshot(
    root: &Path,
    snapshot: &AgentManagementSnapshot,
) -> Result<(), AgentManagementError> {
    invalidate_snapshot_cache();
    crate::director_address::invalidate_authority_cache();
    write_private_pretty_json_atomic(&root.join(STORE_FILE), snapshot, "agent management store")
        .map_err(|_| AgentManagementError::PersistenceUnavailable)?;
    crate::director_address::invalidate_authority_cache();
    #[cfg(unix)]
    File::open(root)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| AgentManagementError::PersistenceUnavailable)?;
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

#[cfg(test)]
mod tests {
    use super::*;

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
        // A writer in another process replaces the file without touching the
        // cache: the new file identity is enough.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut foreign = read_snapshot_file(&root.join(STORE_FILE)).unwrap();
        foreign.store_revision = 7;
        write_private_pretty_json_atomic(&root.join(STORE_FILE), &foreign, "fixture").unwrap();
        assert_eq!(store.snapshot().unwrap().store_revision, 7);
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
                fs::metadata(root.join(STORE_FILE))
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
