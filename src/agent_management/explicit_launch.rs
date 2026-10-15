//! Human-only execution selection. No removal or implicit backend fallback.
use super::*;
use crate::management::control_plane::HumanManagementPrincipal;
use crate::role_revision::{CutexSessionId, Sha256};
use crate::session::model::CutexSessionRecord;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExplicitLaunchContract {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration_action_id: Option<AgentActionId>,
    pub native_id: String,
    pub native_home: PathBuf,
    pub bundle_manifest: PathBuf,
    pub bundle_sha256: Sha256,
}

impl ExplicitLaunchContract {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(self.version, 1 | 2 | 3 | 4),
            "unsupported explicit launch contract version"
        );
        anyhow::ensure!((self.version == 3) == self.migration_action_id.is_some(),
            "migration contract requires explicit action; legacy contracts cannot carry maintenance authority");
        anyhow::ensure!(
            uuid::Uuid::parse_str(&self.native_id)?.to_string() == self.native_id,
            "exact native UUID required"
        );
        for path in [&self.native_home, &self.bundle_manifest] {
            anyhow::ensure!(
                path.is_absolute() && path.canonicalize()? == *path,
                "canonical existing explicit launch path required"
            );
        }
        anyhow::ensure!(self.native_home.is_dir(), "native home missing");
        anyhow::ensure!(
            file_sha256(&self.bundle_manifest)? == self.bundle_sha256,
            "bundle evidence missing or changed"
        );
        Ok(())
    }
}

/// SHA-256 of a file. Large files (native executables, 70-330 MB each, hashed
/// several times per create / start) reuse a digest computed by this process
/// while their size, modification and creation times are unchanged, for at
/// most [`DIGEST_MEMO_TTL`]. Files below [`DIGEST_MEMO_MIN_BYTES`] (manifests,
/// configs, auth) are always hashed. The memo does not weaken the existing
/// guarantee: the check was never atomic with the later exec, so a same-user
/// writer able to replace a binary while forging its timestamps could already
/// do so between verification and use.
pub fn file_sha256(path: &Path) -> anyhow::Result<Sha256> {
    let started = std::time::Instant::now();
    let digest = digest_memo::file_sha256_memoized(path, file_sha256_uncached);
    if std::env::var_os("CUTEX_DEBUG_CREATE_TIMING").is_some_and(|v| v == "1") {
        eprintln!("[create-timing] sha256 {} {}ms", path.display(), started.elapsed().as_millis());
    }
    digest
}

/// Files at least this large are memoized by [`file_sha256`].
pub const DIGEST_MEMO_MIN_BYTES: u64 = 1024 * 1024;
/// A memoized digest is recomputed after this long even if metadata is unchanged.
pub const DIGEST_MEMO_TTL: std::time::Duration = std::time::Duration::from_secs(600);

mod digest_memo {
    use super::{Sha256, DIGEST_MEMO_MIN_BYTES, DIGEST_MEMO_TTL};
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::{Instant, SystemTime};

    #[derive(Clone, PartialEq, Eq)]
    struct Stamp {
        len: u64,
        modified: Option<SystemTime>,
        created: Option<SystemTime>,
    }

    static MEMO: Mutex<Option<HashMap<PathBuf, (Stamp, Instant, Sha256)>>> = Mutex::new(None);

    fn stamp(path: &Path) -> Option<Stamp> {
        let metadata = std::fs::metadata(path).ok()?;
        (metadata.is_file() && metadata.len() >= DIGEST_MEMO_MIN_BYTES).then(|| Stamp {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
        })
    }

