//! Reviewed package upgrades for existing local explicit-launch Agents.
//!
//! An explicit launch contract intentionally freezes a reviewed native package.
//! Replacing that package is therefore an explicit Human action: preserve the
//! native identity and home, derive a fresh side-car manifest from the selected
//! immutable deployment, and commit the new contract with a durable receipt.
use super::*;
use crate::launch::local_deployment::LocalDeployment;
use crate::launch::stock::StockBundle;
use crate::management::control_plane::HumanManagementPrincipal;
use crate::role_revision::{CutexSessionId, Sha256};
use crate::session::model::CutexSessionRecord;
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimePackageUpgradeReview {
    pub version: u32,
    pub subject: ExplicitLaunchSubject,
    pub current_contract: ExplicitLaunchContract,
    pub target_contract: ExplicitLaunchContract,
    pub selected_manifest: PathBuf,
    pub selected_manifest_sha256: Sha256,
    pub current_runtime_version: Option<String>,
    pub target_runtime_version: Option<String>,
    pub latest_runtime_version: Option<String>,
    /// Set when the current package can no longer be loaded because files it
    /// pins were deleted (an old release directory removed): the upgrade is a
    /// repair onto the selected package. There is nothing to go back to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair: Option<RuntimePackageRepair>,
}

/// Why a reviewed upgrade is a repair: the current package's manifest is
/// intact (digest matches the contract) but these pinned files are gone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimePackageRepair {
    pub missing_files: Vec<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimePackageUpgradeReceipt {
    pub action_id: AgentActionId,
    pub review: RuntimePackageUpgradeReview,
    pub activated_revision: u64,
    pub committed_at: String,
}

/// Result of putting an Agent back on the contract it had before a runtime
/// package upgrade (Management v2 `/v2/runtime-packages/runs/{id}/revert`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimePackageRevertReceipt {
    pub cutex_session_id: CutexSessionId,
    pub replaced_contract: ExplicitLaunchContract,
    pub restored_contract: ExplicitLaunchContract,
    pub activated_revision: u64,
    pub committed_at: String,
}

impl AgentManagementProvider {
    /// Undo one reviewed package upgrade in-daemon: `explicit_launch` must
    /// still be exactly `expected` (the contract the upgrade wrote) and becomes
    /// `restore` (the contract it replaced) under the same locks as apply
    /// (execution lock, provider mutation lock, archive read fence and the
    /// cross-process session-store lock), so neither Management service has
    /// to be stopped. The restored package must still load and validate.
    #[allow(clippy::too_many_arguments)]
    pub fn revert_runtime_package_upgrade(
        &self,
        _principal: &HumanManagementPrincipal,
        path: &Path,
        id: &CutexSessionId,
        expected: &ExplicitLaunchContract,
        restore: &ExplicitLaunchContract,
    ) -> anyhow::Result<RuntimePackageRevertReceipt> {
        let _execution = provider_execution_lock()
            .lock()
            .map_err(|_| anyhow::anyhow!("execution lock unavailable"))?;
        let _mutation = self.store().lock_mutations()?;
        revert_locked(path, id, expected, restore, |candidate, sessions| {
            let state = self_runtime_guard_state(self)?;
            package_upgrade_subject(candidate, &state)?;
            crate::launch::stock::StockBundle::load(restore)
                .context("previous runtime package is unavailable")?;
            crate::launch::stock::validate_native(candidate, sessions, restore)
                .context("previous runtime home is invalid")?;
            Ok(())
        })
    }

    pub fn review_runtime_package_upgrade(
        &self,
        path: &Path,
        id: &CutexSessionId,
    ) -> anyhow::Result<RuntimePackageUpgradeReview> {
        let _mutation = self.store().lock_mutations()?;
        let state = self.store().snapshot()?;
        let sessions = crate::session::store::load_cutex_session_store_from_path(path)?;
        let record = sessions
            .sessions
            .get(id.as_str())
            .ok_or_else(|| anyhow::anyhow!("stock durable record missing"))?;
        let subject = package_upgrade_subject(record, &state)?;
        build_review(subject, record, &sessions)
    }

