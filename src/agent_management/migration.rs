//! One-shot, authenticated Human maintenance of the fixed selected cohort.
//! This is not an Agent lifecycle privilege or a second authority store.
use super::migration_files as files;
pub use super::migration_files::MigrationFile;
use super::*;
use crate::launch::stock::{StockBundle, StockConfiguration, VerifiedFile, current_configuration};
use crate::management::control_plane::HumanManagementPrincipal;
use crate::role_revision::{CutexSessionId, Sha256};
use crate::session::model::{
    CutexSessionRecord, CutexSessionRuntimeBackend, CutexSessionUserAction,
};
use crate::session::store::{
    load_cutex_session_store_from_path, save_locked_session_store, with_locked_session_store,
};
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const MAX_HISTORY: u64 = 2 * 1024 * 1024 * 1024;
const MAX_CONFIG: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceCatalog {
    pub native_id: String,
    pub rollout_path: PathBuf,
    pub memory_mode: String,
    pub history_mode: String,
}

fn catalog_metadata(home: &Path, native: &str) -> anyhow::Result<MaintenanceCatalog> {
    catalog_metadata_in(home, native, &std::env::temp_dir())
}

fn catalog_metadata_in(
    home: &Path,
    native: &str,
    parent: &Path,
) -> anyhow::Result<MaintenanceCatalog> {
    let db = MigrationFile::capture(&home.join("state_5.sqlite"), MAX_HISTORY)?;
    let wal_path = home.join("state_5.sqlite-wal");
    let wal = match std::fs::symlink_metadata(&wal_path) {
        Ok(_) => Some(MigrationFile::capture(&wal_path, MAX_HISTORY)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let parent_fd = files::directory(&parent, false)?;
    let child = format!("cutex-catalog-{}", uuid::Uuid::new_v4());
    let scratch = parent.join(&child);
    ensure!(
        !scratch.starts_with(home) && !home.starts_with(&scratch),
        "catalog scratch/source overlap"
    );
    let held = files::mkdir(&parent_fd, &child)?;
    let result = (|| {
        db.copy_to(&held, "state_5.sqlite")?;
        if let Some(wal) = &wal {
            wal.copy_to(&held, "state_5.sqlite-wal")?;
        }
        let result = read_catalog_copy(&scratch, native, wal.as_ref().map_or(0, |w| w.length))?;
        db.validate()?;
        if let Some(wal) = &wal {
            wal.validate()?;
        } else {
            ensure!(
                matches!(std::fs::symlink_metadata(&wal_path),Err(e) if e.kind()==std::io::ErrorKind::NotFound),
                "source WAL appeared during capture"
            );
        }
        Ok(result)
    })();
    files::remove_catalog_scratch(&parent, &child, &held)?;
    result
}

fn read_catalog_copy(
    home: &Path,
    native: &str,
    wal_bytes: u64,
) -> anyhow::Result<MaintenanceCatalog> {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    // Existing system Python's sqlite reader, no dependency installation or
    // native build. Isolated mode excludes user site, PYTHONPATH and startup.
    let mut child = Command::new("/usr/bin/python3")
        .args(["-I", "-S", "-c", include_str!("migration_catalog.py")])
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let request = serde_json::to_vec(
        &serde_json::json!({"path":home.join("state_5.sqlite"),"native_id":native,"wal_bytes":wal_bytes}),
    )?;
    let mut input = child.stdin.take().context("catalog stdin")?;
    input.write_all(&request)?;
    drop(input);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            // This exact child handle is owned by this invocation only.
            child.kill()?;
            child.wait()?;
            anyhow::bail!("native catalog reader deadline; no migration");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    ensure!(
        status.success(),
        "native catalog reader failed; requires supported state_5 schema and /usr/bin/python3 sqlite3"
    );
    let mut bytes = Vec::new();
    child
        .stdout
        .take()
        .context("catalog stdout")?
        .take(8193)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 8192,
        "native catalog response bound exceeded"
    );
    let result: MaintenanceCatalog = serde_json::from_slice(&bytes)?;
    ensure!(
        result.native_id == native
            && result.memory_mode == "enabled"
            && matches!(result.history_mode.as_str(), "legacy" | "paginated"),
        "selected catalog semantics changed/unsupported; not silently reconstructed"
    );
    Ok(result)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceReviewRequest {
    pub action_id: AgentActionId,
    pub cutex_session_id: CutexSessionId,
    pub destination: PathBuf,
    pub bundle: StockBundle,
    pub expires_at_unix: i64,
    #[serde(default)]
    pub job_mcp: Option<crate::launch::job_mcp::JobMcpDescriptor>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceReview {
    pub version: u32,
    pub action_id: AgentActionId,
    pub subject: ExplicitLaunchSubject,
    pub expires_at_unix: i64,
    pub preserve_offline: bool,
    pub source_home: PathBuf,
    pub history: MigrationFile,
    pub shared: MigrationFile,
    pub catalog_files: Vec<MigrationFile>,
    pub catalog: MaintenanceCatalog,
    /// Only native's established noncredential skill/memory roots. Not a home
    /// clone, profile import or arbitrary plugin/config copy operation.
    pub assets: Vec<MigrationFile>,
    pub destination_parent_device: u64,
    pub destination_parent_inode: u64,
    pub contract: ExplicitLaunchContract,
    pub bundle: StockBundle,
    pub shared_projection: String,
    pub configuration: StockConfiguration,
    pub job_mcp: Option<crate::launch::job_mcp::ReviewedJobMcp>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaintenancePhase {
    Preparing,
    Prepared,
    Applied,
    Activated,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceReceipt {
    pub review: MaintenanceReview,
    pub phase: MaintenancePhase,
    pub runtime_review: Option<StockRuntimeReview>,
    pub error: Option<String>,
}

/// Durable evidence that a migrated offline Agent has not started a native
/// occurrence. Package upgrades may change bundle paths and hashes, but never
/// the migration action, native identity, home, or runtime generation.
/// Callers must still verify current process/manager observations are offline.
pub fn has_unstarted_offline_migration(
    record: &CutexSessionRecord,
    store: &crate::session::model::CutexSessionStore,
) -> bool {
    if crate::session::archive::record_has_runtime_claim(record)
        || record.pending_launch_id.is_some()
    {
        return false;
    }
    let Some(contract) = &record.explicit_launch else {
        return false;
    };
    let Some(action) = &contract.migration_action_id else {
        return false;
    };
    let Some(ExplicitLaunchActionReceipt::Maintenance(receipt)) =
        store.explicit_launch_receipts.get(action.as_str())
    else {
        return false;
    };
    receipt.phase == MaintenancePhase::Applied
        && receipt.error.is_none()
        && receipt.review.preserve_offline
        && receipt.review.action_id == *action
        && receipt.review.subject.cutex_session_id.as_str() == record.cutex_session_id
        && receipt.review.subject.runtime_generation == record.runtime_generation
        && record.codex_session_id.as_deref() == Some(contract.native_id.as_str())
        && receipt.review.contract.migration_action_id.as_ref() == Some(action)
        && receipt.review.contract.native_id == contract.native_id
        && receipt.review.contract.native_home == contract.native_home
}

/// Human-sealed continuation for an already-applied maintenance migration whose
/// frozen runtime confirmation was invalidated after a UI selection write. This
/// does not create a new migration, identity, home or runtime action.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceRecoveryStartRequest {
    pub action_id: AgentActionId,
    pub frozen_durable_sha256: Sha256,
    pub observed_durable_sha256: Sha256,
    pub observed_last_user_selected_at: Option<String>,
    pub observed_last_user_action: Option<CutexSessionUserAction>,
    pub expires_at_unix: i64,
}
#[derive(Clone, Debug, Serialize)]
pub struct MaintenanceStatus {
    pub state: &'static str,
    pub migration: Option<MaintenanceReceipt>,
    pub runtime: Option<StockRuntimeReceipt>,
}

/// Sealed in this module, borrowed only by the genuine Human start entry.
/// Neither request JSON nor an Agent credential can construct this permit.
pub(super) struct MaintenancePermit<'a> {
    receipt: &'a MaintenanceReceipt,
    recovery: Option<&'a MaintenanceRecoveryStartRequest>,
}
impl MaintenancePermit<'_> {
    pub(super) fn validate(
        &self,
        path: &Path,
        action: &AgentActionId,
        review: &StockRuntimeReview,
        state: &AgentManagementSnapshot,
    ) -> anyhow::Result<()> {
        let receipt = self.receipt;
        let original = &receipt.review;
        ensure!(
            receipt.phase == MaintenancePhase::Applied
                && !original.preserve_offline
                && original.expires_at_unix > chrono::Utc::now().timestamp(),
            "maintenance start unavailable/expired; offline intent is preserved"
        );
        let frozen = receipt
            .runtime_review
            .as_ref()
            .context("maintenance frozen runtime review absent")?;
        if let Some(recovery) = self.recovery {
            ensure!(
                recovery.action_id == original.action_id
                    && recovery.frozen_durable_sha256 == frozen.subject.durable_sha256
                    && recovery.observed_durable_sha256 == review.subject.durable_sha256,
                "maintenance recovery digest/action mismatch"
            );
            let mut expected = frozen.clone();
            expected.subject.durable_sha256 = recovery.observed_durable_sha256.clone();
            ensure!(
                expected == *review,
                "maintenance recovery changed more than the durable confirmation"
            );
        } else {
            ensure!(frozen == review, "maintenance runtime review mismatch");
        }
        ensure!(
            *action == runtime_action(&original.action_id)? && !review.restart,
            "maintenance runtime action/review mismatch"
        );
        let store = load_cutex_session_store_from_path(path)?;
        ensure!(
            matches!(store.explicit_launch_receipts.get(original.action_id.as_str()),Some(ExplicitLaunchActionReceipt::Maintenance(r)) if r==receipt),
            "maintenance permit is not the committed Human receipt"
        );
        let (project, digest) = authority_digest(state, &original.subject.cutex_session_id)?;
        ensure!(
            Some(project) == original.subject.current_project_id
                && digest == original.subject.authority_sha256,
            "maintenance authority/task mutation; no continuation"
        );
        materialize(original)?;
        Ok(())
    }
}
fn runtime_action(action: &AgentActionId) -> anyhow::Result<AgentActionId> {
    AgentActionId::new(format!(
        "maintenance-start-{}",
        bytes_digest(action.as_str().as_bytes()).as_str()
    ))
    .map_err(|_| anyhow::anyhow!("maintenance action invalid"))
}

#[derive(Deserialize)]
struct Selection {
    rows: Vec<SelectedSubject>,
}
#[derive(Deserialize)]
struct SelectedSubject {
    durable_id: String,
    native_id: String,
    formal_name: String,
    project_id: String,
    classification: String,
    status: String,
}
fn selected(id: &CutexSessionId) -> anyhow::Result<SelectedSubject> {
    let selection: Selection =
        serde_json::from_str(include_str!("../../docs/project-recent14-candidates.json"))?;
    let selected: Vec<_> = selection
        .rows
        .into_iter()
        .filter(|r| r.classification == "selected")
        .collect();
    ensure!(selected.len() == 34, "compiled migration cohort invalid");
    selected
        .into_iter()
        .find(|r| r.durable_id == id.as_str())
        .context("target outside fixed selected34; damaged/retired/unassigned are excluded")
}

pub(super) fn record_digest(record: &CutexSessionRecord) -> anyhow::Result<Sha256> {
    let mut value = serde_json::to_value(record)?;
    let object = value.as_object_mut().context("record object")?;
    object.remove("last_seen_at");
    object.remove("updated_at");
    Ok(super::store::request_sha256(&value)?)
}

fn offline(record: &CutexSessionRecord) -> anyhow::Result<()> {
    ensure!(
        !record.is_retired()
            && record.agent_enabled
            && record.registration_class
                == crate::agent_bus::model::AgentRegistrationClass::Persistent,
        "migration requires active persistent durable Agent"
    );
    ensure!(
        !crate::session::archive::record_has_runtime_claim(record)
            && record.pending_launch_id.is_none(),
        "migration target must be offline with no owner, pending launch or unresolved claim; no automatic stop"
    );
    ensure!(
        crate::runtime::lifecycle::cutex_session_host_is_local(
            &record.host_id,
            &crate::platform::host::current_host_name()
        ),
        "migration target not local"
    );
    Ok(())
}

/// Hash only the target's Agent Management authority state (its agent row,
/// membership, project authority/state and the roles it holds), not store
/// sequence, unrelated projects or session heartbeats. r219: the Task Service
/// and its Director seat copy are gone, so neither is part of the digest.
pub(super) fn authority_digest(
    state: &AgentManagementSnapshot,
    id: &CutexSessionId,
) -> anyhow::Result<(ProjectId, Sha256)> {
    let agent = state
        .agents
        .get(id)
        .context("managed migration target missing")?;
    ensure!(agent.retired_at.is_none(), "retired target cannot migrate");
    let project = super::projects::current_project_id(state, agent)
        .context("migration requires current project")?;
    let authority = state
        .projects
        .get(&project)
        .context("project authority absent")?;
    ensure!(
        !state.project_tombstones.contains_key(&project)
            && state
                .project_states
                .get(&project)
                .is_none_or(|p| p.lifecycle == ProjectLifecycle::Active),
        "project inactive"
    );
    let directed: Vec<_> = state
        .projects
        .iter()
        .filter(|(_, p)| p.authorized_director_session == *id)
        .collect();
    let operators: Vec<_> = state
        .operator_grants
        .iter()
        .filter_map(|(p, grants)| {
            grants
                .get(id)
                .map(|g| (p, g, state.operator_grant_revisions.get(p)))
        })
        .collect();
    let digest = super::store::request_sha256(&(
        agent,
        state.current_project_memberships.get(id),
        authority,
        state.project_states.get(&project),
        state.operator_grants.get(&project),
        state.operator_grant_revisions.get(&project),
        (directed, operators),
    ))?;
    Ok((project, digest))
}

fn history(home: &Path, native: &str) -> anyhow::Result<MigrationFile> {
    let mut pending = vec![home.join("sessions")];
    let mut found = Vec::new();
    while let Some(dir) = pending.pop() {
        files::directory(&dir, false)?;
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            ensure!(!ty.is_symlink(), "symlink in source history tree");
            if ty.is_dir() {
                pending.push(entry.path());
            } else if entry
                .file_name()
                .to_string_lossy()
                .ends_with(&format!("{native}.jsonl"))
            {
                found.push(entry.path());
            }
        }
    }
    ensure!(found.len() == 1, "source native history missing/ambiguous");
    let file = MigrationFile::capture(&found[0], MAX_HISTORY)?;
    let metadata: serde_json::Value = serde_json::from_slice(&file.first_line(1024 * 1024)?)?;
    ensure!(
        metadata["type"] == "session_meta" && metadata["payload"]["id"] == native,
        "source native metadata identity mismatch"
    );
    Ok(file)
}

fn assets(home: &Path) -> anyhow::Result<Vec<MigrationFile>> {
    let mut pending = Vec::new();
    let mut result = Vec::new();
    let mut total = 0u64;
    for name in ["skills", "memories"] {
        let path = home.join(name);
        if path.try_exists()? {
            pending.push(path)
        }
    }
    while let Some(dir) = pending.pop() {
        files::directory(&dir, false)?;
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            ensure!(
                !ty.is_symlink(),
                "migration skills/memories symlink requires explicit asset decision"
            );
            if ty.is_dir() {
                pending.push(entry.path());
            } else {
                let file = MigrationFile::capture(&entry.path(), 32 * 1024 * 1024)?;
                total = total
                    .checked_add(file.length)
                    .context("asset size overflow")?;
                ensure!(
                    result.len() < 2048 && total <= 128 * 1024 * 1024,
                    "migration asset inventory exceeds bounded scope"
                );
                result.push(file);
            }
        }
    }
    result.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(result)
}