    pub(super) fn file_sha256_memoized(
        path: &Path,
        compute: impl Fn(&Path) -> anyhow::Result<Sha256>,
    ) -> anyhow::Result<Sha256> {
        let Some(before) = stamp(path) else {
            return compute(path);
        };
        if let Ok(memo) = MEMO.lock() {
            if let Some((stamp, at, digest)) = memo.as_ref().and_then(|memo| memo.get(path)) {
                if *stamp == before && at.elapsed() < DIGEST_MEMO_TTL {
                    return Ok(digest.clone());
                }
            }
        }
        let digest = compute(path)?;
        // Only remember a digest whose file did not change while it was read.
        if stamp(path).as_ref() == Some(&before) {
            if let Ok(mut memo) = MEMO.lock() {
                memo.get_or_insert_with(HashMap::new)
                    .insert(path.to_path_buf(), (before, Instant::now(), digest.clone()));
            }
        }
        Ok(digest)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[test]
        fn large_files_reuse_digest_until_metadata_changes_small_files_never() {
            let dir = std::env::temp_dir().join(format!("cutex-digest-memo-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&dir).unwrap();
            let large = dir.join("large.bin");
            let small = dir.join("small.json");
            std::fs::write(&large, vec![7u8; DIGEST_MEMO_MIN_BYTES as usize]).unwrap();
            std::fs::write(&small, b"{}").unwrap();
            let calls = AtomicUsize::new(0);
            let compute = |path: &Path| {
                calls.fetch_add(1, Ordering::SeqCst);
                super::super::file_sha256_uncached(path)
            };
            let first = file_sha256_memoized(&large, compute).unwrap();
            assert_eq!(file_sha256_memoized(&large, compute).unwrap(), first);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            // Same size, new content and modification time: recomputed.
            std::thread::sleep(std::time::Duration::from_millis(20));
            std::fs::write(&large, vec![8u8; DIGEST_MEMO_MIN_BYTES as usize]).unwrap();
            let changed = file_sha256_memoized(&large, compute).unwrap();
            assert_ne!(changed, first);
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            file_sha256_memoized(&small, compute).unwrap();
            file_sha256_memoized(&small, compute).unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 4);
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }
}

fn file_sha256_uncached(path: &Path) -> anyhow::Result<Sha256> {
    use sha2::Digest;
    use std::io::Read;
    let mut input = std::fs::File::open(path)?;
    let mut hash = sha2::Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Sha256::new(format!("{:x}", hash.finalize())).map_err(|_| anyhow::anyhow!("invalid digest"))
}

/// Explain unsupported legacy startup before launching a process or reading old logs.
pub fn require_migrated_runtime(record: &CutexSessionRecord) -> anyhow::Result<()> {
    if record.runtime_backend == crate::session::model::CutexSessionRuntimeBackend::CuteAlden
        && record.registration_class == crate::agent_bus::model::AgentRegistrationClass::Persistent
        && record.explicit_launch.is_none()
    {
        let name=crate::session::metadata::cutex_session_display_name(record);
        anyhow::bail!("migration_required: {name} uses the legacy cute-alden backend. Migrate this Agent before starting it ({}).",record.cutex_session_id);
    }
    Ok(())
}

#[test]
fn explicit_launch_versions_require_unchanged_manifest_evidence() {
    let root = std::env::temp_dir().join(format!("s6f-contract-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    // The contract requires canonical paths, including the Windows verbatim prefix.
    let root = root.canonicalize().unwrap();
    let manifest = root.join("bundle.json");
    std::fs::write(&manifest, b"private evidence").unwrap();
    let mut contract = ExplicitLaunchContract {
        version: 1,
        migration_action_id: None,
        native_id: uuid::Uuid::new_v4().to_string(),
        native_home: root.clone(),
        bundle_manifest: manifest.clone(),
        bundle_sha256: file_sha256(&manifest).unwrap(),
    };
    contract.validate().unwrap();
    contract.version = 2;
    contract.validate().unwrap();
    contract.version = 3;
    assert!(contract.validate().is_err());
    contract.version = 2;
    std::fs::write(&manifest, b"changed evidence").unwrap();
    assert!(contract.validate().is_err());
    std::fs::remove_file(manifest).unwrap();
    std::fs::remove_dir(root).unwrap();
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExplicitLaunchReview {
    pub subject: ExplicitLaunchSubject,
    pub contract: ExplicitLaunchContract,
    pub configuration: crate::launch::stock::StockConfiguration,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExplicitLaunchSubject {
    pub cutex_session_id: CutexSessionId,
    pub formal_name: String,
    pub durable_sha256: Sha256,
    pub authority_sha256: Sha256,
    pub current_project_id: Option<ProjectId>,
    pub revision: u64,
    pub runtime_generation: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExplicitLaunchReceipt {
    pub action_id: AgentActionId,
    pub review: ExplicitLaunchReview,
    pub activated_revision: u64,
    pub committed_at: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "receipt", rename_all = "snake_case")]
pub enum ExplicitLaunchActionReceipt {
    Bootstrap(BootstrapAdoptionReceipt),
    Activation(ExplicitLaunchReceipt),
    Runtime(StockRuntimeReceipt),
    #[cfg(target_os = "linux")]
    Maintenance(MaintenanceReceipt),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExplicitLaunchRequest {
    RuntimeStatus {
        action_id: AgentActionId,
    },
    RecoverRuntime {
        cutex_session_id: CutexSessionId,
        action_id: AgentActionId,
    },
    #[cfg(target_os = "linux")]
    MaintenanceReview {
        request: MaintenanceReviewRequest,
    },
    #[cfg(target_os = "linux")]
    MaintenanceApply {
        review: MaintenanceReview,
    },
    #[cfg(target_os = "linux")]
    MaintenanceStatus {
        action_id: AgentActionId,
    },
    #[cfg(target_os = "linux")]
    MaintenanceStart {
        action_id: AgentActionId,
    },
    #[cfg(target_os = "linux")]
    MaintenanceRecoveryReview {
        action_id: AgentActionId,
    },
    #[cfg(target_os = "linux")]
    MaintenanceRecoveryStart {
        request: MaintenanceRecoveryStartRequest,
    },
    ReviewBootstrap {
        request: AgentManagementRequest,
        native_home: PathBuf,
        bundle_manifest: PathBuf,
        bundle_sha256: Sha256,
        expires_at_unix: i64,
        #[serde(default)]
        job_mcp: Option<crate::launch::job_mcp::JobMcpDescriptor>,
    },
    AuthorizeBootstrap {
        review: BootstrapIntentReview,
    },
    ReviewRuntime {
        cutex_session_id: CutexSessionId,
        restart: bool,
        #[serde(default)]
        receiver_canonical_byte_limit: crate::launch::stock::CanonicalBytePolicy,
        #[serde(default)]
        job_mcp: Option<crate::launch::job_mcp::JobMcpDescriptor>,
    },
    ReviewRuntimePackageUpgrade {
        cutex_session_id: CutexSessionId,
    },
    ApplyRuntimePackageUpgrade {
        action_id: AgentActionId,
        review: RuntimePackageUpgradeReview,
    },
    /// Additive (Management v2 runtime-package runs): put `explicit_launch`
    /// back from `expected_contract` (what an upgrade wrote) to
    /// `restore_contract` (what it replaced).
    RevertRuntimePackageUpgrade {
        cutex_session_id: CutexSessionId,
        expected_contract: ExplicitLaunchContract,
        restore_contract: ExplicitLaunchContract,
    },
    /// Additive (Management v2 runtime-package runs): clear a runtime claim
    /// left on an offline agent by a runtime that exited (or a crashed
    /// launch), only when its processes are proven gone. Executed by the
    /// daemon's runtime owner, like `Run`.
    ReconcileExitedRuntime {
        cutex_session_id: CutexSessionId,
    },
    Run {
        action_id: AgentActionId,
        review: StockRuntimeReview,
    },
    Review {
        cutex_session_id: CutexSessionId,
        contract: ExplicitLaunchContract,
    },
    Activate {
        action_id: AgentActionId,
        review: ExplicitLaunchReview,
    },
}

impl AgentManagementProvider {
    pub fn explicit_launch_action(
        &self,
        _principal: &HumanManagementPrincipal,
        path: &Path,
        request: &ExplicitLaunchRequest,
    ) -> anyhow::Result<serde_json::Value> {
        match request {
            ExplicitLaunchRequest::RuntimeStatus { action_id } => Ok(serde_json::to_value(
                self.runtime_action_status(path, action_id)?,
            )?),
            ExplicitLaunchRequest::RecoverRuntime { .. } => {
                anyhow::bail!("Human runtime recovery executor required")
            }
            #[cfg(target_os = "linux")]
            ExplicitLaunchRequest::MaintenanceReview { request } => Ok(serde_json::to_value(
                self.review_maintenance(_principal, path, request)?,
            )?),
            #[cfg(target_os = "linux")]
            ExplicitLaunchRequest::MaintenanceApply { review } => Ok(serde_json::to_value(
                self.apply_maintenance(_principal, path, review)?,
            )?),
            #[cfg(target_os = "linux")]
            ExplicitLaunchRequest::MaintenanceStatus { action_id } => Ok(serde_json::to_value(
                self.maintenance_status(_principal, path, action_id)?,
            )?),
            #[cfg(target_os = "linux")]
            ExplicitLaunchRequest::MaintenanceRecoveryReview { action_id } => {
                Ok(serde_json::to_value(
                    self.review_start_maintenance_recovery(_principal, path, action_id)?,
                )?)
            }
            #[cfg(target_os = "linux")]
            ExplicitLaunchRequest::MaintenanceStart { .. }
            | ExplicitLaunchRequest::MaintenanceRecoveryStart { .. } => {
                anyhow::bail!("Human maintenance executor required")
            }
            ExplicitLaunchRequest::ReviewBootstrap {
                request,
                native_home,
                bundle_manifest,
                bundle_sha256,
                expires_at_unix,
                job_mcp,
            } => {
                // r219: refuses a removed `director_rotate` before any review.
                request.validate()?;
                let _mutation = self.store().lock_mutations()?;
                let state = self.store().snapshot()?;
                let project = request
                    .project_id
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("bootstrap requires exact project"))?;
                let authority = state
                    .projects
                    .get(project)
                    .ok_or_else(|| anyhow::anyhow!("bootstrap project missing"))?;
                let spec = request.operation.bootstrap_spec().ok_or_else(|| {
                    anyhow::anyhow!("bootstrap intent requires create/replace")
                })?;
                let review = BootstrapIntentReview {
                    version: if matches!(
                        &request.operation,
                        AgentOperation::Create {
                            start_mode: AgentStartMode::BootstrapOnly,
                            frozen_message: None,
                            ..
                        }
                    ) {
                        1
                    } else {
                        2
                    },
                    request: request.clone(),
                    director: authority.authorized_director_session.clone(),
                    authority_sha256: super::store::request_sha256(authority)?,
                    store_revision: state.store_revision,
                    native_home: native_home.clone(),
                    bundle_manifest: bundle_manifest.clone(),
                    bundle_sha256: bundle_sha256.clone(),
                    configuration: crate::launch::stock::bootstrap_configuration(spec)?,
                    runtime_groups: crate::agent_bus::groups::normalize_registered_agent_groups(
                        spec.groups.clone(),
                        None,
                        &spec.cwd,
                    ),
                    expires_at_unix: *expires_at_unix,
                    job_mcp: job_mcp
                        .as_ref()
                        .map(|job| {
                            let bundle = crate::launch::stock::StockBundle::load_references(
                                native_home,
                                bundle_manifest,
                                bundle_sha256,
                            )?;
                            job.review(&bundle)
                        })
                        .transpose()?,
                };
                review.validate_evidence()?;
                anyhow::ensure!(
                    *expires_at_unix > chrono::Utc::now().timestamp(),
                    "bootstrap intent expired"
                );
                Ok(serde_json::to_value(review)?)
            }
            ExplicitLaunchRequest::AuthorizeBootstrap { review } => Ok(serde_json::to_value(
                self.authorize_bootstrap_intent(review)?,
            )?),
            ExplicitLaunchRequest::ReviewRuntime {
                cutex_session_id,
                restart,
                receiver_canonical_byte_limit,
                job_mcp,
            } => self
                .review_stock_runtime(path, cutex_session_id, *restart)
                .and_then(|mut r| {
                    anyhow::ensure!(
                        crate::launch::stock::StockBundle::load(&r.contract)?.common_ingress()
                            || receiver_canonical_byte_limit.is_default(),
                        "unchanged stock is registration-only; receiver ingress policy unsupported"
                    );
                    r.receiver_canonical_byte_limit = receiver_canonical_byte_limit.clone();
                    let bundle = crate::launch::stock::StockBundle::load(&r.contract)?;
                    r.job_mcp = if let Some(job) = job_mcp.as_ref() {
                        Some(job.review(&bundle)?)
                    } else {
                        inherited_runtime_job(path, cutex_session_id, &r.contract)?
                    };
                    r.configuration
                        .validate_job_requirement(r.job_mcp.is_some())?;
                    Ok(serde_json::to_value(r)?)
                }),
            ExplicitLaunchRequest::ReviewRuntimePackageUpgrade { cutex_session_id } => self
                .review_runtime_package_upgrade(path, cutex_session_id)
                .and_then(|review| serde_json::to_value(review).map_err(Into::into)),
            ExplicitLaunchRequest::ApplyRuntimePackageUpgrade { action_id, review } => self
                .apply_runtime_package_upgrade(_principal, path, action_id, review)
                .and_then(|receipt| serde_json::to_value(receipt).map_err(Into::into)),
            ExplicitLaunchRequest::RevertRuntimePackageUpgrade {
                cutex_session_id,
                expected_contract,
                restore_contract,
            } => self
                .revert_runtime_package_upgrade(
                    _principal,
                    path,
                    cutex_session_id,
                    expected_contract,
                    restore_contract,
                )
                .and_then(|receipt| serde_json::to_value(receipt).map_err(Into::into)),
            ExplicitLaunchRequest::Run { .. } | ExplicitLaunchRequest::ReconcileExitedRuntime { .. } => {
                anyhow::bail!("explicit stock runtime executor required")
            }
            ExplicitLaunchRequest::Review {
                cutex_session_id,
                contract,
            } => {
                anyhow::ensure!(
                    contract.migration_action_id.is_none() && contract.version != 3,
                    "migration requires dedicated Human maintenance entry"
                );
                let _mutation = self.store().lock_mutations()?;
                {
                    let state = self.store().snapshot()?;
                    let current_project_id = super::archive::guard(&state, cutex_session_id)
                        .map_err(|e| {
                            anyhow::anyhow!("explicit launch protected-role/project guard: {e}")
                        })?;
                    let sessions = crate::session::store::load_cutex_session_store_from_path(path)?;
                    let record = sessions
                        .sessions
                        .get(cutex_session_id.as_str())
                        .ok_or_else(|| anyhow::anyhow!("explicit launch durable record missing"))?;
                    validate_activation(record, contract)?;
                    crate::launch::stock::StockBundle::load(contract)?;
                    crate::launch::stock::validate_native(record, &sessions, contract)?;
                    let configuration = crate::launch::stock::current_configuration(record)?;
                    let formal_name = record
                        .formal_agent_name
                        .clone()
                        .or_else(|| {
                            state
                                .agents
                                .get(cutex_session_id)
                                .map(|a| a.spec.name.clone())
                        })
                        .ok_or_else(|| anyhow::anyhow!("explicit formal Agent name required"))?;
                    anyhow::ensure!(
                        !formal_name.trim().is_empty()
                            && !formal_name.chars().any(char::is_control),
                        "malformed formal Agent name"
                    );
                    Ok(serde_json::to_value(ExplicitLaunchReview {
                        subject: ExplicitLaunchSubject {
                            cutex_session_id: cutex_session_id.clone(),
                            formal_name,
                            durable_sha256: super::store::request_sha256(record)?,
                            authority_sha256: super::archive::authority_digest(
                                &state,
                                cutex_session_id,
                            )?,
                            current_project_id,
                            revision: record.revision,
                            runtime_generation: record.runtime_generation,
                        },
                        contract: contract.clone(),
                        configuration,
                    })?)
                }
            }
            ExplicitLaunchRequest::Activate { action_id, review } => {
                anyhow::ensure!(
                    review.contract.migration_action_id.is_none() && review.contract.version != 3,
                    "migration requires dedicated Human maintenance entry"
                );
                let _execution = super::provider::provider_execution_lock()
                    .lock()
                    .map_err(|_| anyhow::anyhow!("execution lock unavailable"))?;
                let _mutation = self.store().lock_mutations()?;
                {
                    crate::session::store::with_locked_session_store(path, |sessions| {
                        if let Some(receipt) =
                            crate::session::receipt_retention::explicit_launch_receipt(
                                sessions,
                                action_id.as_str(),
                            )?
                        {
                            let ExplicitLaunchActionReceipt::Activation(receipt) = receipt else {
                                anyhow::bail!("explicit_launch_action_conflict")
                            };
                            anyhow::ensure!(
                                &receipt.review == review,
                                "explicit_launch_action_conflict"
                            );
                            return Ok(serde_json::to_value(receipt)?);
                        }
                        crate::launch::stock::StockBundle::load(&review.contract)?;
                        let state = self.store().snapshot()?;
                        let id = &review.subject.cutex_session_id;
                        super::archive::guard(&state, id)?;
                        anyhow::ensure!(
                            super::archive::authority_digest(&state, id)?
                                == review.subject.authority_sha256,
                            "explicit_launch_authority_stale"
                        );
                        let record = sessions
                            .sessions
                            .get(id.as_str())
                            .ok_or_else(|| anyhow::anyhow!("durable record missing"))?;
                        crate::launch::stock::validate_native(record, sessions, &review.contract)?;
                        anyhow::ensure!(
                            crate::launch::stock::current_configuration(record)?
                                == review.configuration,
                            "explicit_launch_configuration_stale"
                        );
                        let record = sessions
                            .sessions
                            .get_mut(id.as_str())
                            .ok_or_else(|| anyhow::anyhow!("durable record missing"))?;
                        anyhow::ensure!(
                            super::store::request_sha256(record)? == review.subject.durable_sha256
                                && record.revision == review.subject.revision
                                && record.runtime_generation == review.subject.runtime_generation,
                            "explicit_launch_confirmation_stale"
                        );
                        validate_activation(record, &review.contract)?;
                        record.explicit_launch = Some(review.contract.clone());
                        record.revision = record
                            .revision
                            .checked_add(1)
                            .ok_or_else(|| anyhow::anyhow!("revision exhausted"))?;
                        record.updated_at = chrono::Utc::now().to_rfc3339();
                        let receipt = ExplicitLaunchReceipt {
                            action_id: action_id.clone(),
                            review: review.clone(),
                            activated_revision: record.revision,
                            committed_at: record.updated_at.clone(),
                        };
                        sessions.explicit_launch_receipts.insert(
                            action_id.to_string(),
                            ExplicitLaunchActionReceipt::Activation(receipt.clone()),
                        );
                        // Requirement and immutable action/audit receipt share the durable CAS write.
                        crate::session::store::save_locked_session_store(path, sessions)?;
                        Ok(serde_json::to_value(receipt)?)
                    })
                }
            }
        }
    }
}

fn validate_activation(
    record: &CutexSessionRecord,
    contract: &ExplicitLaunchContract,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        record.explicit_launch.is_none(),
        "explicit launch already activated; no downgrade API"
    );
    anyhow::ensure!(
        !record.is_retired()
            && record.agent_enabled
            && record.codex_session_id.as_deref() == Some(contract.native_id.as_str()),
        "active exact adopted identity required"
    );
    anyhow::ensure!(
        !crate::session::archive::record_has_runtime_claim(record),
        "explicit launch requires proven offline/no claim"
    );
    anyhow::ensure!(
        record.registration_class == crate::agent_bus::model::AgentRegistrationClass::Persistent,
        "persistent identity required"
    );
    Ok(())
}

/// Shared default resolution for ordinary and typed management launches.
/// Refresh only the Job daemon occurrence; keep historical receipts immutable.
pub(super) fn inherited_runtime_job(
    path: &Path,
    id: &CutexSessionId,
    contract: &ExplicitLaunchContract,
) -> anyhow::Result<Option<crate::launch::job_mcp::ReviewedJobMcp>> {
    let sessions = crate::session::store::load_cutex_session_store_from_path(path)?;
    if sessions.sessions.get(id.as_str()).is_some_and(|r| r.is_owned_session()) { return Ok(None); }
    let saved = saved_runtime_job(&sessions, id, contract).map(|job| job.descriptor.clone());
    let selected =
        crate::launch::local_deployment::LocalDeployment::selected()?.and_then(|d| d.job_mcp);
    let descriptor = match (saved, selected) {
        // Installation may replace the daemon/adapter while retaining its
        // endpoint and credentials. Refresh new launches from that installation;
        // never rewrite an existing action receipt or move an agent to another service.
        (Some(saved), Some(selected))
            if saved.endpoint == selected.endpoint
                && saved.api_token_file == selected.api_token_file
                && saved.grant_key_file == selected.grant_key_file =>
        {
            Some(selected)
        }
        (Some(saved), _) => Some(saved),
        (None, selected) => selected,
    };
    let bundle = crate::launch::stock::StockBundle::load(contract)?;
    descriptor
        .map(|job| job.review_current(&bundle))
        .transpose()
}

/// Reuse configuration from the most recent completed launch of this contract,
/// falling back to its migration receipt. Occurrence evidence is refreshed only
/// when creating a new review; persisted receipts remain unchanged.
fn saved_runtime_job<'a>(
    sessions: &'a crate::session::model::CutexSessionStore,
    id: &CutexSessionId,
    contract: &ExplicitLaunchContract,
) -> Option<&'a crate::launch::job_mcp::ReviewedJobMcp> {
    let latest = sessions
        .explicit_launch_receipts
        .values()
        .filter_map(|receipt| match receipt {
            ExplicitLaunchActionReceipt::Runtime(r)
                if &r.review.subject.cutex_session_id == id
                    && r.review.contract.native_id == contract.native_id
                    && r.review.contract.native_home == contract.native_home
                    && r.stage == StockRuntimeStage::Ready =>
            {
                Some(r)
            }
            _ => None,
        })
        .max_by_key(|r| r.expected_generation);
    if let Some(runtime) = latest {
        return runtime.review.job_mcp.as_ref();
    }
    #[cfg(target_os = "linux")]
    if let Some(ExplicitLaunchActionReceipt::Maintenance(receipt)) = contract
        .migration_action_id
        .as_ref()
        .and_then(|action| sessions.explicit_launch_receipts.get(action.as_str()))
    {
        return receipt
            .runtime_review
            .as_ref()
            .and_then(|r| r.job_mcp.as_ref())
            .or(receipt.review.job_mcp.as_ref());
    }
    None
}

#[test]
fn unmigrated_persistent_runtime_has_actionable_start_error() {
    let mut record:CutexSessionRecord=serde_json::from_value(serde_json::json!({
        "cutex_session_id":"cutex.00000000-0000-4000-8000-000000000001",
        "codex_session_id":"00000000-0000-4000-8000-000000000001",
        "host_id":"test-host", "cwd":"/tmp", "runtime_backend":"cute_alden",
        "registration_class":"persistent", "display_name_hint":"old-worker",
        "created_at":"2026-09-21T00:00:00Z", "updated_at":"2026-09-21T00:00:00Z"
    })).unwrap();
    let error=require_migrated_runtime(&record).unwrap_err().to_string();
    assert!(error.starts_with("migration_required: old-worker"));
    assert!(error.len()<240);
    record.runtime_backend=crate::session::model::CutexSessionRuntimeBackend::Host;
    assert!(require_migrated_runtime(&record).is_ok());
}