    pub fn apply_runtime_package_upgrade(
        &self,
        _principal: &HumanManagementPrincipal,
        path: &Path,
        action_id: &AgentActionId,
        review: &RuntimePackageUpgradeReview,
    ) -> anyhow::Result<RuntimePackageUpgradeReceipt> {
        let _execution = provider_execution_lock()
            .lock()
            .map_err(|_| anyhow::anyhow!("execution lock unavailable"))?;
        let _mutation = self.store().lock_mutations()?;
        crate::session::store::with_locked_session_store(path, |sessions| {
            let record = sessions
                .sessions
                .get(review.subject.cutex_session_id.as_str())
                .ok_or_else(|| anyhow::anyhow!("stock durable record missing"))?;
            let receipt_path =
                package_receipt_path(&review.target_contract.native_home, action_id)?;
            if receipt_path.try_exists()? {
                let saved: RuntimePackageUpgradeReceipt =
                    serde_json::from_slice(&std::fs::read(&receipt_path)?)
                        .context("runtime package receipt missing or invalid")?;
                ensure!(
                    saved.action_id == *action_id && &saved.review == review,
                    "runtime_package_action_payload_conflict"
                );
                if record.explicit_launch.as_ref() == Some(&review.target_contract)
                    && record.revision == saved.activated_revision
                {
                    return Ok(saved);
                }
                ensure!(
                    record.explicit_launch.as_ref() == Some(&review.current_contract)
                        && record.durable_revision().checked_add(1)
                            == Some(saved.activated_revision),
                    "interrupted runtime package action state changed; review recovery"
                );
            } else {
                let state = self_runtime_guard_state(self)?;
                let subject = package_upgrade_subject(record, &state)?;
                let recomputed = build_review(subject, record, sessions)?;
                ensure!(
                    review.version == 1 && &recomputed == review,
                    "runtime package review is stale; review again"
                );
            }

            ensure!(
                record.app_server_launch_claim_id.is_none()
                    && !crate::session::archive::record_has_runtime_claim(record)
                    && record.pending_launch_id.is_none(),
                "runtime package upgrade requires offline/no claim"
            );
            if review.current_contract.version == 3 {
                LocalDeployment::materialize_v3_upgrade(
                    record,
                    sessions,
                    &review.current_contract,
                    &review.target_contract,
                )?;
            }
            install_target_manifest(
                &review.target_contract,
                &review.selected_manifest,
                &review.selected_manifest_sha256,
            )?;
            super::stock_runtime::sanitize_shared_projection_marker(&review.target_contract)?;
            crate::launch::stock::StockBundle::load(&review.target_contract)?;
            let mut candidate = record.clone();
            candidate.explicit_launch = Some(review.target_contract.clone());
            crate::launch::stock::validate_native(
                &candidate,
                sessions,
                &review.target_contract,
            )?;

            let record = sessions
                .sessions
                .get_mut(review.subject.cutex_session_id.as_str())
                .ok_or_else(|| anyhow::anyhow!("stock durable record missing"))?;
            let activated_revision = record
                .durable_revision()
                .checked_add(1)
                .context("cutex session durable revision exhausted")?;
            let receipt = RuntimePackageUpgradeReceipt {
                action_id: action_id.clone(),
                review: review.clone(),
                activated_revision,
                committed_at: chrono::Utc::now().to_rfc3339(),
            };
            write_package_receipt(&receipt_path, &receipt)?;
            let bumped = record.bump_durable_revision()?;
            ensure!(
                bumped == activated_revision,
                "durable revision changed during upgrade"
            );
            record.updated_at = receipt.committed_at.clone();
            record.explicit_launch = Some(review.target_contract.clone());
            crate::session::store::save_locked_session_store(path, sessions)?;
            Ok(receipt)
        })
    }
}

/// The store half of [`AgentManagementProvider::revert_runtime_package_upgrade`]:
/// guards, `validate_restore` on the candidate record, then the commit.
fn revert_locked(
    path: &Path,
    id: &CutexSessionId,
    expected: &ExplicitLaunchContract,
    restore: &ExplicitLaunchContract,
    validate_restore: impl FnOnce(
        &CutexSessionRecord,
        &crate::session::model::CutexSessionStore,
    ) -> anyhow::Result<()>,
) -> anyhow::Result<RuntimePackageRevertReceipt> {
    ensure!(expected != restore, "revert target equals the current contract");
    ensure!(
        expected.native_id == restore.native_id,
        "runtime package revert must keep the native identity"
    );
    ensure!(
        matches!(restore.version, 3 | 4),
        "only local explicit-launch packages can be reverted"
    );
    crate::session::store::with_locked_session_store(path, |sessions| {
        let record = sessions
            .sessions
            .get(id.as_str())
            .ok_or_else(|| anyhow::anyhow!("stock durable record missing"))?;
        ensure!(
            record.explicit_launch.as_ref() == Some(expected),
            "runtime_package_contract_changed: the agent's launch contract is no longer the one the upgrade wrote"
        );
        ensure!(
            crate::runtime::lifecycle::cutex_session_host_is_local(
                &record.host_id,
                &crate::platform::host::current_host_name(),
            ),
            "runtime package must be reverted on its owning host"
        );
        ensure!(
            record.runtime_backend == crate::session::model::CutexSessionRuntimeBackend::Host,
            "runtime package revert requires the explicit host backend"
        );
        ensure!(
            !record.is_retired()
                && record.codex_session_id.as_deref() == Some(restore.native_id.as_str()),
            "active exact adopted identity required"
        );
        ensure!(
            record.app_server_launch_claim_id.is_none()
                && !crate::session::archive::record_has_runtime_claim(record)
                && record.pending_launch_id.is_none(),
            "runtime package revert requires offline/no claim"
        );
        let mut candidate = record.clone();
        candidate.explicit_launch = Some(restore.clone());
        validate_restore(&candidate, sessions)?;

        let record = sessions
            .sessions
            .get_mut(id.as_str())
            .ok_or_else(|| anyhow::anyhow!("stock durable record missing"))?;
        let activated_revision = record.bump_durable_revision()?;
        let committed_at = chrono::Utc::now().to_rfc3339();
        record.updated_at = committed_at.clone();
        record.explicit_launch = Some(restore.clone());
        crate::session::store::save_locked_session_store(path, sessions)?;
        Ok(RuntimePackageRevertReceipt {
            cutex_session_id: id.clone(),
            replaced_contract: expected.clone(),
            restored_contract: restore.clone(),
            activated_revision,
            committed_at,
        })
    })
}