pub(crate) fn validate_migration_home(
    record: &CutexSessionRecord,
    sessions: &crate::session::model::CutexSessionStore,
    contract: &ExplicitLaunchContract,
) -> anyhow::Result<()> {
    ensure!(
        record.explicit_launch.as_ref() == Some(contract) && contract.version == 3,
        "migration marker mismatch"
    );
    let action = contract
        .migration_action_id
        .as_ref()
        .context("migration action missing")?;
    let Some(ExplicitLaunchActionReceipt::Maintenance(receipt)) =
        sessions.explicit_launch_receipts.get(action.as_str())
    else {
        anyhow::bail!("committed Human migration receipt absent")
    };
    ensure!(
        matches!(
            receipt.phase,
            MaintenancePhase::Applied | MaintenancePhase::Activated
        ) && receipt.review.contract.native_id == contract.native_id
            && receipt.review.contract.native_home == contract.native_home
            && receipt.review.contract.migration_action_id == contract.migration_action_id
            && receipt.review.action_id == *action
            && receipt.review.subject.cutex_session_id.as_str() == record.cutex_session_id,
        "migration receipt/home owner mismatch"
    );
    // The committed receipt establishes ownership, not a permanent freeze on
    // package selection. Human configuration may select a rebuilt bundle while
    // retaining the same native history. The current bundle is validated by
    // StockBundle::load; historical package bytes are not launch authority.
    // Likewise a restored filesystem can have new device/inode numbers. Check
    // current private directory ownership instead of the migration-time inode.
    files::directory(
        contract
            .native_home
            .parent()
            .context("migration parent missing")?,
        true,
    )?;
    files::directory(&contract.native_home, true)?;
    Ok(())
}

fn projected_shared(input: &[u8]) -> anyhow::Result<String> {
    let raw = std::str::from_utf8(input)?;
    Ok(crate::launch::stock::remove_projection_marker(raw)?.unwrap_or_else(|| raw.to_string()))
}

/// Receipts written before the marker was moved out of native config stored a
/// projection that stamped `cutex_projection_version = 2`. Reproduce that
/// exact projection so historical evidence remains verifiable byte-for-byte.
fn projected_shared_legacy(input: &[u8]) -> anyhow::Result<String> {
    let mut value: toml::Value = toml::from_str(std::str::from_utf8(input)?)?;
    let table = value
        .as_table_mut()
        .context("shared config table required")?;
    ensure!(
        !table.contains_key(crate::launch::stock::SHARED_PROJECTION_MARKER)
            || table[crate::launch::stock::SHARED_PROJECTION_MARKER].as_integer() == Some(2),
        "unknown shared projection version"
    );
    table.insert(
        crate::launch::stock::SHARED_PROJECTION_MARKER.into(),
        toml::Value::Integer(2),
    );
    let raw = toml::to_string(&value)?;
    crate::launch::stock::validate_shared_config(&raw)?;
    Ok(raw)
}

/// A migration review remains valid when the current source still projects to
/// its stored bytes, using either the current marker-less projection or the
/// legacy marker-stamping projection frozen into older receipts.
fn shared_projection_unchanged(input: &[u8], stored: &str) -> anyhow::Result<bool> {
    Ok(projected_shared(input)? == stored || projected_shared_legacy(input)? == stored)
}

fn bytes_digest(bytes: &[u8]) -> Sha256 {
    use sha2::Digest;
    Sha256::new(format!("{:x}", sha2::Sha256::digest(bytes))).expect("sha256 hex")
}

impl AgentManagementProvider {
    pub fn start_maintenance(
        &self,
        human: &HumanManagementPrincipal,
        path: &Path,
        action: &AgentActionId,
        runtime: &mut dyn StockRuntimeExecutor,
    ) -> anyhow::Result<MaintenanceReceipt> {
        let _execution = super::provider::provider_execution_lock()
            .lock()
            .map_err(|_| anyhow::anyhow!("maintenance execution lock"))?;
        let _mutation = self.store().lock_mutations()?;
        let mut receipt = self
            .maintenance_receipt(human, path, action)?
            .context("maintenance action not applied")?;
        if receipt.phase == MaintenancePhase::Activated {
            return Ok(receipt);
        }
        ensure!(
            receipt.phase == MaintenancePhase::Applied,
            "maintenance preparation incomplete; use original apply/status"
        );
        let review = receipt
            .runtime_review
            .clone()
            .context("maintenance runtime review absent")?;
        let permit = MaintenancePermit {
            receipt: &receipt,
            recovery: None,
        };
        let result = self.execute_stock_runtime_maintenance_locked(
            path,
            &runtime_action(action)?,
            &review,
            runtime,
            &permit,
        )?;
        if result.stage == StockRuntimeStage::Ready {
            receipt.phase = MaintenancePhase::Activated;
            receipt.error = None;
        } else {
            receipt.error = result.error;
        }
        save_maintenance(path, &receipt)?;
        Ok(receipt)
    }

    /// Recover the original deterministic maintenance start after a rejected
    /// generic UI action changed the explicitly observed user-selection fields.
    /// The Human request seals both the old and complete current record digests;
    /// all identity, authority, configuration, claim and runtime fences remain.
    pub fn recover_start_maintenance(
        &self,
        human: &HumanManagementPrincipal,
        path: &Path,
        request: &MaintenanceRecoveryStartRequest,
        runtime: &mut dyn StockRuntimeExecutor,
    ) -> anyhow::Result<MaintenanceReceipt> {
        let _execution = super::provider::provider_execution_lock()
            .lock()
            .map_err(|_| anyhow::anyhow!("maintenance execution lock"))?;
        let _mutation = self.store().lock_mutations()?;
        ensure!(
            request.expires_at_unix > chrono::Utc::now().timestamp()
                && request.expires_at_unix <= chrono::Utc::now().timestamp() + 600,
            "maintenance recovery approval expired/too broad"
        );
        let mut receipt = self
            .maintenance_receipt(human, path, &request.action_id)?
            .context("maintenance action not applied")?;
        if receipt.phase == MaintenancePhase::Activated {
            return Ok(receipt);
        }
        ensure!(
            receipt.phase == MaintenancePhase::Applied,
            "maintenance preparation incomplete; use original apply/status"
        );
        let frozen = receipt
            .runtime_review
            .clone()
            .context("maintenance runtime review absent")?;
        ensure!(
            frozen.digest_version == RuntimeReviewDigestVersion::SemanticV2
                && frozen.subject.durable_sha256 == request.frozen_durable_sha256,
            "maintenance recovery frozen digest mismatch"
        );
        let runtime_action = runtime_action(&request.action_id)?;
        let sessions = load_cutex_session_store_from_path(path)?;
        let mut review = frozen.clone();
        review.subject.durable_sha256 = request.observed_durable_sha256.clone();
        match crate::session::receipt_retention::explicit_launch_receipt(&sessions, runtime_action.as_str())? {
            Some(ExplicitLaunchActionReceipt::Runtime(prior)) => ensure!(
                prior.review == review,
                "maintenance recovery conflicts with existing runtime action"
            ),
            Some(_) => anyhow::bail!("maintenance recovery runtime action domain conflict"),
            None => {
                let record = sessions
                    .sessions
                    .get(frozen.subject.cutex_session_id.as_str())
                    .context("maintenance recovery target absent")?;
                ensure!(
                    !crate::session::archive::record_has_runtime_claim(record)
                        && record.revision == frozen.subject.revision
                        && record.runtime_generation == frozen.subject.runtime_generation
                        && record.explicit_launch.as_ref() == Some(&frozen.contract)
                        && current_configuration(record)? == frozen.configuration,
                    "maintenance recovery target/configuration/claim changed"
                );
                ensure!(
                    record.last_user_selected_at == request.observed_last_user_selected_at
                        && record.last_user_action == request.observed_last_user_action
                        && record.last_user_selected_at.as_deref()
                            == Some(record.updated_at.as_str()),
                    "maintenance recovery UI observation changed/unproven"
                );
                ensure!(
                    frozen.digest_version.digest(record)? == request.observed_durable_sha256,
                    "maintenance recovery current digest mismatch"
                );
            }
        }
        let permit = MaintenancePermit {
            receipt: &receipt,
            recovery: Some(request),
        };
        let result = self.execute_stock_runtime_maintenance_locked(
            path,
            &runtime_action,
            &review,
            runtime,
            &permit,
        )?;
        if result.stage == StockRuntimeStage::Ready {
            receipt.phase = MaintenancePhase::Activated;
            receipt.error = None;
        } else {
            receipt.error = result.error;
        }
        save_maintenance(path, &receipt)?;
        Ok(receipt)
    }