fn self_runtime_guard_state(
    provider: &AgentManagementProvider,
) -> anyhow::Result<crate::agent_management::AgentManagementSnapshot> {
    provider
        .store()
        .snapshot()
        .map_err(|error| anyhow::anyhow!("agent management state unavailable: {error}"))
}

fn current_bundle(record: &CutexSessionRecord) -> anyhow::Result<StockBundle> {
    let contract = explicit_contract(record)?;
    serde_json::from_slice(&std::fs::read(&contract.bundle_manifest)?)
        .context("current runtime package manifest missing or invalid")
}

fn selected_bundle() -> anyhow::Result<(PathBuf, Sha256, StockBundle)> {
    let deployment =
        LocalDeployment::selected()?.context("selected runtime deployment required")?;
    let manifest = deployment.bundle_manifest.canonicalize()?;
    let manifest_bundle: StockBundle = serde_json::from_slice(&std::fs::read(&manifest)?)?;
    let selected_native_home = manifest_bundle
        .shared_config
        .path
        .parent()
        .context("selected runtime shared-config home missing")?;
    let digest = file_sha256(&manifest)?;
    let bundle = StockBundle::load_references(selected_native_home, &manifest, &digest)?;
    ensure!(
        bundle.soon_ingress(),
        "selected runtime deployment requires supported light protocol"
    );
    Ok((manifest, digest, bundle))
}

fn explicit_contract(record: &CutexSessionRecord) -> anyhow::Result<&ExplicitLaunchContract> {
    record
        .explicit_launch
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("explicit launch contract missing"))
}

fn package_upgrade_subject(
    record: &CutexSessionRecord,
    state: &crate::agent_management::AgentManagementSnapshot,
) -> anyhow::Result<ExplicitLaunchSubject> {
    let id = CutexSessionId::new(record.cutex_session_id.clone())
        .map_err(|error| anyhow::anyhow!("invalid durable Agent ID: {error:?}"))?;
    let project = super::archive::runtime_guard(state, &id)
        .map_err(|error| anyhow::anyhow!("runtime package/project guard: {error}"))?;
    Ok(ExplicitLaunchSubject {
        cutex_session_id: id,
        formal_name: record
            .formal_agent_name
            .clone()
            .context("explicit formal Agent name required")?,
        // No digest of the mutable record: the upgrade re-reads it under lock.
        durable_sha256: zero_sha256()?,
        authority_sha256: zero_sha256()?,
        current_project_id: project,
        revision: record.revision,
        runtime_generation: record.runtime_generation,
    })
}

fn build_review(
    subject: ExplicitLaunchSubject,
    record: &CutexSessionRecord,
    sessions: &crate::session::model::CutexSessionStore,
) -> anyhow::Result<RuntimePackageUpgradeReview> {
    let current_contract = explicit_contract(record)?.clone();
    let current_bundle = current_bundle(record)?;
    let selected = selected_bundle()?;
    let repair = validate_upgrade_source(record, sessions, &current_contract)?;
    ensure!(
        package_changed(&current_bundle, &selected.2),
        "agent already uses the selected runtime package"
    );
    let target_contract = target_contract(&current_contract, &selected.2)?;
    let registry = crate::launch::version_registry::load_relaxed();
    Ok(RuntimePackageUpgradeReview {
        version: 1,
        subject,
        current_contract,
        target_contract,
        selected_manifest: selected.0,
        selected_manifest_sha256: selected.1,
        current_runtime_version: registry.as_ref().and_then(|registry| {
            registry
                .runtime_version(current_bundle.executable.sha256.as_str())
                .map(str::to_string)
        }),
        target_runtime_version: registry.as_ref().and_then(|registry| {
            registry
                .runtime_version(selected.2.executable.sha256.as_str())
                .map(str::to_string)
        }),
        latest_runtime_version: registry.map(|registry| registry.latest_runtime.version),
        repair,
    })
}