    /// Produce the short-lived, exact Human review consumed by
    /// `recover_start_maintenance`. Review is read-only and refuses any target
    /// with an owner, claim, runtime journal, configuration or authority drift.
    pub fn review_start_maintenance_recovery(
        &self,
        human: &HumanManagementPrincipal,
        path: &Path,
        action: &AgentActionId,
    ) -> anyhow::Result<MaintenanceRecoveryStartRequest> {
        let _mutation = self.store().lock_mutations()?;
        let receipt = self
            .maintenance_receipt(human, path, action)?
            .context("maintenance action not applied")?;
        ensure!(
            receipt.phase == MaintenancePhase::Applied,
            "maintenance recovery requires an applied, inactive migration"
        );
        let frozen = receipt
            .runtime_review
            .as_ref()
            .context("maintenance runtime review absent")?;
        ensure!(
            frozen.digest_version == RuntimeReviewDigestVersion::SemanticV2,
            "maintenance recovery requires the affected semantic-v2 confirmation"
        );
        let sessions = load_cutex_session_store_from_path(path)?;
        crate::session::receipt_retention::ensure_action_not_pruned(&sessions, runtime_action(action)?.as_str())?;
        ensure!(
            !sessions
                .explicit_launch_receipts
                .contains_key(runtime_action(action)?.as_str()),
            "maintenance recovery runtime action already exists; use status/exact replay"
        );
        let record = sessions
            .sessions
            .get(frozen.subject.cutex_session_id.as_str())
            .context("maintenance recovery target absent")?;
        ensure!(
            !crate::session::archive::record_has_runtime_claim(record)
                && record.revision == frozen.subject.revision
                && record.runtime_generation == frozen.subject.runtime_generation
                && record.explicit_launch.as_ref() == Some(&frozen.contract)
                && current_configuration(record)? == frozen.configuration,
            "maintenance recovery target/configuration/claim changed"
        );
        ensure!(
            record.last_user_selected_at.is_some()
                && record.last_user_action.is_some()
                && record.last_user_selected_at.as_deref() == Some(record.updated_at.as_str()),
            "maintenance recovery lacks exact UI selection write evidence"
        );
        let observed = frozen.digest_version.digest(record)?;
        ensure!(
            observed != frozen.subject.durable_sha256,
            "maintenance frozen confirmation is current; use ordinary maintenance_start"
        );
        let state = self.store().snapshot()?;
        let (project, digest) =
            authority_digest(&state, &receipt.review.subject.cutex_session_id)?;
        ensure!(
            Some(project) == receipt.review.subject.current_project_id
                && digest == receipt.review.subject.authority_sha256,
            "maintenance recovery authority/task mutation"
        );
        Ok(MaintenanceRecoveryStartRequest {
            action_id: action.clone(),
            frozen_durable_sha256: frozen.subject.durable_sha256.clone(),
            observed_durable_sha256: observed,
            observed_last_user_selected_at: record.last_user_selected_at.clone(),
            observed_last_user_action: record.last_user_action,
            expires_at_unix: chrono::Utc::now().timestamp() + 600,
        })
    }
    pub fn maintenance_status(
        &self,
        human: &HumanManagementPrincipal,
        path: &Path,
        action: &AgentActionId,
    ) -> anyhow::Result<MaintenanceStatus> {
        let migration = self.maintenance_receipt(human, path, action)?;
        let store = load_cutex_session_store_from_path(path)?;
        let runtime = match crate::session::receipt_retention::explicit_launch_receipt(&store, runtime_action(action)?.as_str())? {
            None => None,
            Some(ExplicitLaunchActionReceipt::Runtime(r)) => Some(r.clone()),
            Some(_) => anyhow::bail!("maintenance runtime action domain conflict"),
        };
        let state = match migration.as_ref().map(|r| r.phase) {
            None => "not_started",
            Some(MaintenancePhase::Preparing) => "preparing",
            Some(MaintenancePhase::Prepared) => "prepared",
            Some(MaintenancePhase::Applied) => "applied",
            Some(MaintenancePhase::Activated) => "activated",
        };
        Ok(MaintenanceStatus {
            state,
            migration,
            runtime,
        })
    }
    fn maintenance_receipt(
        &self,
        _human: &HumanManagementPrincipal,
        path: &Path,
        action: &AgentActionId,
    ) -> anyhow::Result<Option<MaintenanceReceipt>> {
        let store = load_cutex_session_store_from_path(path)?;
        match crate::session::receipt_retention::explicit_launch_receipt(&store, action.as_str())? {
            None => Ok(None),
            Some(ExplicitLaunchActionReceipt::Maintenance(r)) => Ok(Some(r.clone())),
            Some(_) => anyhow::bail!("maintenance action conflicts with existing action domain"),
        }
    }

    pub fn apply_maintenance(
        &self,
        human: &HumanManagementPrincipal,
        path: &Path,
        review: &MaintenanceReview,
    ) -> anyhow::Result<MaintenanceReceipt> {
        let _execution = super::provider::provider_execution_lock()
            .lock()
            .map_err(|_| anyhow::anyhow!("maintenance execution lock"))?;
        let _mutation = self.store().lock_mutations()?;
        let mut receipt=if let Some(prior)=self.maintenance_receipt(human,path,&review.action_id)? {
            ensure!(&prior.review==review,"maintenance action changed semantics");
            if matches!(prior.phase,MaintenancePhase::Applied|MaintenancePhase::Activated){return Ok(prior)}
            prior
        } else {MaintenanceReceipt{review:review.clone(),phase:MaintenancePhase::Preparing,runtime_review:None,error:None}};
        let state=self.store().snapshot()?;
        let sessions=load_cutex_session_store_from_path(path)?;
        ensure!(!sessions.explicit_launch_receipts.values().any(|r|matches!(r,ExplicitLaunchActionReceipt::Maintenance(r) if r.review.subject.cutex_session_id==review.subject.cutex_session_id && r.review.action_id!=review.action_id)),"target already has a migration action; use original status/recovery, not duplicate materialization");
        let record=sessions.sessions.get(review.subject.cutex_session_id.as_str()).context("maintenance target absent")?;
        validate_review(review,record,&state)?;
        if !sessions.explicit_launch_receipts.contains_key(review.action_id.as_str()) {
            save_maintenance(path,&receipt)?;
        }
        // No deletion or overwrite, even when the previous creator died.
        // A complete filesystem marker permits exact replay; an incomplete
        // destination is retained and requires an explicit recovery decision.
        if let Err(error)=materialize(review) {
            receipt.error=Some(format!("materialization incomplete: {error:#}; partial destination retained, do not overwrite"));
            save_maintenance(path,&receipt)?;
            return Err(error)
        }
        receipt.phase=MaintenancePhase::Prepared; receipt.error=None;
        save_maintenance(path,&receipt)?;
        with_locked_session_store(path,|store| {
            let record=store.sessions.get(review.subject.cutex_session_id.as_str()).context("maintenance target absent")?;
            validate_review(review,record,&state)?;
            ensure!(store.sessions.values().filter(|r|r.codex_session_id==record.codex_session_id).count()==1,"native identity became ambiguous");
            let mut next=record.clone();
            next.runtime_backend=CutexSessionRuntimeBackend::Host;
            next.explicit_launch=Some(review.contract.clone());
            next.revision=next.revision.checked_add(1).context("durable revision exhausted")?;
            next.updated_at=chrono::Utc::now().to_rfc3339();
            let mut subject=review.subject.clone();
            subject.revision=next.revision;
            subject.durable_sha256=record_digest(&next)?;
            receipt.runtime_review=Some(StockRuntimeReview{digest_version:RuntimeReviewDigestVersion::SemanticV2,subject,contract:review.contract.clone(),configuration:review.configuration.clone(),restart:false,job_mcp:review.job_mcp.clone(),receiver_canonical_byte_limit:Default::default()});
            receipt.phase=MaintenancePhase::Applied;
            store.sessions.insert(next.cutex_session_id.clone(),next);
            store.explicit_launch_receipts.insert(review.action_id.to_string(),ExplicitLaunchActionReceipt::Maintenance(receipt.clone()));
            save_locked_session_store(path,store)
        })?;
        Ok(receipt)
    }

    pub fn review_maintenance(
        &self,
        _human: &HumanManagementPrincipal,
        path: &Path,
        request: &MaintenanceReviewRequest,
    ) -> anyhow::Result<MaintenanceReview> {
        let _mutation = self.store().lock_mutations()?;
        let expected=selected(&request.cutex_session_id)?;
        let state=self.store().snapshot()?;
        let (project,authority)=authority_digest(&state,&request.cutex_session_id)?;
        let sessions=load_cutex_session_store_from_path(path)?;
        crate::session::receipt_retention::ensure_action_not_pruned(&sessions,request.action_id.as_str())?;
        ensure!(!sessions.explicit_launch_receipts.contains_key(request.action_id.as_str()),"migration action already exists; use status/exact apply replay");
        ensure!(!sessions.explicit_launch_receipts.values().any(|r|matches!(r,ExplicitLaunchActionReceipt::Maintenance(r) if r.review.subject.cutex_session_id==request.cutex_session_id)),"target already has migration evidence; use original action/status");
        let record=sessions.sessions.get(request.cutex_session_id.as_str()).context("durable target absent")?;
        offline(record)?;
        ensure!(record.explicit_launch.is_none() && record.runtime_backend==CutexSessionRuntimeBackend::CuteAlden,"migration requires unmarked existing K owner");
        // Cohort rows are matched by durable session id (selected()) and native id;
        // a rename or project move since the list was frozen is not drift.
        ensure!(record.codex_session_id.as_deref()==Some(&expected.native_id),"fixed cohort native identity drift");
        ensure!(sessions.sessions.values().filter(|r|r.codex_session_id==record.codex_session_id).count()==1,"native mapping ambiguous");
        let now=chrono::Utc::now().timestamp();
        ensure!(request.expires_at_unix>now && request.expires_at_unix<=now+86400,"maintenance expiry must be within 24 hours");
        let source_home=crate::config::paths::host_codex_home_dir()?;
        files::directory(&source_home,false)?;
        let destination=&request.destination;
        ensure!(destination.is_absolute() && !destination.starts_with(&source_home) && !source_home.starts_with(destination),"destination must be source-disjoint");
        let parent=files::directory(destination.parent().context("destination parent")?,true)?;
        ensure!(destination.file_name().is_some() && !destination.try_exists()? && std::fs::symlink_metadata(destination).is_err(),"fresh exclusive destination required");
        let shared=MigrationFile::capture(&source_home.join("config.toml"),MAX_CONFIG)?;
        ensure!(request.bundle.shared_config.path==shared.path && request.bundle.shared_config.sha256==shared.sha256,"bundle must reference current source shared config");
        request.bundle.validate_components()?;
        ensure!(request.bundle.soon_ingress(),"maintenance requires exact accepted coherent bundle");
        let shared_projection=projected_shared(&shared.read_bounded(MAX_CONFIG)?)?;
        let mut bundle=request.bundle.clone();
        bundle.launch_config_sha256=Some(crate::launch::stock::launch_config_digest(&shared_projection)?);
        bundle.shared_config=VerifiedFile{path:destination.join("config.toml"),sha256:bytes_digest(shared_projection.as_bytes())};
        let contract=ExplicitLaunchContract{version:3,migration_action_id:Some(request.action_id.clone()),native_id:expected.native_id.clone(),native_home:destination.clone(),bundle_manifest:destination.join("bundle.json"),bundle_sha256:bytes_digest(&serde_json::to_vec(&bundle)?)};
        let configuration=crate::launch::stock::migration_configuration(record)?;
        let job_mcp=request.job_mcp.as_ref().map(|j|j.review(&request.bundle)).transpose()?;
        configuration.validate_job_requirement(job_mcp.is_some())?;
        let mut catalog_files=Vec::new();
        for file in ["state_5.sqlite","state_5.sqlite-wal"] {
            let catalog=source_home.join(file);
            if catalog.try_exists()? {catalog_files.push(MigrationFile::capture(&catalog,MAX_HISTORY)?);}
        }
        ensure!(!catalog_files.is_empty(),"source native catalog evidence missing");
        let catalog=catalog_metadata(&source_home,&expected.native_id)?;
        for file in &catalog_files{file.validate()?;}
        let history=history(&source_home,&expected.native_id)?;
        ensure!(catalog.rollout_path==history.path,"catalog/rollout mapping disagreement");
        Ok(MaintenanceReview{version:1,action_id:request.action_id.clone(),subject:ExplicitLaunchSubject{cutex_session_id:request.cutex_session_id.clone(),formal_name:record.formal_agent_name.clone().unwrap_or(expected.formal_name),durable_sha256:record_digest(record)?,authority_sha256:authority,current_project_id:Some(project),revision:record.revision,runtime_generation:record.runtime_generation},expires_at_unix:request.expires_at_unix,preserve_offline:expected.status!="online-observed",history,assets:assets(&source_home)?,source_home,shared,catalog_files,catalog,destination_parent_device:parent.metadata()?.dev(),destination_parent_inode:parent.metadata()?.ino(),contract,bundle,shared_projection,configuration,job_mcp})
    }
}

fn save_maintenance(path: &Path, receipt: &MaintenanceReceipt) -> anyhow::Result<()> {
    with_locked_session_store(path, |store| {
        crate::session::receipt_retention::ensure_action_not_pruned(store, receipt.review.action_id.as_str())?;
        if let Some(previous) = store
            .explicit_launch_receipts
            .get(receipt.review.action_id.as_str())
        {
            ensure!(
                matches!(previous,ExplicitLaunchActionReceipt::Maintenance(r) if r.review==receipt.review),
                "maintenance action conflict"
            );
        }
        store.explicit_launch_receipts.insert(
            receipt.review.action_id.to_string(),
            ExplicitLaunchActionReceipt::Maintenance(receipt.clone()),
        );
        save_locked_session_store(path, store)
    })
}