/// Pinned files of `contract`'s package that no longer exist, when that is
/// the only thing wrong with its evidence: a v4 contract whose native home
/// exists and whose manifest still matches the contract digest. Any other
/// defect (manifest missing or changed, older contract) is an error: only a
/// deleted release directory is repairable.
pub fn missing_package_files(contract: &ExplicitLaunchContract) -> anyhow::Result<Vec<PathBuf>> {
    ensure!(contract.version == 4, "only v4 packages can be repaired");
    contract.validate()?;
    ensure!(contract.native_home.is_dir(), "native home missing");
    ensure!(
        file_sha256(&contract.bundle_manifest)? == contract.bundle_sha256,
        "bundle evidence missing or changed"
    );
    let bundle: StockBundle = serde_json::from_slice(&std::fs::read(&contract.bundle_manifest)?)
        .context("invalid stock bundle manifest")?;
    let mut missing = Vec::new();
    for file in [
        Some(&bundle.executable),
        bundle.cli.as_ref(),
        Some(&bundle.code_mode_host),
        Some(&bundle.facade),
        Some(&bundle.schema),
    ]
    .into_iter()
    .flatten()
    {
        if !file.path.try_exists()? {
            missing.push(file.path.clone());
        }
    }
    Ok(missing)
}

/// The current package must load, except that a package whose pinned files
/// were deleted ([`missing_package_files`]) makes the upgrade a repair.
fn current_package_or_repair(
    contract: &ExplicitLaunchContract,
) -> anyhow::Result<Option<RuntimePackageRepair>> {
    let Err(error) = crate::launch::stock::StockBundle::load(contract) else {
        return Ok(None);
    };
    match missing_package_files(contract) {
        Ok(missing_files) if !missing_files.is_empty() => {
            Ok(Some(RuntimePackageRepair { missing_files }))
        }
        _ => Err(error.context("current runtime package is unavailable")),
    }
}

fn validate_upgrade_source(
    record: &CutexSessionRecord,
    sessions: &crate::session::model::CutexSessionStore,
    contract: &ExplicitLaunchContract,
) -> anyhow::Result<Option<RuntimePackageRepair>> {
    ensure!(
        crate::runtime::lifecycle::cutex_session_host_is_local(
            &record.host_id,
            &crate::platform::host::current_host_name(),
        ),
        "runtime package must be upgraded on its owning host"
    );
    ensure!(
        record.runtime_backend == crate::session::model::CutexSessionRuntimeBackend::Host,
        "runtime package upgrade requires the explicit host backend"
    );
    ensure!(
        !record.is_retired()
            && record.agent_enabled
            && record.registration_class
                == crate::agent_bus::model::AgentRegistrationClass::Persistent
            && record.codex_session_id.as_deref() == Some(contract.native_id.as_str()),
        "active exact adopted identity required"
    );
    ensure!(
        contract.version == 3 || contract.version == 4,
        "only local explicit-launch packages can be upgraded"
    );
    let repair = current_package_or_repair(contract)?;
    // The native identity, home, auth and history must validate either way:
    // a repair only replaces the package, never a broken home.
    crate::launch::stock::validate_native(record, sessions, contract)
        .context("current runtime home is invalid")?;
    if contract.version == 4 {
        let native_home = contract.native_home.canonicalize()?;
        ensure!(
            native_home
                == crate::config::paths::runtime_dir()?
                    .join("light/agents")
                    .join(&contract.native_id)
                    .canonicalize()?,
            "runtime package upgrade cannot move the v4 native home"
        );
    }
    ensure!(
        record.app_server_launch_claim_id.is_none()
            && !crate::session::archive::record_has_runtime_claim(record)
            && record.pending_launch_id.is_none(),
        "runtime package upgrade requires offline/no claim"
    );
    Ok(repair)
}