fn validate_review(
    review: &MaintenanceReview,
    record: &CutexSessionRecord,
    state: &AgentManagementSnapshot,
) -> anyhow::Result<()> {
    ensure!(
        review.version == 1
            && review.expires_at_unix > chrono::Utc::now().timestamp()
            && review.expires_at_unix <= chrono::Utc::now().timestamp() + 86400,
        "maintenance review expired/unknown version"
    );
    let expected = selected(&review.subject.cutex_session_id)?;
    offline(record)?;
    ensure!(
        record.explicit_launch.is_none()
            && record.runtime_backend == CutexSessionRuntimeBackend::CuteAlden
            && record.cutex_session_id == expected.durable_id
            && record.codex_session_id.as_deref() == Some(&expected.native_id),
        "maintenance source identity changed"
    );
    ensure!(
        record_digest(record)? == review.subject.durable_sha256
            && record.revision == review.subject.revision
            && record.runtime_generation == review.subject.runtime_generation,
        "maintenance source configuration/claim stale"
    );
    let (project, digest) = authority_digest(state, &review.subject.cutex_session_id)?;
    ensure!(
        Some(project) == review.subject.current_project_id
            && digest == review.subject.authority_sha256,
        "maintenance authority/task state changed"
    );
    ensure!(
        review.preserve_offline == (expected.status != "online-observed"),
        "original offline intent changed"
    );
    ensure!(
        review.source_home == crate::config::paths::host_codex_home_dir()?
            && review.shared.path == review.source_home.join("config.toml"),
        "source home/configuration changed"
    );
    ensure!(
        review.contract.version == 3
            && review.contract.migration_action_id.as_ref() == Some(&review.action_id)
            && review.contract.native_id == expected.native_id,
        "maintenance contract identity/version mismatch"
    );
    let destination = &review.contract.native_home;
    ensure!(
        !destination.starts_with(&review.source_home)
            && !review.source_home.starts_with(destination),
        "source/destination overlap"
    );
    let parent = files::directory(
        destination.parent().context("destination parent missing")?,
        true,
    )?;
    ensure!(
        (parent.metadata()?.dev(), parent.metadata()?.ino())
            == (
                review.destination_parent_device,
                review.destination_parent_inode
            ),
        "destination parent changed"
    );
    ensure!(
        review.contract.bundle_manifest == destination.join("bundle.json")
            && review.bundle.shared_config.path == destination.join("config.toml"),
        "destination bundle path mismatch"
    );
    ensure!(
        bytes_digest(&serde_json::to_vec(&review.bundle)?) == review.contract.bundle_sha256
            && bytes_digest(review.shared_projection.as_bytes())
                == review.bundle.shared_config.sha256,
        "migration bundle/projection tampered"
    );
    ensure!(
        shared_projection_unchanged(
            &review.shared.read_bounded(MAX_CONFIG)?,
            &review.shared_projection
        )?,
        "shared configuration changed"
    );
    let mut source_bundle = review.bundle.clone();
    source_bundle.shared_config = VerifiedFile {
        path: review.shared.path.clone(),
        sha256: review.shared.sha256.clone(),
    };
    source_bundle.validate_components()?;
    ensure!(source_bundle.soon_ingress(), "unsupported migration bundle");
    ensure!(
        crate::launch::stock::migration_configuration(record)? == review.configuration,
        "effective projection/auth custody changed"
    );
    review
        .configuration
        .validate_job_requirement(review.job_mcp.is_some())?;
    if let Some(job) = &review.job_mcp {
        job.validate(&source_bundle)?;
    }
    ensure!(
        history(&review.source_home, &expected.native_id)? == review.history,
        "source history changed/missing/ambiguous"
    );
    let mut catalog = Vec::new();
    for name in ["state_5.sqlite", "state_5.sqlite-wal"] {
        let path = review.source_home.join(name);
        if path.try_exists()? {
            catalog.push(MigrationFile::capture(&path, MAX_HISTORY)?);
        }
    }
    ensure!(
        !catalog.is_empty() && catalog == review.catalog_files,
        "source catalog changed"
    );
    ensure!(
        catalog_metadata(&review.source_home, &expected.native_id)? == review.catalog
            && review.catalog.rollout_path == review.history.path,
        "source catalog semantics changed"
    );
    for file in &catalog {
        file.validate()?;
    }
    ensure!(
        assets(&review.source_home)? == review.assets,
        "skills/memories source changed"
    );
    Ok(())
}

fn materialize(review: &MaintenanceReview) -> anyhow::Result<()> {
    let destination = &review.contract.native_home;
    let parent = files::directory(destination.parent().context("destination parent")?, true)?;
    ensure!(
        (parent.metadata()?.dev(), parent.metadata()?.ino())
            == (
                review.destination_parent_device,
                review.destination_parent_inode
            ),
        "destination parent changed"
    );
    let complete = serde_json::to_vec(
        &serde_json::json!({"version":1,"review_sha256":super::store::request_sha256(review)?}),
    )?;
    if destination.try_exists()? {
        files::directory(destination, true)?;
        let marker =
            MigrationFile::capture(&destination.join("migration-complete.json"), MAX_CONFIG)?;
        ensure!(
            marker.read_bounded(MAX_CONFIG)? == complete,
            "partial/conflicting destination; original retained, automatic overwrite forbidden"
        );
    } else {
        let root = files::mkdir(
            &parent,
            destination
                .file_name()
                .context("destination filename")?
                .to_str()
                .context("destination UTF8")?,
        )?;
        let sessions = files::mkdir(&root, "sessions")?;
        review.history.copy_to(
            &sessions,
            review
                .history
                .path
                .file_name()
                .context("history name")?
                .to_str()
                .context("history UTF8 name")?,
        )?;
        for asset in &review.assets {
            let relative = asset.path.strip_prefix(&review.source_home)?;
            let mut directory = root.try_clone()?;
            let mut current = destination.clone();
            for component in relative.parent().context("asset parent")?.components() {
                let std::path::Component::Normal(name) = component else {
                    anyhow::bail!("asset relative path invalid")
                };
                current.push(name);
                directory = if current.try_exists()? {
                    files::directory(&current, true)?
                } else {
                    files::mkdir(&directory, name.to_str().context("asset path UTF8")?)?
                };
            }
            asset.copy_to(
                &directory,
                relative
                    .file_name()
                    .context("asset filename")?
                    .to_str()
                    .context("asset filename UTF8")?,
            )?;
        }
        files::write_new(&root, "config.toml", review.shared_projection.as_bytes())?;
        files::write_new(&root, "bundle.json", &serde_json::to_vec(&review.bundle)?)?;
        files::write_new(&root, "migration-complete.json", &complete)?;
        let current = files::directory(destination, true)?;
        ensure!(
            (root.metadata()?.dev(), root.metadata()?.ino())
                == (current.metadata()?.dev(), current.metadata()?.ino()),
            "destination replaced during materialization"
        );
    }
    let copied = MigrationFile::capture(
        &destination.join("sessions").join(
            review
                .history
                .path
                .file_name()
                .context("history filename")?,
        ),
        MAX_HISTORY,
    )?;
    ensure!(
        copied.sha256 == review.history.sha256
            && copied.length == review.history.length
            && (copied.device, copied.inode) != (review.history.device, review.history.inode),
        "history not an independent exact copy"
    );
    for source in &review.assets {
        let target = MigrationFile::capture(
            &destination.join(source.path.strip_prefix(&review.source_home)?),
            32 * 1024 * 1024,
        )?;
        ensure!(
            target.sha256 == source.sha256
                && target.length == source.length
                && target.executable == source.executable
                && (target.device, target.inode) != (source.device, source.inode),
            "copied skill/memory asset changed"
        );
    }
    StockBundle::load(&review.contract)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::DirBuilderExt;
    fn home_fixture() -> (
        PathBuf,
        CutexSessionRecord,
        crate::session::model::CutexSessionStore,
    ) {
        use serde_json::json;
        let parent = std::env::temp_dir().join(format!("migration-home-{}", uuid::Uuid::new_v4()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&parent)
            .unwrap();
        let home = parent.join("native-home");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&home)
            .unwrap();
        let native = uuid::Uuid::new_v4().to_string();
        let action = AgentActionId::new("original-migration").unwrap();
        let mut record = CutexSessionRecord::new(
            "cutex.migrated".into(),
            Some(native.clone()),
            "local".into(),
            parent.to_str().unwrap().into(),
            None,
        )
        .unwrap();
        let contract = ExplicitLaunchContract {
            version: 3,
            migration_action_id: Some(action.clone()),
            native_id: native.clone(),
            native_home: home.clone(),
            bundle_manifest: home.join("old-bundle.json"),
            bundle_sha256: bytes_digest(b"old bundle"),
        };
        record.explicit_launch = Some(contract.clone());
        let file = json!({"path":home.join("old-file"),"sha256":"a".repeat(64)});
        let migration_file = json!({"path":home.join("old-file"),"device":1,"inode":2,"length":3,
            "executable":false,"modified_seconds":0,"modified_nanos":0,"changed_seconds":0,"changed_nanos":0,"sha256":"a".repeat(64)});
        let receipt: MaintenanceReceipt = serde_json::from_value(json!({
            "phase":"activated","runtime_review":null,"error":null,
            "review":{
                "version":1,"action_id":action,
                "subject":{"cutex_session_id":record.cutex_session_id,"formal_name":"migrated",
                    "durable_sha256":"a".repeat(64),"authority_sha256":"b".repeat(64),"current_project_id":null,"revision":0,"runtime_generation":0},
                "expires_at_unix":1,"preserve_offline":true,"source_home":"/old/source",
                "history":migration_file,"shared":migration_file,"catalog_files":[],"assets":[],
                "catalog":{"native_id":native,"rollout_path":"/old/rollout","memory_mode":"enabled","history_mode":"legacy"},
                "destination_parent_device":0,"destination_parent_inode":0,"contract":contract,
                "bundle":{"version":3,"upstream_commit":"old-upstream","native_patch_commit":"old-build",
                    "executable":file,"cli":file,"code_mode_host":file,"facade":file,"schema":file,"shared_config":file},
                "shared_projection":"old configuration",
                "configuration":{"profile_name":"alpha","profile_id":"profile","inherited":false,
                    "profile_sha256":"c".repeat(64),"account_sha256":"d".repeat(64),"model":"model","reasoning":null,
                    "model_provider":"private","provider":{"name":"private","base_url":"http://127.0.0.1:1/v1",
                    "wire_api":"responses","requires_openai_auth":false,"supports_websockets":false},
                    "sandbox":"read-only","approval":"on-request"},"job_mcp":null
            }
        })).unwrap();
        let mut sessions = crate::session::model::CutexSessionStore::default();
        sessions
            .sessions
            .insert(record.cutex_session_id.clone(), record.clone());
        sessions.explicit_launch_receipts.insert(
            action.as_str().into(),
            ExplicitLaunchActionReceipt::Maintenance(receipt),
        );
        (parent, record, sessions)
    }

    #[test]
    fn offline_migration_archive_evidence_survives_package_upgrade_but_not_runtime_change() {
        let (parent, mut record, mut sessions) = home_fixture();
        record.runtime_generation = 9;
        let ExplicitLaunchActionReceipt::Maintenance(receipt) = sessions
            .explicit_launch_receipts
            .get_mut("original-migration")
            .unwrap()
        else {
            panic!("maintenance fixture")
        };
        receipt.phase = MaintenancePhase::Applied;
        receipt.review.subject.runtime_generation = 9;
        assert!(has_unstarted_offline_migration(&record, &sessions));
        record.explicit_launch.as_mut().unwrap().bundle_manifest = parent.join("upgraded.json");
        record.explicit_launch.as_mut().unwrap().bundle_sha256 = bytes_digest(b"new bundle");
        assert!(has_unstarted_offline_migration(&record, &sessions));
        record.runtime_generation += 1;
        assert!(!has_unstarted_offline_migration(&record, &sessions));
        record.runtime_generation -= 1;
        record.app_server_launch_claim_id = Some("launch-in-progress".into());
        assert!(!has_unstarted_offline_migration(&record, &sessions));
        record.app_server_launch_claim_id = None;
        record.explicit_launch.as_mut().unwrap().native_home = parent.join("other-home");
        assert!(!has_unstarted_offline_migration(&record, &sessions));
        record.explicit_launch.as_mut().unwrap().native_home = parent.join("native-home");
        let original = sessions.clone();
        for phase in [
            MaintenancePhase::Preparing,
            MaintenancePhase::Prepared,
            MaintenancePhase::Activated,
        ] {
            let ExplicitLaunchActionReceipt::Maintenance(receipt) = sessions
                .explicit_launch_receipts
                .get_mut("original-migration")
                .unwrap()
            else {
                unreachable!()
            };
            receipt.phase = phase;
            assert!(!has_unstarted_offline_migration(&record, &sessions));
        }
        sessions = original;
        let ExplicitLaunchActionReceipt::Maintenance(receipt) = sessions
            .explicit_launch_receipts
            .get_mut("original-migration")
            .unwrap()
        else {
            unreachable!()
        };
        receipt.review.preserve_offline = false;
        assert!(!has_unstarted_offline_migration(&record, &sessions));
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn migrated_home_accepts_new_package_and_restored_directory_identity() {
        let (parent, mut record, sessions) = home_fixture();
        // Historical inode values are intentionally stale, and package evidence
        // changes without moving native identity or copying its history again.
        let mut contract = record.explicit_launch.clone().unwrap();
        contract.bundle_manifest = parent.join("new-package.json");
        contract.bundle_sha256 = bytes_digest(b"new package");
        record.explicit_launch = Some(contract.clone());
        assert!(validate_migration_home(&record, &sessions, &contract).is_ok());
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn migrated_home_still_requires_committed_owner_and_private_current_home() {
        use std::os::unix::fs::PermissionsExt;
        let (parent, record, sessions) = home_fixture();
        let original = record.explicit_launch.clone().unwrap();
        for field in ["native_id", "native_home", "owner"] {
            let mut record = record.clone();
            let mut contract = original.clone();
            match field {
                "native_id" => contract.native_id = uuid::Uuid::new_v4().to_string(),
                "native_home" => contract.native_home = parent.join("another-home"),
                "owner" => record.cutex_session_id = "cutex.another-agent".into(),
                _ => unreachable!(),
            }
            record.explicit_launch = Some(contract.clone());
            assert!(validate_migration_home(&record, &sessions, &contract).is_err());
        }
        let mut uncommitted: crate::session::model::CutexSessionStore =
            serde_json::from_value(serde_json::to_value(&sessions).unwrap()).unwrap();
        if let Some(ExplicitLaunchActionReceipt::Maintenance(receipt)) =
            uncommitted.explicit_launch_receipts.values_mut().next()
        {
            receipt.phase = MaintenancePhase::Prepared;
        }
        assert!(validate_migration_home(&record, &uncommitted, &original).is_err());
        std::fs::set_permissions(
            &original.native_home,
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(validate_migration_home(&record, &sessions, &original).is_err());
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn maintenance_wal_committed_view_and_corruption_are_independent() {
        let root = std::env::temp_dir().join(format!("maintenance-wal-{}", uuid::Uuid::new_v4()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        let code = r#"
import sqlite3,sys,shutil,pathlib,os
os.umask(0o077)
r=pathlib.Path(sys.argv[1]); w=r/'writer';w.mkdir(mode=0o700)
c=sqlite3.connect(w/'state_5.sqlite')
c.execute('pragma journal_mode=wal');c.execute('pragma wal_autocheckpoint=0')
c.execute('create table threads(id text,rollout_path text,memory_mode text,history_mode text)')
c.execute("insert into threads values('native','/private/history','enabled','legacy')");c.commit()
c.execute('pragma wal_checkpoint(TRUNCATE)')
c.execute("update threads set history_mode='paginated'");c.commit()
for f in ('state_5.sqlite','state_5.sqlite-wal'):shutil.copyfile(w/f,r/f)
c.close()
# Independent baseline proves the new field exists in WAL, not base DB.
d=sqlite3.connect('file:'+str(r/'state_5.sqlite')+'?immutable=1',uri=True)
assert d.execute('select history_mode from threads').fetchone()==('legacy',)
d.close()
"#;
        assert!(
            std::process::Command::new("/usr/bin/python3")
                .args(["-I", "-S", "-c", code])
                .arg(&root)
                .env_clear()
                .status()
                .unwrap()
                .success()
        );
        let db = MigrationFile::capture(&root.join("state_5.sqlite"), MAX_CONFIG).unwrap();
        let wal = MigrationFile::capture(&root.join("state_5.sqlite-wal"), MAX_CONFIG).unwrap();
        assert_eq!(
            catalog_metadata(&root, "native").unwrap().history_mode,
            "paginated"
        );
        assert!(
            catalog_metadata_in(&root, "native", &root).is_err(),
            "source-overlapping scratch accepted"
        );
        assert_eq!(MigrationFile::capture(&db.path, MAX_CONFIG).unwrap(), db);
        assert_eq!(MigrationFile::capture(&wal.path, MAX_CONFIG).unwrap(), wal);
        assert!(!root.join("state_5.sqlite-shm").exists());
        let mut bytes = std::fs::read(&wal.path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(&wal.path, &bytes).unwrap();
        assert!(
            catalog_metadata(&root, "native").is_err(),
            "corrupt committed WAL silently ignored"
        );
        assert_eq!(std::fs::read(&wal.path).unwrap(), bytes);
        assert!(!root.join("state_5.sqlite-shm").exists());
    }
    #[test]
    fn maintenance_selected34_is_finite_and_offline_is_not_reselected() {
        let selection: Selection =
            serde_json::from_str(include_str!("../../docs/project-recent14-candidates.json"))
                .unwrap();
        let mut included = 0;
        let mut offline = 0;
        for row in selection.rows {
            let id = CutexSessionId::new(row.durable_id.clone()).unwrap();
            let result = selected(&id);
            if row.classification == "selected" {
                let target = result.unwrap();
                included += 1;
                offline += usize::from(target.status != "online-observed");
                assert_eq!(target.native_id, row.native_id);
                assert_eq!(target.project_id, row.project_id);
            } else {
                assert!(result.is_err());
            }
        }
        assert_eq!((included, offline), (34, 16));
    }
    #[test]
    fn maintenance_semantic_record_ignores_only_observation_times() {
        let base = CutexSessionRecord::new(
            "cutex.test".into(),
            Some(uuid::Uuid::new_v4().to_string()),
            "private".into(),
            "/private".into(),
            None,
        )
        .unwrap();
        let digest = record_digest(&base).unwrap();
        let mut other = base.clone();
        other.last_seen_at = Some("2030-01-01T00:00:00Z".into());
        other.updated_at = "2030-01-01T00:00:00Z".into();
        assert_eq!(record_digest(&other).unwrap(), digest);
        for field in ["revision", "runtime_generation", "runtime_pid"] {
            let mut value = serde_json::to_value(&base).unwrap();
            value[field] = serde_json::json!(42);
            let record: CutexSessionRecord = serde_json::from_value(value).unwrap();
            assert_ne!(record_digest(&record).unwrap(), digest);
        }
        other = base.clone();
        other.agent_groups.push("changed".into());
        assert_ne!(record_digest(&other).unwrap(), digest);
    }
    #[test]
    fn maintenance_unknown_fields_and_versions_do_not_gain_authority() {
        assert!(
            serde_json::from_value::<ExplicitLaunchRequest>(
                serde_json::json!({"operation":"maintenance_start","action_id":"test","human":true})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<ExplicitLaunchActionReceipt>(
                serde_json::json!({"kind":"maintenance_v2","receipt":{}})
            )
            .is_err()
        );
        let review: ExplicitLaunchRequest = serde_json::from_value(serde_json::json!({
            "operation":"maintenance_recovery_review",
            "action_id":"migration-action"
        }))
        .unwrap();
        assert!(matches!(
            review,
            ExplicitLaunchRequest::MaintenanceRecoveryReview { .. }
        ));
        let recovery: ExplicitLaunchRequest = serde_json::from_value(serde_json::json!({
            "operation":"maintenance_recovery_start",
            "request":{
                "action_id":"migration-action",
                "frozen_durable_sha256":"1111111111111111111111111111111111111111111111111111111111111111",
                "observed_durable_sha256":"2222222222222222222222222222222222222222222222222222222222222222",
                "observed_last_user_selected_at":"2026-09-13T08:22:39Z",
                "observed_last_user_action":"online",
                "expires_at_unix":1789326408
            }
        }))
        .unwrap();
        assert!(matches!(
            recovery,
            ExplicitLaunchRequest::MaintenanceRecoveryStart { .. }
        ));
        assert!(serde_json::from_value::<ExplicitLaunchRequest>(serde_json::json!({
            "operation":"maintenance_recovery_start",
            "request":{
                "action_id":"migration-action",
                "frozen_durable_sha256":"1111111111111111111111111111111111111111111111111111111111111111",
                "observed_durable_sha256":"2222222222222222222222222222222222222222222222222222222222222222",
                "observed_last_user_selected_at":"2026-09-13T08:22:39Z",
                "observed_last_user_action":"online",
                "expires_at_unix":1789326408,
                "skip_fences":true
            }
        })).is_err());
        let raw = "[mcp_servers.injected]\ncommand='bad'\n";
        let projected: toml::Value =
            toml::from_str(&projected_shared(raw.as_bytes()).unwrap()).unwrap();
        assert_eq!(
            projected["mcp_servers"]["injected"]["command"].as_str(),
            Some("bad")
        );
        assert!(projected_shared(b"cutex_projection_version=999\n").is_err());
        // New reviews freeze marker-less projections; legacy receipts stamped
        // the marker into the stored bytes. Both remain verifiable against the
        // same unchanged source, while a changed source fails both forms.
        let projected = projected_shared(raw.as_bytes()).unwrap();
        let legacy = projected_shared_legacy(raw.as_bytes()).unwrap();
        let legacy_value: toml::Value = toml::from_str(&legacy).unwrap();
        assert_eq!(
            legacy_value[crate::launch::stock::SHARED_PROJECTION_MARKER].as_integer(),
            Some(2)
        );
        assert!(!projected.contains(crate::launch::stock::SHARED_PROJECTION_MARKER));
        assert!(shared_projection_unchanged(raw.as_bytes(), &projected).unwrap());
        assert!(shared_projection_unchanged(raw.as_bytes(), &legacy).unwrap());
        assert!(!shared_projection_unchanged(b"model='changed'\n", &legacy).unwrap());
        assert!(projected_shared_legacy(b"cutex_projection_version=999\n").is_err());
        let stripped: toml::Value = toml::from_str(
            &projected_shared(b"cutex_projection_version=2\nmodel='gpt-5'\n").unwrap(),
        )
        .unwrap();
        assert_eq!(stripped["model"].as_str(), Some("gpt-5"));
    }
    #[test]
    fn maintenance_catalog_is_real_read_only_and_pending_wal_refuses() {
        let root = std::env::temp_dir().join(format!("migration-catalog-{}", uuid::Uuid::new_v4()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        let db = root.join("state_5.sqlite");
        let code = "import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); c.execute('create table threads(id text, rollout_path text, memory_mode text, history_mode text)'); c.execute('insert into threads values(?,?,?,?)',('native','/private/history','enabled','paginated')); c.commit(); c.close()";
        assert!(
            std::process::Command::new("/usr/bin/python3")
                .args(["-I", "-S", "-c", code])
                .arg(&db)
                .env_clear()
                .status()
                .unwrap()
                .success()
        );
        let before = MigrationFile::capture(&db, MAX_CONFIG).unwrap();
        assert_eq!(
            catalog_metadata(&root, "native").unwrap().history_mode,
            "paginated"
        );
        assert_eq!(MigrationFile::capture(&db, MAX_CONFIG).unwrap(), before);
        assert!(!root.join("state_5.sqlite-shm").exists());
        assert!(catalog_metadata(&root, "wrong").is_err());
        std::fs::write(root.join("state_5.sqlite-wal"), b"uncheckpointed").unwrap();
        assert!(catalog_metadata(&root, "native").is_err());
        assert_eq!(MigrationFile::capture(&db, MAX_CONFIG).unwrap(), before);
    }
}