fn package_changed(current: &StockBundle, target: &StockBundle) -> bool {
    let left = [
        Some(&current.executable),
        current.cli.as_ref(),
        Some(&current.code_mode_host),
        Some(&current.facade),
        Some(&current.schema),
    ];
    let right = [
        Some(&target.executable),
        target.cli.as_ref(),
        Some(&target.code_mode_host),
        Some(&target.facade),
        Some(&target.schema),
    ];
    left.into_iter()
        .zip(right)
        .any(|(left, right)| match (left, right) {
            (Some(left), Some(right)) => left != right,
            _ => true,
        })
}

fn target_contract(
    current: &ExplicitLaunchContract,
    selected: &StockBundle,
) -> anyhow::Result<ExplicitLaunchContract> {
    let native_home = if current.version == 3 {
        crate::config::paths::runtime_dir()?
            .canonicalize()?
            .join("light")
            .join("agents")
            .join(&current.native_id)
    } else {
        current.native_home.canonicalize()?
    };
    let source_config = current.native_home.join("config.toml");
    let raw = std::fs::read_to_string(&source_config).with_context(|| {
        format!(
            "current runtime config missing: {}",
            source_config.display()
        )
    })?;
    let config = launch_shared_config(raw)?;
    let (_, digest) = projected_target_bundle(selected, native_home.join("config.toml"), &config)?;
    let manifest = native_home.join(format!("bundle-v4-{}.json", &digest.as_str()[..16]));
    Ok(ExplicitLaunchContract {
        version: 4,
        migration_action_id: None,
        native_id: current.native_id.clone(),
        native_home,
        bundle_manifest: manifest,
        bundle_sha256: digest,
    })
}

fn install_target_manifest(
    target: &ExplicitLaunchContract,
    selected_manifest: &Path,
    selected_manifest_sha256: &Sha256,
) -> anyhow::Result<()> {
    ensure!(
        file_sha256(selected_manifest)? == *selected_manifest_sha256,
        "selected runtime package changed during review"
    );
    let selected_bundle: StockBundle =
        serde_json::from_slice(&std::fs::read(selected_manifest)?)
            .context("selected runtime package manifest missing or invalid")?;
    if target.bundle_manifest.try_exists()? {
        ensure!(
            file_sha256(&target.bundle_manifest)? == target.bundle_sha256,
            "target runtime package manifest conflict"
        );
        return Ok(());
    }
    // Same path and bytes the review hashed: the native home's config.toml as
    // it will be read at launch, i.e. without the legacy projection marker.
    let config_path = target.native_home.join("config.toml");
    ensure!(
        config_path.canonicalize()? == config_path,
        "shared config must live in the native home"
    );
    let raw = std::fs::read_to_string(&config_path)
        .with_context(|| format!("target runtime config missing: {}", config_path.display()))?;
    let config = launch_shared_config(raw.clone())?;
    let (bytes, digest) = projected_target_bundle(&selected_bundle, config_path.clone(), &config)?;
    // A config edited after review (beyond the marker) changes the digest.
    ensure!(
        digest == target.bundle_sha256,
        "target runtime package digest changed"
    );
    if config != raw {
        crate::config::atomic::write_private_bytes_atomic(&config_path, config.as_bytes())?;
        ensure!(
            file_sha256(&config_path)? == bytes_sha256(config.as_bytes())?,
            "target runtime config write changed"
        );
    }
    crate::config::atomic::write_private_bytes_atomic(&target.bundle_manifest, &bytes)?;
    ensure!(
        file_sha256(&target.bundle_manifest)? == target.bundle_sha256,
        "target runtime package manifest write changed"
    );
    Ok(())
}

/// The shared config exactly as the launcher will read it: the legacy
/// `cutex_projection_version` marker is stripped (launch sanitizes it in place),
/// all other bytes are preserved.
fn launch_shared_config(raw: String) -> anyhow::Result<String> {
    let config = crate::launch::stock::remove_projection_marker(&raw)?.unwrap_or(raw);
    crate::launch::stock::validate_shared_config(&config)?;
    Ok(config)
}

/// The v4 side-car manifest for `selected` bound to the native home's config.
/// Review and apply both derive it here so they describe the same bytes.
fn projected_target_bundle(
    selected: &StockBundle,
    config_path: PathBuf,
    config: &str,
) -> anyhow::Result<(Vec<u8>, Sha256)> {
    let mut bundle = selected.clone();
    bundle.version = 4;
    bundle.launch_config_sha256 = None;
    bundle.shared_config = crate::launch::stock::VerifiedFile {
        path: config_path,
        sha256: bytes_sha256(config.as_bytes())?,
    };
    let bytes = serde_json::to_vec_pretty(&bundle)?;
    let digest = bytes_sha256(&bytes)?;
    Ok((bytes, digest))
}

fn bytes_sha256(bytes: &[u8]) -> anyhow::Result<Sha256> {
    use sha2::Digest;
    Sha256::new(format!("{:x}", sha2::Sha256::digest(bytes)))
        .map_err(|error| anyhow::anyhow!("invalid SHA-256 result: {error:?}"))
}

pub(super) fn zero_sha256() -> anyhow::Result<Sha256> {
    Sha256::new("0".repeat(64))
        .map_err(|error| anyhow::anyhow!("invalid placeholder SHA-256: {error:?}"))
}

fn package_receipt_path(native_home: &Path, action_id: &AgentActionId) -> anyhow::Result<PathBuf> {
    let digest = bytes_sha256(action_id.as_str().as_bytes())?;
    Ok(native_home
        .join("runtime-package-upgrades")
        .join(format!("{}.json", digest.as_str())))
}

fn write_package_receipt(
    path: &Path,
    receipt: &RuntimePackageUpgradeReceipt,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    crate::config::atomic::write_private_pretty_json_atomic(
        path,
        receipt,
        "runtime package receipt",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_change_compares_all_artifact_locations() {
        let file = |path: &str| crate::launch::stock::VerifiedFile {
            path: path.into(),
            sha256: Sha256::new("a".repeat(64)).unwrap(),
        };
        let base = |hash: &str| crate::launch::stock::StockBundle {
            version: 4,
            upstream_commit: "test".into(),
            native_patch_commit: Some("test".into()),
            executable: crate::launch::stock::VerifiedFile {
                sha256: Sha256::new(hash.to_string()).unwrap(),
                path: "/bin/app".into(),
            },
            cli: Some(file("/bin/cli")),
            code_mode_host: file("/bin/host"),
            facade: file("/bin/facade"),
            schema: crate::launch::stock::VerifiedFile {
                sha256: Sha256::new(crate::launch::stock::NATIVE_157_R44_SCHEMA_SHA256).unwrap(),
                path: "/bin/schema".into(),
            },
            shared_config: file("/home/config.toml"),
            launch_config_sha256: None,
        };
        assert!(!package_changed(
            &base(&"a".repeat(64)),
            &base(&"a".repeat(64))
        ));
        assert!(package_changed(
            &base(&"a".repeat(64)),
            &base(&"b".repeat(64))
        ));
        let mut moved = base(&"a".repeat(64));
        moved.executable.path = "/bin/app-new".into();
        assert!(package_changed(&base(&"a".repeat(64)), &moved));
    }

    struct UpgradeFixture {
        root: PathBuf,
        current: ExplicitLaunchContract,
        selected: StockBundle,
        selected_manifest: PathBuf,
        selected_sha256: Sha256,
    }

    fn upgrade_fixture(config: &str) -> UpgradeFixture {
        let root = std::env::temp_dir().join(format!("runtime-package-{}", uuid::Uuid::new_v4()));
        let native_home = root.join("native-home");
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&native_home)
                .unwrap();
        }
        #[cfg(not(unix))]
        std::fs::create_dir_all(&native_home).unwrap();
        let native_home = native_home.canonicalize().unwrap();
        std::fs::write(native_home.join("config.toml"), config).unwrap();
        let file = |name: &str, hash: char| crate::launch::stock::VerifiedFile {
            path: root.join(name),
            sha256: Sha256::new(hash.to_string().repeat(64)).unwrap(),
        };
        let selected = StockBundle {
            version: 4,
            upstream_commit: "test".into(),
            native_patch_commit: Some("test".into()),
            executable: file("new-codex", 'a'),
            cli: Some(file("new-cli", 'b')),
            code_mode_host: file("new-host", 'c'),
            facade: file("new-facade", 'd'),
            schema: file("new-schema", 'e'),
            shared_config: file("selected-home/config.toml", 'f'),
            launch_config_sha256: None,
        };
        let selected_manifest = root.join("selected.json");
        std::fs::write(
            &selected_manifest,
            serde_json::to_vec_pretty(&selected).unwrap(),
        )
        .unwrap();
        let selected_sha256 = file_sha256(&selected_manifest).unwrap();
        let current_manifest = native_home.join("bundle-v4-current.json");
        std::fs::write(&current_manifest, b"{}").unwrap();
        let current = ExplicitLaunchContract {
            version: 4,
            migration_action_id: None,
            native_id: uuid::Uuid::new_v4().to_string(),
            native_home: native_home.clone(),
            bundle_manifest: current_manifest.canonicalize().unwrap(),
            bundle_sha256: file_sha256(&current_manifest).unwrap(),
        };
        UpgradeFixture {
            root,
            current,
            selected,
            selected_manifest,
            selected_sha256,
        }
    }

    /// Review then apply; afterwards the launch-time checks must accept it.
    fn review_apply_and_verify_launch(fixture: &UpgradeFixture) -> ExplicitLaunchContract {
        let target = target_contract(&fixture.current, &fixture.selected).unwrap();
        install_target_manifest(
            &target,
            &fixture.selected_manifest,
            &fixture.selected_sha256,
        )
        .unwrap();
        // The installed contract is the reviewed one, byte for byte.
        target.validate().unwrap();
        let installed: StockBundle =
            serde_json::from_slice(&std::fs::read(&target.bundle_manifest).unwrap()).unwrap();
        let config = target.native_home.join("config.toml");
        assert_eq!(installed.shared_config.path, config);
        assert_eq!(
            installed.shared_config.sha256,
            file_sha256(&config).unwrap()
        );
        assert_eq!(installed.executable, fixture.selected.executable);
        // Launch strips nothing further and sees the native home's own config.
        let before = std::fs::read(&config).unwrap();
        super::super::stock_runtime::sanitize_shared_projection_marker(&target).unwrap();
        assert_eq!(std::fs::read(&config).unwrap(), before);
        assert_eq!(
            installed.shared_config.path.canonicalize().unwrap(),
            target
                .native_home
                .canonicalize()
                .unwrap()
                .join("config.toml")
        );
        // Replaying the apply step is idempotent.
        install_target_manifest(
            &target,
            &fixture.selected_manifest,
            &fixture.selected_sha256,
        )
        .unwrap();
        target
    }

    #[test]
    fn only_a_package_with_deleted_files_is_repairable() {
        let fixture = upgrade_fixture("model = 'gpt-5'\n");
        // The current manifest pins files of a release that was deleted,
        // except the executable.
        let manifest = fixture.current.bundle_manifest.clone();
        std::fs::write(&manifest, serde_json::to_vec_pretty(&fixture.selected).unwrap()).unwrap();
        let mut current = fixture.current.clone();
        current.bundle_sha256 = file_sha256(&manifest).unwrap();
        std::fs::write(&fixture.selected.executable.path, b"exe").unwrap();
        let missing = missing_package_files(&current).unwrap();
        assert_eq!(
            missing,
            vec![
                fixture.root.join("new-cli"),
                fixture.root.join("new-host"),
                fixture.root.join("new-facade"),
                fixture.root.join("new-schema"),
            ]
        );
        let repair = current_package_or_repair(&current).unwrap().unwrap();
        assert_eq!(repair.missing_files, missing);

        // Every file present but not the pinned bytes: no repair, the normal
        // refusal stands.
        for name in ["new-cli", "new-host", "new-facade", "new-schema"] {
            std::fs::write(fixture.root.join(name), b"other").unwrap();
        }
        assert!(missing_package_files(&current).unwrap().is_empty());
        let error = current_package_or_repair(&current).unwrap_err();
        assert!(format!("{error:#}").contains("current runtime package is unavailable"), "{error:#}");

        // A manifest that changed (or an older contract) is never repairable.
        std::fs::remove_file(fixture.root.join("new-facade")).unwrap();
        std::fs::write(&manifest, b"{\"changed\":true}").unwrap();
        assert!(missing_package_files(&current).is_err());
        assert!(current_package_or_repair(&current).is_err());
        let mut v2 = current.clone();
        v2.version = 2;
        assert!(missing_package_files(&v2).is_err());
        std::fs::remove_dir_all(&fixture.root).unwrap();
    }

    #[test]
    fn marked_v4_config_review_matches_apply_and_launches_stripped() {
        let fixture = upgrade_fixture("cutex_projection_version = 2\nmodel = 'gpt-5'\n");
        let target = review_apply_and_verify_launch(&fixture);
        let raw = std::fs::read_to_string(target.native_home.join("config.toml")).unwrap();
        assert!(!raw.contains("cutex_projection_version"));
        assert_eq!(
            toml::from_str::<toml::Value>(&raw).unwrap()["model"].as_str(),
            Some("gpt-5")
        );
        std::fs::remove_dir_all(&fixture.root).unwrap();
    }

    #[test]
    fn unmarked_v4_config_is_installed_byte_identical() {
        let original = "# keep my comment\nmodel = \"gpt-5\"\n";
        let fixture = upgrade_fixture(original);
        let target = review_apply_and_verify_launch(&fixture);
        assert_eq!(
            std::fs::read_to_string(target.native_home.join("config.toml")).unwrap(),
            original
        );
        std::fs::remove_dir_all(&fixture.root).unwrap();
    }

    #[test]
    fn revert_requires_the_upgraded_contract_and_an_offline_record() {
        let root = std::env::temp_dir().join(format!("runtime-revert-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("cutex-sessions.json");
        let native_id = uuid::Uuid::new_v4().to_string();
        let contract = |name: &str| ExplicitLaunchContract {
            version: 4,
            migration_action_id: None,
            native_id: native_id.clone(),
            native_home: root.join("home"),
            bundle_manifest: root.join(format!("{name}.json")),
            bundle_sha256: Sha256::new("a".repeat(64)).unwrap(),
        };
        let (old, upgraded, other) = (contract("old"), contract("upgraded"), contract("other"));
        let id = CutexSessionId::new("cutex.revert-test".to_string()).unwrap();
        let mut record = CutexSessionRecord::new_at(
            id.as_str().to_string(),
            Some(native_id.clone()),
            crate::platform::host::current_host_name(),
            root.display().to_string(),
            None,
            "2026-10-08T00:00:00Z".to_string(),
        )
        .unwrap();
        record.explicit_launch = Some(upgraded.clone());
        let mut store = crate::session::model::CutexSessionStore::default();
        store.sessions.insert(id.as_str().to_string(), record);
        crate::session::store::save_cutex_session_store_to_path(&path, &store).unwrap();
        let before = crate::session::store::session_store_json_bytes(&path).unwrap();
        let ok = |_: &CutexSessionRecord, _: &crate::session::model::CutexSessionStore| Ok(());

        // The contract changed since the upgrade (e.g. another upgrade): refuse, no write.
        let error = revert_locked(&path, &id, &other, &old, ok).unwrap_err();
        assert!(error.to_string().contains("contract_changed"), "{error:#}");
        assert_eq!(crate::session::store::session_store_json_bytes(&path).unwrap(), before);
        // A different native identity is never a revert.
        let mut foreign = old.clone();
        foreign.native_id = uuid::Uuid::new_v4().to_string();
        assert!(revert_locked(&path, &id, &upgraded, &foreign, ok).is_err());
        // The restored package must validate.
        let error = revert_locked(&path, &id, &upgraded, &old, |_, _| {
            anyhow::bail!("previous runtime package is unavailable")
        })
        .unwrap_err();
        assert!(error.to_string().contains("unavailable"), "{error:#}");
        assert_eq!(crate::session::store::session_store_json_bytes(&path).unwrap(), before);
        // A runtime claim (online agent) is refused.
        let set_pid = |pid: Option<u32>| {
            crate::session::store::with_locked_session_store(&path, |sessions| {
                sessions.sessions.get_mut(id.as_str()).unwrap().runtime_pid = pid;
                crate::session::store::save_locked_session_store(&path, sessions)
            })
            .unwrap()
        };
        set_pid(Some(42));
        let error = revert_locked(&path, &id, &upgraded, &old, ok).unwrap_err();
        assert!(error.to_string().contains("offline/no claim"), "{error:#}");
        set_pid(None);

        let revision = store.sessions[id.as_str()].durable_revision();
        let receipt = revert_locked(&path, &id, &upgraded, &old, ok).unwrap();
        assert_eq!(receipt.restored_contract, old);
        assert_eq!(receipt.replaced_contract, upgraded);
        assert_eq!(receipt.activated_revision, revision + 1);
        let saved = crate::session::store::load_cutex_session_store_from_path(&path).unwrap();
        let saved = &saved.sessions[id.as_str()];
        assert_eq!(saved.explicit_launch.as_ref(), Some(&old));
        assert_eq!(saved.durable_revision(), revision + 1);
        // Replaying the same revert finds the restored contract: refused.
        let error = revert_locked(&path, &id, &upgraded, &old, ok).unwrap_err();
        assert!(error.to_string().contains("contract_changed"), "{error:#}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn config_edited_after_review_is_refused_without_writes() {
        for header in ["cutex_projection_version = 2\n", ""] {
            let fixture = upgrade_fixture(&format!("{header}model = 'gpt-5'\n"));
            let target = target_contract(&fixture.current, &fixture.selected).unwrap();
            let config = target.native_home.join("config.toml");
            let edited = format!("{header}model = 'gpt-5'\nmodel_reasoning_effort = 'high'\n");
            std::fs::write(&config, &edited).unwrap();
            let error = install_target_manifest(
                &target,
                &fixture.selected_manifest,
                &fixture.selected_sha256,
            )
            .unwrap_err();
            assert!(error.to_string().contains("digest changed"), "{error:#}");
            assert_eq!(std::fs::read_to_string(&config).unwrap(), edited);
            assert!(!target.bundle_manifest.exists());
            std::fs::remove_dir_all(&fixture.root).unwrap();
        }
    }
}
