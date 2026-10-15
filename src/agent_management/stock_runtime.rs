//! Same-action stock execution journal. No implicit downgrade or new identity.
use super::*;
use super::runtime_package::zero_sha256;
use crate::launch::stock::{
    current_configuration, validate_native, StockBundle, StockConfiguration,
};
use crate::management::control_plane::HumanManagementPrincipal;
use crate::role_revision::CutexSessionId;
use crate::session::model::{CutexAppServerRuntimeBinding, CutexSessionRecord};
use crate::session::store::{
    load_cutex_session_store_from_path, session_record_at, with_locked_session_rows,
};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StockRuntimeReview {
    #[serde(default, skip_serializing_if = "RuntimeReviewDigestVersion::is_legacy")]
    pub digest_version: RuntimeReviewDigestVersion,
    pub subject: ExplicitLaunchSubject,
    pub contract: ExplicitLaunchContract,
    pub configuration: StockConfiguration,
    pub restart: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_mcp: Option<crate::launch::job_mcp::ReviewedJobMcp>,
    #[serde(
        default,
        skip_serializing_if = "crate::launch::stock::CanonicalBytePolicy::is_default"
    )]
    pub receiver_canonical_byte_limit: crate::launch::stock::CanonicalBytePolicy,
}

/// Version 1 is the historical typed full-record serialization. Never upgrade
/// an incoming/persisted review at execution. Version 2 excludes only telemetry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub enum RuntimeReviewDigestVersion {
    #[default]
    FullRecordV1,
    SemanticV2,
}
impl TryFrom<u8> for RuntimeReviewDigestVersion {
    type Error = &'static str;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::FullRecordV1),
            2 => Ok(Self::SemanticV2),
            _ => Err("unsupported runtime review digest version"),
        }
    }
}
impl From<RuntimeReviewDigestVersion> for u8 {
    fn from(value: RuntimeReviewDigestVersion) -> Self {
        match value {
            RuntimeReviewDigestVersion::FullRecordV1 => 1,
            RuntimeReviewDigestVersion::SemanticV2 => 2,
        }
    }
}
impl RuntimeReviewDigestVersion {
    fn is_legacy(&self) -> bool {
        *self == Self::FullRecordV1
    }
    pub(super) fn digest(
        &self,
        record: &CutexSessionRecord,
    ) -> anyhow::Result<crate::role_revision::Sha256> {
        match self {
            Self::FullRecordV1 => Ok(super::store::request_sha256(record)?),
            Self::SemanticV2 => {
                let mut value = serde_json::to_value(record)?;
                let object = value
                    .as_object_mut()
                    .expect("serialized session record object");
                object.remove("last_seen_at");
                object.remove("updated_at");
                Ok(super::store::request_sha256(&value)?)
            }
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StockRuntimeStage {
    Prepared,
    Claimed,
    Spawned,
    Ready,
    /// The launch's creator and child were proven gone and its claim was
    /// cleared automatically. Terminal: replaying the action fails clearly
    /// and a fresh start may proceed.
    Abandoned,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StockRuntimeReceipt {
    /// Actual launch cwd for this occurrence, independent of next-launch edits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_cwd: Option<String>,
    pub action_id: AgentActionId,
    pub review: StockRuntimeReview,
    pub stage: StockRuntimeStage,
    pub claim_id: String,
    pub runtime_agent_id: String,
    pub expected_generation: u64,
    pub binding: Option<CutexAppServerRuntimeBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication: Option<StockPublication>,
    pub error: Option<String>,
    pub updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StockPublication {
    pub path: std::path::PathBuf,
    pub device: u64,
    pub inode: u64,
}

pub trait StockRuntimeExecutor {
    /// Hold the exact kernel lease through child publication. On replay, a busy,
    /// missing or replaced lease cannot establish absence and must reject.
    fn publication(&mut self, receipt: &StockRuntimeReceipt) -> anyhow::Result<StockPublication>;
    /// Exact published occurrence only; false means still alive. Unknowns reject.
    fn published_owner_absent(&mut self, receipt: &StockRuntimeReceipt) -> anyhow::Result<bool>;
    fn stop(&mut self, record: &CutexSessionRecord) -> anyhow::Result<()>;
    fn spawn(
        &mut self,
        record: &CutexSessionRecord,
        bundle: &StockBundle,
        receipt: &StockRuntimeReceipt,
    ) -> anyhow::Result<CutexAppServerRuntimeBinding>;
    fn connect(
        &mut self,
        record: &CutexSessionRecord,
        receipt: &StockRuntimeReceipt,
    ) -> anyhow::Result<()>;
    /// Must only stop the child created by this invocation, never another owner.
    fn cleanup_owned(&mut self) -> anyhow::Result<()>;
    fn retain_owner(&mut self);
}

impl AgentManagementProvider {
    pub fn review_stock_runtime(
        &self,
        path: &Path,
        id: &CutexSessionId,
        restart: bool,
    ) -> anyhow::Result<StockRuntimeReview> {
        let _mutation = self.store().lock_mutations()?;
        self.review_stock_runtime_locked(path, id, restart)
    }

    /// Caller holds the provider mutation lock.
    pub(crate) fn review_stock_runtime_locked(
        &self,
        path: &Path,
        id: &CutexSessionId,
        restart: bool,
    ) -> anyhow::Result<StockRuntimeReview> {
        let state = self.store().snapshot()?;
        let project = super::archive::runtime_guard(&state, id)
            .map_err(|e| anyhow::anyhow!("explicit stock runtime/project guard: {e}"))?;
        let sessions = load_cutex_session_store_from_path(path)?;
        let record = sessions
            .sessions
            .get(id.as_str())
            .ok_or_else(|| anyhow::anyhow!("stock durable record missing"))?;
        active(record, id)?;
        anyhow::ensure!(
            record.app_server_launch_claim_id.is_none(),
            "stock launch unresolved; replay original action"
        );
        if !restart {
            anyhow::ensure!(
                !crate::session::archive::record_has_runtime_claim(record),
                "stock start requires offline; explicitly review restart"
            );
        }
        let contract = record
            .explicit_launch
            .clone()
            .ok_or_else(|| anyhow::anyhow!("explicit stock activation required"))?;
        sanitize_shared_projection_marker(&contract)?;
        StockBundle::load(&contract)?;
        validate_native(record, &sessions, &contract)?;
        let configuration = current_configuration(record)?;
        let formal_name = record
            .formal_agent_name
            .clone()
            .or_else(|| state.agents.get(id).map(|a| a.spec.name.clone()))
            .or_else(|| record.is_owned_session().then(|| crate::session::metadata::cutex_session_display_name(record)))
            .ok_or_else(|| anyhow::anyhow!("formal Agent name unavailable"))?;
        Ok(StockRuntimeReview {
            digest_version: RuntimeReviewDigestVersion::SemanticV2,
            subject: ExplicitLaunchSubject {
                cutex_session_id: id.clone(),
                formal_name,
                // Informational only: execute re-reads the record under its
                // locks instead of comparing digests.
                durable_sha256: zero_sha256()?,
                authority_sha256: zero_sha256()?,
                current_project_id: project,
                revision: record.revision,
                runtime_generation: record.runtime_generation,
            },
            contract,
            configuration,
            restart,
            job_mcp: None,
            receiver_canonical_byte_limit: Default::default(),
        })
    }

    pub fn execute_stock_runtime(
        &self,
        _principal: &HumanManagementPrincipal,
        path: &Path,
        action_id: &AgentActionId,
        review: &StockRuntimeReview,
        runtime: &mut dyn StockRuntimeExecutor,
    ) -> anyhow::Result<StockRuntimeReceipt> {
        let _execution = super::provider::provider_execution_lock()
            .lock()
            .map_err(|_| anyhow::anyhow!("stock execution lock unavailable"))?;
        let _mutation = self.store().lock_mutations()?;
        self.execute_stock_runtime_locked(path, action_id, review, runtime)
    }

    /// Caller holds execution then mutation locks. Only the root wrapper and
    /// sealed, provider-authorized bootstrap permit may enter this helper.
    pub(crate) fn execute_stock_runtime_locked(
        &self,
        path: &Path,
        action_id: &AgentActionId,
        review: &StockRuntimeReview,
        runtime: &mut dyn StockRuntimeExecutor,
    ) -> anyhow::Result<StockRuntimeReceipt> {
        self.execute_stock_runtime_inner(path, action_id, review, runtime, None, None)
    }

    /// Reviewed relaunch (owner approval 2026-10-14): an offline
    /// explicit-launch agent whose latest launch the owner reviewed and which
    /// ran Ready is started again for a delegation of its creator or its
    /// project's Director (`requester`; policy in `asks::wake::decide`). The
    /// launch reuses that receipt's review: the run is pinned to its exact
    /// contract and configuration (no fresh, broader review), rechecked here
    /// under the provider locks and again by every execution step. The new
    /// receipt's action id names the requester; the returned id is the
    /// owner's reviewed action it reused.
    pub fn execute_reviewed_relaunch(
        &self,
        path: &Path,
        id: &CutexSessionId,
        requester: &str,
        action_id: &AgentActionId,
        runtime: &mut dyn StockRuntimeExecutor,
    ) -> anyhow::Result<(StockRuntimeReceipt, AgentActionId)> {
        let _execution = super::provider::provider_execution_lock()
            .lock()
            .map_err(|_| anyhow::anyhow!("stock execution lock unavailable"))?;
        let _mutation = self.store().lock_mutations()?;
        let sessions = load_cutex_session_store_from_path(path)?;
        let record = sessions
            .sessions
            .get(id.as_str())
            .ok_or_else(|| anyhow::anyhow!("stock durable record missing"))?;
        anyhow::ensure!(
            !crate::session::archive::record_has_runtime_claim(record),
            "reviewed relaunch requires an offline runtime"
        );
        let prior = reviewed_relaunch_receipt(&sessions, record).ok_or_else(|| {
            anyhow::anyhow!(
                "owner_start_required: no reviewed launch of {} matches its current configuration; the owner has to start it",
                id.as_str()
            )
        })?;
        let mut review = prior.review.clone();
        review.restart = false;
        // Name the owner's original action, also for a relaunch on a relaunch.
        let reviewed_action = original_owner_reviewed_action(&sessions, prior).clone();
        eprintln!(
            "agent-management: reviewed relaunch of {} for {requester} (action {}) on the owner's reviewed action {}",
            id.as_str(),
            action_id.as_str(),
            reviewed_action.as_str()
        );
        let pinned = review.clone();
        let receipt =
            self.execute_stock_runtime_inner(path, action_id, &review, runtime, None, Some(&pinned))?;
        Ok((receipt, reviewed_action))
    }

    #[cfg(target_os = "linux")]
    pub(super) fn execute_stock_runtime_maintenance_locked(
        &self,
        path: &Path,
        action_id: &AgentActionId,
        review: &StockRuntimeReview,
        runtime: &mut dyn StockRuntimeExecutor,
        permit: &super::migration::MaintenancePermit<'_>,
    ) -> anyhow::Result<StockRuntimeReceipt> {
        self.execute_stock_runtime_inner(path, action_id, review, runtime, Some(permit), None)
    }

    fn execute_stock_runtime_inner(
        &self,
        path: &Path,
        action_id: &AgentActionId,
        review: &StockRuntimeReview,
        runtime: &mut dyn StockRuntimeExecutor,
        #[cfg(target_os = "linux")] maintenance: Option<&super::migration::MaintenancePermit<'_>>,
        #[cfg(not(target_os = "linux"))] _maintenance: Option<&()>,
        pinned: Option<&StockRuntimeReview>,
    ) -> anyhow::Result<StockRuntimeReceipt> {
        // r219: no Task Service admission fence or Director seat snapshot.
        // Prepared below records the transition before external work;
        // deliveries defer that owner.
        let mut operation = || -> anyhow::Result<_> {
            review.configuration.validate_job_requirement(review.job_mcp.is_some())?;
            let id = &review.subject.cutex_session_id;
            let sessions = load_cutex_session_store_from_path(path)?;
            // Ordinary starts are not fenced on a digest of the reviewed record:
            // a fresh action is rebased onto the record as it is now (under the
            // execution lock), and later steps compare against that.
            #[cfg(target_os = "linux")]
            let rebase = maintenance.is_none();
            #[cfg(not(target_os = "linux"))]
            let rebase = true;
            let mut receipt =
                if let Some(previous) = crate::session::receipt_retention::explicit_launch_receipt(&sessions, action_id.as_str())? {
                    let ExplicitLaunchActionReceipt::Runtime(previous) = previous else {
                        anyhow::bail!("explicit_launch_action_conflict")
                    };
                    anyhow::ensure!(
                        if rebase {
                            previous.review.subject.cutex_session_id == review.subject.cutex_session_id
                                && previous.review.restart == review.restart
                        } else {
                            &previous.review == review
                        },
                        "explicit_launch_action_conflict"
                    );
                    if previous.stage == StockRuntimeStage::Ready {
                        return Ok(previous.clone());
                    }
                    anyhow::ensure!(
                        previous.stage != StockRuntimeStage::Abandoned,
                        "launch action was abandoned after its owner exited; start the agent again"
                    );
                    previous.clone()
                } else {
                    let record = sessions
                        .sessions
                        .get(id.as_str())
                        .ok_or_else(|| anyhow::anyhow!("stock durable record missing"))?;
                    let review = if rebase {
                        let mut current = review.clone();
                        current.subject.revision = record.revision;
                        current.subject.runtime_generation = record.runtime_generation;
                        if let Some(contract) = &record.explicit_launch {
                            current.contract = contract.clone();
                        }
                        current.configuration = current_configuration(record)?;
                        current
                    } else {
                        anyhow::ensure!(
                            review.digest_version.digest(record)? == review.subject.durable_sha256
                                && record.revision == review.subject.revision
                                && record.runtime_generation == review.subject.runtime_generation,
                            "stock confirmation stale"
                        );
                        review.clone()
                    };
                    StockRuntimeReceipt {
                        launch_cwd: Some(crate::session::service::cutex_session_launch_cwd(record).into()),
                        action_id: action_id.clone(),
                        review,
                        stage: StockRuntimeStage::Prepared,
                        claim_id: uuid::Uuid::new_v4().to_string(),
                        runtime_agent_id: format!("stock.{}", uuid::Uuid::new_v4()),
                        expected_generation: record
                            .runtime_generation
                            .checked_add(1)
                            .filter(|g| *g <= crate::management::v2::model::MAX_SAFE_SEQUENCE)
                            .ok_or_else(|| anyhow::anyhow!("runtime generation exhausted"))?,
                        binding: None,
                        publication: None,
                        error: None,
                        updated_at: chrono::Utc::now().to_rfc3339(),
                    }
                };
            if let Some(pinned) = pinned {
                // Reviewed relaunch: the rebased review must be the owner's
                // reviewed one, never a newer configuration.
                anyhow::ensure!(
                    receipt.review.contract == pinned.contract
                        && receipt.review.configuration == pinned.configuration
                        && receipt.review.job_mcp.is_none()
                        && pinned.job_mcp.is_none(),
                    "stock configuration changed since the owner's reviewed launch; the owner has to start it"
                );
            }
            let rebased_review = receipt.review.clone();
            let review = if rebase { &rebased_review } else { review };
            let state = self.store().snapshot()?;
            #[cfg(target_os="linux")]
            let maintenance_validated=if let Some(permit)=maintenance {
                permit.validate(path,action_id,review,&state)?;
                true
            } else {false};
            #[cfg(not(target_os="linux"))]
            let maintenance_validated=false;
            if !maintenance_validated {
                super::archive::runtime_guard(&state, id)?;
            }
            let record = sessions
                .sessions
                .get(id.as_str())
                .ok_or_else(|| anyhow::anyhow!("stock durable record missing"))?;
            active(record, id)?;
            anyhow::ensure!(
                record.explicit_launch.as_ref() == Some(&review.contract),
                "stock launch requirement changed"
            );
            anyhow::ensure!(
                current_configuration(record)? == review.configuration,
                "stock configuration changed; fresh review required"
            );
            sanitize_shared_projection_marker(&review.contract)?;
            let bundle = StockBundle::load(&review.contract)?;
            if let Some(job) = &review.job_mcp {
                job.validate(&bundle)?;
            } else {
                anyhow::ensure!(!sessions.explicit_launch_receipts.values().any(|r| matches!(r,
                    ExplicitLaunchActionReceipt::Runtime(r) if r.review.subject.cutex_session_id == *id && r.review.job_mcp.is_some())),
                    "previously reviewed Job MCP cannot be silently omitted; provide an explicit descriptor");
            }
            anyhow::ensure!(bundle.common_ingress() || review.receiver_canonical_byte_limit.is_default(),
                "unchanged stock is registration-only; receiver ingress policy unsupported");
            validate_native(record, &sessions, &review.contract)?;
            if receipt.stage == StockRuntimeStage::Spawned
                && record.runtime_generation == review.subject.runtime_generation
                && runtime.published_owner_absent(&receipt)?
            {
                // A committed gate may lose its creator before release. Same
                // lease + exact process absence permits only this unregistered
                // occurrence to return to Claimed under the original action.
                with_locked_session_rows(path, |rows| {
                    let current = rows.session_mut(id.as_str()).ok_or_else(|| anyhow::anyhow!("stock record missing"))?;
                    anyhow::ensure!(current.app_server_launch_claim_id.as_deref() == Some(&receipt.claim_id)
                        && current.app_server_runtime == receipt.binding
                        && current.runtime_generation == review.subject.runtime_generation
                        && current.revision == review.subject.revision
                        && current.explicit_launch.as_ref() == Some(&review.contract),
                        "published recovery occurrence changed");
                    current.app_server_runtime = None;
                    current.runtime_pid = None;
                    receipt.binding = None;
                    receipt.stage = StockRuntimeStage::Claimed;
                    rows.insert_receipt::<crate::session::store::ExplicitLaunchReceipts>(action_id.to_string(), ExplicitLaunchActionReceipt::Runtime(receipt.clone()));
                    rows.commit().map(drop)
                })?;
            }
            if receipt.stage == StockRuntimeStage::Prepared {
                if !rebase {
                    anyhow::ensure!(
                        review.digest_version.digest(record)? == review.subject.durable_sha256,
                        "stock prepared outcome uncertain; no repeated destructive stop"
                    );
                }
                #[cfg(target_os="linux")]
                if let Some(permit) = maintenance {
                    permit.validate(path,action_id,review,&state)?;
                }
                save_receipt(path, &receipt)?;
                // Reobserve after potentially expensive evidence validation.
                // Compare to the ORIGINAL review, never mint a replacement CAS.
                if let Some(job) = &review.job_mcp { job.validate(&bundle)?; }
                let before_execution = session_record_at(path, id.as_str())?
                    .ok_or_else(|| anyhow::anyhow!("stock durable record missing"))?;
                anyhow::ensure!((rebase || review.digest_version.digest(&before_execution)? == review.subject.durable_sha256)
                    && before_execution.revision == review.subject.revision
                    && before_execution.runtime_generation == review.subject.runtime_generation
                    && current_configuration(&before_execution)? == review.configuration,
                    "stock confirmation stale before execution; owner unchanged");
                if review.restart {
                    runtime.stop(&before_execution)?;
                }
                receipt.publication = Some(runtime.publication(&receipt)?);
                with_locked_session_rows(path, |rows| {
                    if let Some(job) = &review.job_mcp { job.validate(&bundle)?; }
                    let current = rows
                        .session_mut(id.as_str())
                        .ok_or_else(|| anyhow::anyhow!("stock durable record missing"))?;
                    active(current, id)?;
                    anyhow::ensure!(
                        current.revision == review.subject.revision,
                        "stock durable configuration changed after stop; stopped-not-started"
                    );
                    anyhow::ensure!(
                        current.explicit_launch.as_ref() == Some(&review.contract)
                            && current_configuration(current)? == review.configuration,
                        "stock configuration changed after stop; stopped-not-started"
                    );
                    anyhow::ensure!(
                        !crate::session::archive::record_has_runtime_claim(current),
                        "stock owner remains; no duplicate launch"
                    );
                    anyhow::ensure!(
                        current.runtime_generation == review.subject.runtime_generation,
                        "stock runtime generation changed after stop"
                    );
                    current.app_server_launch_claim_id = Some(receipt.claim_id.clone());
                    current.updated_at = chrono::Utc::now().to_rfc3339();
                    receipt.stage = StockRuntimeStage::Claimed;
                    rows.insert_receipt::<crate::session::store::ExplicitLaunchReceipts>(
                        action_id.to_string(),
                        ExplicitLaunchActionReceipt::Runtime(receipt.clone()),
                    );
                    rows.commit().map(drop)
                })?;
            }
            if receipt.stage == StockRuntimeStage::Claimed {
                anyhow::ensure!(receipt.publication.is_some(), "publication_missing: legacy uncertain claim has no absence proof; no automatic retry");
                runtime.publication(&receipt)?;
                let current = session_record_at(path, id.as_str())?
                    .ok_or_else(|| anyhow::anyhow!("stock durable record missing"))?;
                anyhow::ensure!(current.app_server_launch_claim_id.as_deref() == Some(&receipt.claim_id)
                    && current.app_server_runtime.is_none() && current.runtime_pid.is_none()
                    && current.runtime_generation == review.subject.runtime_generation
                    && current.revision == review.subject.revision,
                    "publication conflict: claim/configuration or runtime changed; no spawn");
                let binding = match runtime.spawn(&current, &bundle, &receipt) {
                    Ok(binding) => binding,
                    Err(error) => {
                        receipt.error = Some(format!("spawn failed: {error:#}; claim retained"));
                        save_receipt(path, &receipt)?;
                        return Err(error);
                    }
                };
                receipt.binding = Some(binding.clone());
                receipt.stage = StockRuntimeStage::Spawned;
                #[cfg(feature = "stock-launch-test-hook")]
                if std::env::var("CUTEX_STOCK_TEST_CREATOR_DEATH_ACTION").ok().as_deref() == Some(action_id.as_str()) {
                    eprintln!("private precommit creator death; owned child PID {}", binding.pid);
                    unsafe { libc::_exit(86); }
                }
                let persisted = with_locked_session_rows(path, |rows| {
                    if let Some(job) = &review.job_mcp { job.validate(&bundle)?; }
                    #[cfg(feature = "stock-launch-test-hook")]
                    { static FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                    if std::env::var("CUTEX_STOCK_TEST_COMMIT_FAIL_ACTION")
                        .ok()
                        .as_deref()
                        == Some(action_id.as_str()) && !FAILED.swap(true, std::sync::atomic::Ordering::SeqCst)
                    {
                        anyhow::bail!(
                            "private injected precommit failure; owned child PID {}",
                            binding.pid
                        );
                    }
                    }
                    let current = rows
                        .session_mut(id.as_str())
                        .ok_or_else(|| anyhow::anyhow!("stock durable record missing"))?;
                    anyhow::ensure!(
                        current.revision == review.subject.revision,
                        "stock durable configuration changed during spawn"
                    );
                    anyhow::ensure!(
                        current.app_server_launch_claim_id.as_deref() == Some(&receipt.claim_id)
                            && current.explicit_launch.as_ref() == Some(&review.contract)
                            && current_configuration(current)? == review.configuration,
                        "stock claim/configuration changed during spawn"
                    );
                    current.runtime_pid = Some(binding.pid);
                    current.app_server_runtime = Some(binding.clone());
                    rows.insert_receipt::<crate::session::store::ExplicitLaunchReceipts>(
                        action_id.to_string(),
                        ExplicitLaunchActionReceipt::Runtime(receipt.clone()),
                    );
                    rows.commit().map(drop)
                });
                if let Err(error) = persisted {
                    runtime.cleanup_owned()?;
                    return Err(
                        error.context("spawned child stopped; commit uncertain, claim retained")
                    );
                }
                #[cfg(feature = "stock-launch-test-hook")]
                if std::env::var("CUTEX_STOCK_TEST_PUBLISHED_DEATH_ACTION").ok().as_deref() == Some(action_id.as_str()) {
                    eprintln!("private published creator death; owned child PID {}", binding.pid);
                    unsafe { libc::_exit(87); }
                }
            }
            anyhow::ensure!(
                receipt.stage == StockRuntimeStage::Spawned,
                "stock claim has no committed child; outcome uncertain, no duplicate spawn"
            );
            let current = session_record_at(path, id.as_str())?
                .ok_or_else(|| anyhow::anyhow!("stock durable record missing"))?;
            anyhow::ensure!(
                current.app_server_launch_claim_id.as_deref() == Some(&receipt.claim_id)
                    && current.app_server_runtime == receipt.binding,
                "stock owner changed before registration"
            );
            // A lost response reuses this exact binding/runtime/generation; no spawn.
            if let Err(error) = runtime.connect(&current, &receipt) {
                if rebase {
                    // Ordinary start: never leave a live, unready child holding
                    // the claim (every later start would see launch_in_progress).
                    return abandon_unready_spawn(path, id, receipt, runtime, error);
                }
                runtime.retain_owner();
                receipt.error = Some(format!(
                    "readiness incomplete: {error:#}; replay exact action"
                ));
                save_receipt(path, &receipt)?;
                return Ok(receipt);
            }
            runtime.retain_owner();
            with_locked_session_rows(path, |rows| {
                if let Some(job) = &review.job_mcp { job.validate(&bundle)?; }
                let current = rows
                    .session_mut(id.as_str())
                    .ok_or_else(|| anyhow::anyhow!("stock durable record missing"))?;
                anyhow::ensure!(
                    current.revision == review.subject.revision,
                    "stock durable configuration changed during readiness; claim retained"
                );
                anyhow::ensure!(
                    current.app_server_launch_claim_id.as_deref() == Some(&receipt.claim_id)
                        && current.app_server_runtime == receipt.binding
                        && current.current_runtime_agent_id.as_deref()
                            == Some(&receipt.runtime_agent_id)
                        && current.runtime_generation == receipt.expected_generation,
                    "stock registration readback mismatch; claim retained"
                );
                anyhow::ensure!(
                    current.runtime_pid == receipt.binding.as_ref().map(|b| b.pid),
                    "stock PID readback mismatch; claim retained"
                );
                anyhow::ensure!(
                    current_configuration(current)? == review.configuration
                        && current.explicit_launch.as_ref() == Some(&review.contract),
                    "stock config changed during readiness; claim retained"
                );
                current.app_server_launch_claim_id = None;
                current.updated_at = chrono::Utc::now().to_rfc3339();
                receipt.stage = StockRuntimeStage::Ready;
                receipt.error = None;
                receipt.updated_at = chrono::Utc::now().to_rfc3339();
                rows.insert_receipt::<crate::session::store::ExplicitLaunchReceipts>(
                    action_id.to_string(),
                    ExplicitLaunchActionReceipt::Runtime(receipt.clone()),
                );
                rows.commit().map(drop)
            })?;
            Ok(receipt)
        };
        operation()
    }
}
/// Readiness failed after the child was spawned and its binding committed
/// (2026-10-13: native `thread/resume` refused an unreadable history). Stop
/// that exact occurrence (the executor's stop verifies PID + start time and
/// clears the claim, binding and runtime fields) and mark the action
/// Abandoned, so the next start begins clean. When the stop itself fails the
/// child is retained and the receipt keeps the error; the next start's claim
/// cleanup ([`AgentManagementProvider::clear_dead_launch_claim`]) retries it.
pub(super) fn abandon_unready_spawn(
    path: &Path,
    id: &CutexSessionId,
    mut receipt: StockRuntimeReceipt,
    runtime: &mut dyn StockRuntimeExecutor,
    error: anyhow::Error,
) -> anyhow::Result<StockRuntimeReceipt> {
    let detail = format!("{error:#}");
    let stopped = (|| -> anyhow::Result<()> {
        let current = session_record_at(path, id.as_str())?
            .ok_or_else(|| anyhow::anyhow!("stock durable record missing"))?;
        anyhow::ensure!(
            current.app_server_launch_claim_id.as_deref() == Some(&receipt.claim_id)
                && current.app_server_runtime == receipt.binding,
            "stock owner changed after the readiness failure; nothing stopped"
        );
        runtime.stop(&current)
    })();
    if let Err(stop_error) = stopped {
        runtime.retain_owner();
        receipt.error = Some(format!(
            "readiness incomplete: {detail}; stopping the spawned runtime failed: {stop_error:#}; the next start retries the stop"
        ));
        receipt.updated_at = chrono::Utc::now().to_rfc3339();
        save_receipt(path, &receipt)?;
        return Ok(receipt);
    }
    // The owned child is stopped: reap it (never signals another process).
    let _ = runtime.cleanup_owned();
    with_locked_session_rows(path, |rows| {
        if let Some(current) = rows.session_mut(id.as_str()) {
            if current.app_server_launch_claim_id.as_deref() == Some(&receipt.claim_id) {
                current.app_server_launch_claim_id = None;
                current.updated_at = chrono::Utc::now().to_rfc3339();
            }
        }
        receipt.stage = StockRuntimeStage::Abandoned;
        receipt.error = Some(format!(
            "readiness failed: {detail}; the spawned runtime was stopped and its launch claim released; start the agent again"
        ));
        receipt.updated_at = chrono::Utc::now().to_rfc3339();
        rows.insert_receipt::<crate::session::store::ExplicitLaunchReceipts>(
            receipt.action_id.to_string(),
            ExplicitLaunchActionReceipt::Runtime(receipt.clone()),
        );
        rows.commit().map(drop)
    })?;
    Ok(receipt)
}

/// The owner's reviewed launch an offline explicit-launch agent may be
/// started again on (`reviewed_relaunch`, owner approval 2026-10-14):
/// the latest runtime receipt of the agent (highest `expected_generation`,
/// then `updated_at`) is `Ready` without error and without a Job MCP review,
/// its contract is the agent's current `explicit_launch` and its
/// configuration equals [`current_configuration`] now (the same comparison
/// every execution step makes). `None` otherwise: never launched, a newer
/// failed / abandoned / pending launch, a changed profile / model /
/// permission / contract, an archived record, an open launch claim, or a
/// receipt removed by session store retention
/// (`crate::session::receipt_retention`; the newest Ready receipt is
/// normally kept, older ones are pruned after
/// `sessionStore.receiptRetentionDays`, default 7). Then only the owner can
/// start it.
pub fn reviewed_relaunch_receipt<'a>(
    store: &'a crate::session::model::CutexSessionStore,
    record: &CutexSessionRecord,
) -> Option<&'a StockRuntimeReceipt> {
    reviewed_relaunch_receipt_with(store, record, current_configuration)
}

/// Action-id prefix of a reviewed relaunch (the only explicit-launch start
/// an agent, not the owner, causes): `asks-wake:<requester>:<uuid>`.
pub const REVIEWED_RELAUNCH_ACTION_PREFIX: &str = "asks-wake:";

/// Whether a runtime launch receipt was started by the owner (every start
/// except a reviewed relaunch).
pub fn owner_initiated_runtime_receipt(receipt: &StockRuntimeReceipt) -> bool {
    !receipt.action_id.as_str().starts_with(REVIEWED_RELAUNCH_ACTION_PREFIX)
}

/// The agent's latest runtime launch receipt (highest `expected_generation`,
/// then `updated_at`), whatever its stage.
pub fn latest_runtime_receipt<'a>(
    store: &'a crate::session::model::CutexSessionStore,
    cutex_session_id: &str,
) -> Option<&'a StockRuntimeReceipt> {
    store
        .explicit_launch_receipts
        .values()
        .filter_map(|receipt| match receipt {
            ExplicitLaunchActionReceipt::Runtime(receipt)
                if receipt.review.subject.cutex_session_id.as_str() == cutex_session_id =>
            {
                Some(receipt)
            }
            _ => None,
        })
        .max_by_key(|receipt| {
            (
                receipt.expected_generation,
                chrono::DateTime::parse_from_rfc3339(&receipt.updated_at).ok(),
            )
        })
}

/// The owner's action a reviewed relaunch on `prior` rests on: `prior`
/// itself when the owner started it; for a relaunch on an earlier relaunch,
/// the latest owner-started Ready receipt of the agent with the same
/// contract and configuration (a relaunch copies that review). Falls back to
/// `prior` when retention pruned it.
pub fn original_owner_reviewed_action<'a>(
    store: &'a crate::session::model::CutexSessionStore,
    prior: &'a StockRuntimeReceipt,
) -> &'a AgentActionId {
    if owner_initiated_runtime_receipt(prior) {
        return &prior.action_id;
    }
    store
        .explicit_launch_receipts
        .values()
        .filter_map(|receipt| match receipt {
            ExplicitLaunchActionReceipt::Runtime(receipt)
                if receipt.review.subject.cutex_session_id == prior.review.subject.cutex_session_id
                    && owner_initiated_runtime_receipt(receipt)
                    && receipt.stage == StockRuntimeStage::Ready
                    && receipt.error.is_none()
                    && receipt.review.contract == prior.review.contract
                    && receipt.review.configuration == prior.review.configuration =>
            {
                Some(receipt)
            }
            _ => None,
        })
        .max_by_key(|receipt| {
            (
                receipt.expected_generation,
                chrono::DateTime::parse_from_rfc3339(&receipt.updated_at).ok(),
            )
        })
        .map_or(&prior.action_id, |receipt| &receipt.action_id)
}

pub(crate) fn reviewed_relaunch_receipt_with<'a>(
    store: &'a crate::session::model::CutexSessionStore,
    record: &CutexSessionRecord,
    configuration: impl Fn(&CutexSessionRecord) -> anyhow::Result<StockConfiguration>,
) -> Option<&'a StockRuntimeReceipt> {
    let contract = record.explicit_launch.as_ref()?;
    if record.is_retired() || record.app_server_launch_claim_id.is_some() {
        return None;
    }
    let latest = latest_runtime_receipt(store, &record.cutex_session_id)?;
    let eligible = latest.stage == StockRuntimeStage::Ready
        && latest.error.is_none()
        && latest.review.job_mcp.is_none()
        && &latest.review.contract == contract
        && configuration(record).is_ok_and(|current| current == latest.review.configuration);
    eligible.then_some(latest)
}

fn save_receipt(path: &Path, receipt: &StockRuntimeReceipt) -> anyhow::Result<()> {
    crate::session::store::update_receipt_row::<crate::session::store::ExplicitLaunchReceipts>(
        path,
        receipt.action_id.as_str(),
        Some(ExplicitLaunchActionReceipt::Runtime(receipt.clone())),
    )
    .map(drop)
}

/// Native reads config.toml at process startup, and 0.156.1 reports the
/// legacy Cutex projection marker as an unrecognized user setting. The agent
/// config is user-editable state whose manifest hash is historical evidence,
/// never a launch gate, so remove the marker in place before validation.
/// Unknown projection versions fail closed; a marker-less config is untouched.
pub(super) fn sanitize_shared_projection_marker(
    contract: &ExplicitLaunchContract,
) -> anyhow::Result<()> {
    contract.validate()?;
    let bundle: StockBundle = serde_json::from_slice(&std::fs::read(&contract.bundle_manifest)?)
        .map_err(|error| anyhow::anyhow!("invalid stock bundle manifest: {error}"))?;
    let config = &bundle.shared_config.path;
    anyhow::ensure!(
        config.canonicalize()? == contract.native_home.canonicalize()?.join("config.toml"),
        "shared config must live in the native home"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = std::fs::metadata(&contract.native_home)?.permissions();
        anyhow::ensure!(
            permissions.mode() & 0o077 == 0,
            "native home must remain private before projection update"
        );
    }
    let raw = std::fs::read_to_string(config)?;
    if let Some(projected) = crate::launch::stock::remove_projection_marker(&raw)? {
        crate::config::atomic::write_private_bytes_atomic(config, projected.as_bytes())?;
    }
    Ok(())
}

fn active(record: &CutexSessionRecord, id: &CutexSessionId) -> anyhow::Result<()> {
    anyhow::ensure!(
        record.cutex_session_id == id.as_str()
            && !record.is_retired()
            && (record.is_owned_session() || (record.agent_enabled
            && record.registration_class == crate::agent_bus::model::AgentRegistrationClass::Persistent)),
        "active Cutex agent or owned session runtime required"
    );
    Ok(())
}
/// Called by the owned-child adapter only after exact process stop proof.
pub fn commit_stock_runtime_stop(path: &Path, expected: &CutexSessionRecord) -> anyhow::Result<()> {
    with_locked_session_rows(path, |rows| {
        let current = rows
            .session(&expected.cutex_session_id)
            .ok_or_else(|| anyhow::anyhow!("stock record missing after stop"))?;
        anyhow::ensure!(
            current.app_server_runtime == expected.app_server_runtime
                && current.runtime_generation == expected.runtime_generation
                && current.current_runtime_agent_id == expected.current_runtime_agent_id
                && current.codex_session_id == expected.codex_session_id,
            "stock stopped, but current occurrence changed; no clear"
        );
        if expected.app_server_runtime.is_some() {
            rows.insert_receipt::<crate::session::store::NativeStopReceipts>(expected.cutex_session_id.clone(), crate::session::model::NativeStopReceipt {
                native_id: expected.codex_session_id.clone(), generation: expected.runtime_generation,
                binding: expected.app_server_runtime.clone(),
            });
        }
        if let Some(record) = rows.session_mut(&expected.cutex_session_id) {
            crate::session::runtime_reconciliation::clear_runtime_fields(record, false);
        }
        rows.commit().map(drop)
    })
}

#[cfg(test)]
mod review_digest_tests {
    use super::*;
    use serde_json::json;

    fn record() -> CutexSessionRecord {
        CutexSessionRecord::new(
            "cutex.digest-test".into(),
            Some(uuid::Uuid::new_v4().to_string()),
            "private".into(),
            "/private".into(),
            Some("alpha".into()),
        )
        .unwrap()
    }
    fn review(record: &CutexSessionRecord) -> StockRuntimeReview {
        serde_json::from_value(json!({
            "subject":{"cutex_session_id":record.cutex_session_id,"formal_name":"Explicit formal name","durable_sha256":RuntimeReviewDigestVersion::FullRecordV1.digest(record).unwrap(),"authority_sha256":"a".repeat(64),"current_project_id":null,"revision":record.revision,"runtime_generation":record.runtime_generation},
            "contract":{"version":2,"native_id":record.codex_session_id,"native_home":"/private","bundle_manifest":"/private/manifest","bundle_sha256":"b".repeat(64)},
            "configuration":{"profile_name":"alpha","profile_id":"private-profile","inherited":false,"profile_sha256":"c".repeat(64),"account_sha256":"d".repeat(64),"model":"private-model","reasoning":null,"model_provider":"private","provider":{"name":"private","base_url":"http://127.0.0.1:1/v1","wire_api":"responses","requires_openai_auth":false,"supports_websockets":false},"sandbox":"read-only","approval":"on-request"},
            "restart":true
        })).unwrap()
    }

    #[test]
    fn ordinary_owner_admission_does_not_require_managed_identity() {
        let mut record = record();
        let id = CutexSessionId::new(record.cutex_session_id.clone()).unwrap();
        assert!(active(&record, &id).is_err());
        record.explicit_launch = Some(review(&record).contract);
        record.agent_enabled = false;
        record.formal_agent_name = None;
        record.registration_class = crate::agent_bus::model::AgentRegistrationClass::LocalOnly;
        active(&record, &id).unwrap();
        assert!(!crate::session::metadata::cutex_session_is_managed(&record));
        record.formal_agent_name = Some("unexpected identity".into());
        assert!(active(&record, &id).is_err());
    }

    #[test]
    fn runtime_review_digest_v2_excludes_only_two_observations() {
        let base = record();
        let old = RuntimeReviewDigestVersion::FullRecordV1
            .digest(&base)
            .unwrap();
        assert_eq!(old, super::super::store::request_sha256(&base).unwrap());
        let expected = RuntimeReviewDigestVersion::SemanticV2
            .digest(&base)
            .unwrap();
        let mut refreshed = base.clone();
        refreshed.last_seen_at = Some("2030-01-01T00:00:00Z".into());
        refreshed.updated_at = "2030-01-01T00:00:00Z".into();
        assert_eq!(
            RuntimeReviewDigestVersion::SemanticV2
                .digest(&refreshed)
                .unwrap(),
            expected
        );
        assert_ne!(
            RuntimeReviewDigestVersion::FullRecordV1
                .digest(&refreshed)
                .unwrap(),
            old
        );
        // Exact serialized input contains all other present fields, including
        // future fields: no handwritten allowlist can silently omit one.
        let mut semantic = serde_json::to_value(&base).unwrap();
        semantic.as_object_mut().unwrap().remove("last_seen_at");
        semantic.as_object_mut().unwrap().remove("updated_at");
        assert_eq!(
            super::super::store::request_sha256(&semantic).unwrap(),
            expected
        );
        for (field, value) in [
            ("revision", json!(base.revision + 1)),
            ("runtime_generation", json!(1)),
            ("cutex_session_id", json!("cutex.other")),
            ("codex_session_id", json!(uuid::Uuid::new_v4().to_string())),
            ("profile", json!("beta")),
            ("formal_agent_name", json!("different formal name")),
            ("managed_cwd", json!("/different")),
            ("model_defaults", json!("different-model")),
            ("permission_defaults", json!("full-access")),
            ("approval_policy", json!("never")),
            ("app_server_launch_claim_id", json!("different-claim")),
            ("runtime_pid", json!(42)),
            ("current_runtime_agent_id", json!("stock.other")),
            ("lifecycle", json!("retired")),
            ("agent_enabled", json!(!base.agent_enabled)),
            ("agent_groups", json!(["different-group"])),
            ("default_cli_args", json!(["--different"])),
            (
                "app_server_runtime",
                json!({"transport":"unix_socket","endpoint":"unix:///private/s","pid":42,"runtime_dir":"/private","auth_token_path":null,"diagnostic_journal_path":"/private/j","schema_version":"private","schema_sha256":"e".repeat(64),"started_at":"2030-01-01T00:00:00Z"}),
            ),
        ] {
            let mut value_record = serde_json::to_value(&base).unwrap();
            value_record[field] = value;
            let changed: CutexSessionRecord = serde_json::from_value(value_record).unwrap();
            assert_ne!(
                RuntimeReviewDigestVersion::SemanticV2
                    .digest(&changed)
                    .unwrap(),
                expected,
                "{field}"
            );
        }
    }

    #[test]
    fn runtime_review_version_legacy_wire_and_unknown_versions() {
        let legacy = review(&record());
        assert_eq!(
            legacy.digest_version,
            RuntimeReviewDigestVersion::FullRecordV1
        );
        let bytes = serde_json::to_vec(&legacy).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("digest_version"));
        assert_eq!(
            serde_json::to_vec(&serde_json::from_slice::<StockRuntimeReview>(&bytes).unwrap())
                .unwrap(),
            bytes
        );
        for version in [0, 3, 255, 256] {
            let mut raw = serde_json::to_value(&legacy).unwrap();
            raw["digest_version"] = json!(version);
            assert!(serde_json::from_value::<StockRuntimeReview>(raw).is_err());
        }
        let mut new = legacy.clone();
        new.digest_version = RuntimeReviewDigestVersion::SemanticV2;
        assert_eq!(serde_json::to_value(&new).unwrap()["digest_version"], 2);
        assert_ne!(new, legacy);
    }

    #[test]
    fn persisted_prepared_runtime_receipt_defers_native_transition() {
        // r219: the Task Service admission fence is gone. The persisted
        // Prepared receipt alone defers old-owner delivery until Ready.
        let root = std::env::temp_dir().join(format!("runtime-admission-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("sessions.json");
        let mut record = record();
        let review = review(&record);
        record.explicit_launch = Some(review.contract.clone());
        let action = AgentActionId::new("gated-connect").unwrap();
        let receipt = StockRuntimeReceipt {
            launch_cwd: Some(record.cwd.clone()),
            action_id: action.clone(), review, stage: StockRuntimeStage::Prepared,
            claim_id: "gated-claim".into(), runtime_agent_id: "stock.gated".into(),
            expected_generation: 1, binding: None, publication: None, error: None,
            updated_at: "2030-01-01T00:00:00Z".into(),
        };
        let mut store = crate::session::model::CutexSessionStore::default();
        store.sessions.insert(record.cutex_session_id.clone(), record.clone());
        assert!(!native_runtime_transition_pending(&store, &record));
        crate::session::store::save_cutex_session_store_to_path(&path, &store).unwrap();
        save_receipt(&path, &receipt).unwrap();
        let pending = load_cutex_session_store_from_path(&path).unwrap();
        assert!(
            native_runtime_transition_pending(&pending, &record),
            "persisted intent must defer old-owner delivery"
        );
        let mut ready = pending;
        if let ExplicitLaunchActionReceipt::Runtime(receipt) = ready.explicit_launch_receipts.get_mut(action.as_str()).unwrap() {
            receipt.stage = StockRuntimeStage::Ready;
        }
        assert!(!native_runtime_transition_pending(&ready, &record), "Ready releases delivery guard");
        // A still-held claim must defer even if a historical Ready receipt exists.
        let mut claimed = record.clone();
        claimed.app_server_launch_claim_id = Some("gated-claim".into());
        assert!(native_runtime_transition_pending(&ready, &claimed));
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Owner approval 2026-10-14: the reviewed-relaunch evidence is the
    /// agent's latest runtime receipt, Ready, error- and Job-MCP-free, with
    /// the current contract and configuration.
    #[test]
    fn reviewed_relaunch_needs_the_latest_matching_ready_receipt() {
        let mut record = record();
        let reviewed = review(&record);
        record.explicit_launch = Some(reviewed.contract.clone());
        let same = |_: &CutexSessionRecord| Ok(reviewed.configuration.clone());
        let receipt = |action: &str, generation: u64, stage: StockRuntimeStage, at: &str| StockRuntimeReceipt {
            launch_cwd: None,
            action_id: AgentActionId::new(action).unwrap(),
            review: reviewed.clone(),
            stage,
            claim_id: format!("{action}-claim"),
            runtime_agent_id: format!("stock.{action}"),
            expected_generation: generation,
            binding: None,
            publication: None,
            error: None,
            updated_at: at.into(),
        };
        let with = |receipts: Vec<StockRuntimeReceipt>| {
            let mut store = crate::session::model::CutexSessionStore::default();
            store.sessions.insert(record.cutex_session_id.clone(), record.clone());
            for receipt in receipts {
                store.explicit_launch_receipts.insert(
                    receipt.action_id.to_string(),
                    ExplicitLaunchActionReceipt::Runtime(receipt),
                );
            }
            store
        };
        let ready = receipt("owner-start", 1, StockRuntimeStage::Ready, "2030-01-01T00:00:00Z");
        // Never launched, or the receipt was pruned by retention.
        assert!(reviewed_relaunch_receipt_with(&with(vec![]), &record, same).is_none());
        // Eligible.
        let store = with(vec![ready.clone()]);
        assert_eq!(
            reviewed_relaunch_receipt_with(&store, &record, same).map(|r| r.action_id.as_str()),
            Some("owner-start")
        );
        // Configuration changed (profile, model, permissions...).
        let mut changed = reviewed.configuration.clone();
        changed.model = "different".into();
        assert!(reviewed_relaunch_receipt_with(&store, &record, |_| Ok(changed.clone())).is_none());
        assert!(reviewed_relaunch_receipt_with(&store, &record, |_| anyhow::bail!("profile gone")).is_none());
        // Contract changed (another native / bundle).
        let mut moved = record.clone();
        let mut contract = reviewed.contract.clone();
        contract.bundle_sha256 = crate::role_revision::Sha256::new("e".repeat(64)).unwrap();
        moved.explicit_launch = Some(contract);
        assert!(reviewed_relaunch_receipt_with(&store, &moved, same).is_none());
        // Open launch claim, archived record, not explicit launch.
        let mut claimed = record.clone();
        claimed.app_server_launch_claim_id = Some("claim".into());
        assert!(reviewed_relaunch_receipt_with(&store, &claimed, same).is_none());
        let mut plain = record.clone();
        plain.explicit_launch = None;
        assert!(reviewed_relaunch_receipt_with(&store, &plain, same).is_none());
        // A newer failed / abandoned / pending launch hides an older Ready one.
        for stage in [StockRuntimeStage::Abandoned, StockRuntimeStage::Prepared, StockRuntimeStage::Spawned] {
            let newer = receipt("later", 2, stage, "2030-01-02T00:00:00Z");
            assert!(reviewed_relaunch_receipt_with(&with(vec![ready.clone(), newer]), &record, same).is_none());
        }
        let same_generation = receipt("retry", 1, StockRuntimeStage::Abandoned, "2030-01-02T00:00:00Z");
        assert!(reviewed_relaunch_receipt_with(&with(vec![ready.clone(), same_generation]), &record, same).is_none());
        // Older failures do not matter.
        let older = receipt("earlier", 0, StockRuntimeStage::Abandoned, "2029-12-31T00:00:00Z");
        assert!(reviewed_relaunch_receipt_with(&with(vec![ready.clone(), older]), &record, same).is_some());
        // Ready with an error (readiness incomplete). (A Job MCP review is
        // refused the same way; its fixture needs a live Job daemon.)
        let mut errored = ready.clone();
        errored.error = Some("readiness incomplete".into());
        assert!(reviewed_relaunch_receipt_with(&with(vec![errored]), &record, same).is_none());
        // Another agent's receipt is no evidence.
        let mut foreign = ready.clone();
        foreign.review.subject.cutex_session_id = CutexSessionId::new("cutex.other").unwrap();
        assert!(reviewed_relaunch_receipt_with(&with(vec![foreign]), &record, same).is_none());
    }

    /// A relaunch on an earlier relaunch still names the owner's original
    /// reviewed action (lead request 2026-10-14); a relaunch receipt is not
    /// owner-initiated; the latest receipt is found by generation.
    #[test]
    fn reviewed_relaunch_names_the_original_owner_action() {
        let mut record = record();
        let reviewed = review(&record);
        record.explicit_launch = Some(reviewed.contract.clone());
        let receipt = |action: &str, generation: u64, at: &str| StockRuntimeReceipt {
            launch_cwd: None,
            action_id: AgentActionId::new(action).unwrap(),
            review: reviewed.clone(),
            stage: StockRuntimeStage::Ready,
            claim_id: format!("{action}-claim"),
            runtime_agent_id: format!("stock.{generation}"),
            expected_generation: generation,
            binding: None,
            publication: None,
            error: None,
            updated_at: at.into(),
        };
        let owner = receipt("api-runtime-owner", 1, "2030-01-01T00:00:00Z");
        let first = receipt("asks-wake:cutex.director:one", 2, "2030-01-02T00:00:00Z");
        let second = receipt("asks-wake:cutex.director:two", 3, "2030-01-03T00:00:00Z");
        assert!(owner_initiated_runtime_receipt(&owner));
        assert!(!owner_initiated_runtime_receipt(&first));
        let mut store = crate::session::model::CutexSessionStore::default();
        store.sessions.insert(record.cutex_session_id.clone(), record.clone());
        for receipt in [&owner, &first, &second] {
            store.explicit_launch_receipts.insert(
                receipt.action_id.to_string(),
                ExplicitLaunchActionReceipt::Runtime(receipt.clone()),
            );
        }
        let latest = latest_runtime_receipt(&store, &record.cutex_session_id).unwrap();
        assert_eq!(latest.action_id.as_str(), "asks-wake:cutex.director:two");
        assert_eq!(original_owner_reviewed_action(&store, latest).as_str(), "api-runtime-owner");
        assert_eq!(original_owner_reviewed_action(&store, &owner).as_str(), "api-runtime-owner");
        // Owner receipt pruned by retention: the relaunch's own id.
        store.explicit_launch_receipts.remove("api-runtime-owner");
        let latest = latest_runtime_receipt(&store, &record.cutex_session_id).unwrap();
        assert_eq!(original_owner_reviewed_action(&store, latest).as_str(), "asks-wake:cutex.director:two");
    }

    /// Startup cleanup of owner-stop marks (lead request 2026-10-14): drop
    /// a mark only when the agent is gone, online, or the owner started it
    /// since (online intent, and for an explicit-launch agent an
    /// owner-started Ready latest receipt); keep every other; idempotent.
    #[test]
    fn stale_owner_marks_keep_valid_marks() {
        use crate::asks::wake::{stale_owner_marks, StaleOwnerMark};
        let plain = |id: &str| {
            CutexSessionRecord::new(id.into(), Some(uuid::Uuid::new_v4().to_string()), "p".into(), "/p".into(), None).unwrap()
        };
        let mut explicit = record();
        let reviewed = review(&explicit);
        explicit.explicit_launch = Some(reviewed.contract.clone());
        let mut relaunched = explicit.clone();
        relaunched.cutex_session_id = "cutex.relaunched".into();
        let receipt = |action: &str, subject: &str| {
            let mut review = reviewed.clone();
            review.subject.cutex_session_id = CutexSessionId::new(subject).unwrap();
            StockRuntimeReceipt {
                launch_cwd: None,
                action_id: AgentActionId::new(action).unwrap(),
                review,
                stage: StockRuntimeStage::Ready,
                claim_id: "claim".into(),
                runtime_agent_id: "stock.x".into(),
                expected_generation: 1,
                binding: None,
                publication: None,
                error: None,
                updated_at: "2030-01-01T00:00:00Z".into(),
            }
        };
        let mut sessions = crate::session::model::CutexSessionStore::default();
        for record in [plain("cutex.online"), plain("cutex.plain-started"), plain("cutex.plain-stopped"), explicit.clone(), relaunched.clone()] {
            sessions.sessions.insert(record.cutex_session_id.clone(), record);
        }
        for receipt in [receipt("api-runtime-owner", &explicit.cutex_session_id), receipt("asks-wake:cutex.d:1", "cutex.relaunched")] {
            sessions.explicit_launch_receipts.insert(receipt.action_id.to_string(), ExplicitLaunchActionReceipt::Runtime(receipt));
        }
        let mut intents = crate::session::runtime_intent::IntentState::default();
        for id in ["cutex.plain-started", explicit.cutex_session_id.as_str(), "cutex.relaunched"] {
            intents.intents.insert(id.into(), Default::default());
        }
        let mut state = crate::asks::store::AskState::default();
        for id in ["cutex.gone", "cutex.online", "cutex.plain-started", "cutex.plain-stopped", explicit.cutex_session_id.as_str(), "cutex.relaunched"] {
            state.owner_offline.insert(id.into());
        }
        let online = |record: &CutexSessionRecord| record.cutex_session_id == "cutex.online";
        let mut stale = stale_owner_marks(&state, &sessions, &intents, &online);
        stale.sort_by(|a, b| a.0.cmp(&b.0));
        let mut expected = vec![
            ("cutex.gone".to_string(), StaleOwnerMark::Removed),
            ("cutex.online".to_string(), StaleOwnerMark::Online),
            ("cutex.plain-started".to_string(), StaleOwnerMark::OwnerStarted),
            (explicit.cutex_session_id.clone(), StaleOwnerMark::OwnerStarted),
        ];
        expected.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(stale, expected);
        for (id, _) in &stale {
            state.owner_offline.remove(id);
        }
        // Still valid: offline with no later owner start, or only a relaunch.
        assert_eq!(
            state.owner_offline.iter().map(String::as_str).collect::<Vec<_>>(),
            ["cutex.plain-stopped", "cutex.relaunched"]
        );
        assert!(stale_owner_marks(&state, &sessions, &intents, &online).is_empty(), "idempotent");
    }

    struct NeverRuntime;
    impl StockRuntimeExecutor for NeverRuntime {
        fn publication(&mut self, _: &StockRuntimeReceipt) -> anyhow::Result<StockPublication> {
            panic!("unexpected runtime")
        }
        fn published_owner_absent(&mut self, _: &StockRuntimeReceipt) -> anyhow::Result<bool> {
            panic!("unexpected runtime")
        }
        fn stop(&mut self, _: &CutexSessionRecord) -> anyhow::Result<()> {
            panic!("unexpected runtime")
        }
        fn spawn(
            &mut self,
            _: &CutexSessionRecord,
            _: &StockBundle,
            _: &StockRuntimeReceipt,
        ) -> anyhow::Result<CutexAppServerRuntimeBinding> {
            panic!("unexpected runtime")
        }
        fn connect(
            &mut self,
            _: &CutexSessionRecord,
            _: &StockRuntimeReceipt,
        ) -> anyhow::Result<()> {
            panic!("unexpected runtime")
        }
        fn cleanup_owned(&mut self) -> anyhow::Result<()> {
            panic!("unexpected runtime")
        }
        fn retain_owner(&mut self) {
            panic!("unexpected runtime")
        }
    }
    /// Without matching reviewed-launch evidence the relaunch refuses before
    /// any runtime step and writes nothing.
    #[test]
    fn reviewed_relaunch_without_evidence_refuses_without_side_effects() {
        let root = std::env::temp_dir().join(format!("reviewed-relaunch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let provider = AgentManagementProvider::open(root.join("management")).unwrap();
        let mut record = record();
        let reviewed = review(&record);
        record.explicit_launch = Some(reviewed.contract.clone());
        let id = CutexSessionId::new(record.cutex_session_id.clone()).unwrap();
        let path = root.join("sessions.json");
        let mut store = crate::session::model::CutexSessionStore::default();
        store.sessions.insert(record.cutex_session_id.clone(), record.clone());
        // A Ready receipt exists, but the record's configuration cannot be
        // shown to match it (here: no such profile on this host).
        store.explicit_launch_receipts.insert(
            "owner-start".into(),
            ExplicitLaunchActionReceipt::Runtime(StockRuntimeReceipt {
                launch_cwd: None,
                action_id: AgentActionId::new("owner-start").unwrap(),
                review: reviewed,
                stage: StockRuntimeStage::Ready,
                claim_id: "claim".into(),
                runtime_agent_id: "stock.owner".into(),
                expected_generation: 1,
                binding: None,
                publication: None,
                error: None,
                updated_at: "2030-01-01T00:00:00Z".into(),
            }),
        );
        crate::session::store::save_cutex_session_store_to_path(&path, &store).unwrap();
        let before = crate::session::store::session_store_json_bytes(&path).unwrap();
        let action = AgentActionId::new("asks-wake:cutex.director:test").unwrap();
        let error = provider
            .execute_reviewed_relaunch(&path, &id, "cutex.director", &action, &mut NeverRuntime)
            .unwrap_err();
        assert!(format!("{error:#}").contains("owner_start_required"), "{error:#}");
        assert_eq!(crate::session::store::session_store_json_bytes(&path).unwrap(), before);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn runtime_review_completed_legacy_replay_keeps_bytes_by_action_id() {
        let root = std::env::temp_dir().join(format!("runtime-review-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let provider = AgentManagementProvider::open(root.join("management")).unwrap();
        let review = review(&record());
        let action = AgentActionId::new("legacy-completed").unwrap();
        let receipt = StockRuntimeReceipt {
            launch_cwd: None,
            action_id: action.clone(),
            review: review.clone(),
            stage: StockRuntimeStage::Ready,
            claim_id: "old-claim".into(),
            runtime_agent_id: "stock.old".into(),
            expected_generation: 1,
            binding: None,
            publication: None,
            error: None,
            updated_at: "2030-01-01T00:00:00Z".into(),
        };
        let mut store = crate::session::model::CutexSessionStore::default();
        store.explicit_launch_receipts.insert(
            action.to_string(),
            ExplicitLaunchActionReceipt::Runtime(receipt.clone()),
        );
        let path = root.join("sessions.json");
        crate::session::store::save_cutex_session_store_to_path(&path, &store).unwrap();
        let original = crate::session::store::session_store_json_bytes(&path).unwrap();
        {
            let _execution = super::super::provider::provider_execution_lock()
                .lock()
                .unwrap();
            let _mutation = provider.store().lock_mutations().unwrap();
            let got = provider
                .execute_stock_runtime_locked(&path, &action, &review, &mut NeverRuntime)
                .unwrap();
            assert_eq!(
                serde_json::to_vec(&got).unwrap(),
                serde_json::to_vec(&receipt).unwrap()
            );
            for field in [
                "digest_version",
                "configuration",
                "subject",
                "selected_projection",
            ] {
                let mut changed = review.clone();
                match field {
                    "digest_version" => {
                        changed.digest_version = RuntimeReviewDigestVersion::SemanticV2
                    }
                    "configuration" => changed.configuration.model = "different".into(),
                    "selected_projection" => {
                        changed.configuration.selected_projection = Some(serde_json::from_value(json!({
                            "version":2,"route":"chatgpt_file",
                            "auth":{"path":"/private/auth.json","parent_device":1,"parent_inode":2,"owner":3,"account":"a".repeat(64),"api_file":null},
                            "settings":{},"catalog":null,"requires_job":false
                        })).unwrap());
                    }
                    _ => {
                        changed.subject.authority_sha256 =
                            crate::role_revision::Sha256::new("f".repeat(64)).unwrap()
                    }
                }
                // The action id is the idempotency key: a completed action
                // replays its stored receipt unchanged.
                let replay = provider
                    .execute_stock_runtime_locked(
                        &path,
                        &action,
                        &changed,
                        &mut NeverRuntime,
                    )
                    .unwrap();
                assert_eq!(
                    serde_json::to_vec(&replay).unwrap(),
                    serde_json::to_vec(&receipt).unwrap()
                );
            }
        }
        assert_eq!(crate::session::store::session_store_json_bytes(&path).unwrap(), original);
        std::fs::remove_dir_all(root).unwrap();
    }
}

/// A persisted startup/restart intent suppresses delivery to the old owner while
/// external stop/connect runs without holding the global Task Service lock.
pub fn native_runtime_transition_pending(
    store: &crate::session::model::CutexSessionStore,
    record: &CutexSessionRecord,
) -> bool {
    record.explicit_launch.is_some() && (record.app_server_launch_claim_id.is_some()
        || store.explicit_launch_receipts.values().any(|r| matches!(r,
            ExplicitLaunchActionReceipt::Runtime(r)
                if r.stage == StockRuntimeStage::Prepared
                    && r.review.subject.cutex_session_id.as_str() == record.cutex_session_id
                    && r.review.subject.runtime_generation == record.runtime_generation)))
}

#[cfg(test)]
mod shared_projection_marker_tests {
    use super::*;

    fn marker_fixture(header: &str) -> (std::path::PathBuf, ExplicitLaunchContract) {
        let root = std::env::temp_dir().join(format!("projection-marker-{}", uuid::Uuid::new_v4()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&root).unwrap();
        }
        #[cfg(not(unix))]
        std::fs::create_dir_all(&root).unwrap();
        let config = root.join("config.toml");
        std::fs::write(&config, format!("{header}\nmodel='gpt-5'\n")).unwrap();
        let bundle = serde_json::json!({
            "version": 4,
            "upstream_commit": "test-upstream",
            "executable": {"path": root.join("absent-codex"), "sha256": "0".repeat(64)},
            "cli": {"path": root.join("absent-cli"), "sha256": "1".repeat(64)},
            "code_mode_host": {"path": root.join("absent-host"), "sha256": "2".repeat(64)},
            "facade": {"path": root.join("absent-facade"), "sha256": "3".repeat(64)},
            "schema": {"path": root.join("absent-schema"), "sha256": "4".repeat(64)},
            "shared_config": {"path": &config, "sha256": "5".repeat(64)},
        });
        let manifest = root.join("bundle.json");
        std::fs::write(&manifest, serde_json::to_vec(&bundle).unwrap()).unwrap();
        let contract = ExplicitLaunchContract {
            version: 4,
            migration_action_id: None,
            native_id: uuid::Uuid::new_v4().to_string(),
            native_home: root.canonicalize().unwrap(),
            bundle_manifest: manifest.canonicalize().unwrap(),
            bundle_sha256: file_sha256(&manifest).unwrap(),
        };
        (root, contract)
    }

    #[test]
    fn launch_review_removes_legacy_marker_and_is_idempotent() {
        let (root, contract) = marker_fixture("cutex_projection_version=2");
        sanitize_shared_projection_marker(&contract).unwrap();
        let raw = std::fs::read_to_string(root.join("config.toml")).unwrap();
        assert!(!raw.contains("cutex_projection_version"));
        assert_eq!(
            toml::from_str::<toml::Value>(&raw).unwrap()["model"].as_str(),
            Some("gpt-5")
        );
        // A clean config is left byte-identical, and a second pass is a no-op.
        sanitize_shared_projection_marker(&contract).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("config.toml")).unwrap(), raw);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn marker_less_config_and_unknown_marker_versions_fail_closed() {
        let (root, contract) = marker_fixture("model_reasoning_effort='medium'");
        let before = std::fs::read_to_string(root.join("config.toml")).unwrap();
        sanitize_shared_projection_marker(&contract).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("config.toml")).unwrap(), before);
        std::fs::remove_dir_all(root).unwrap();

        let (root, contract) = marker_fixture("cutex_projection_version=999");
        assert!(sanitize_shared_projection_marker(&contract).is_err());
        assert!(std::fs::read_to_string(root.join("config.toml")).unwrap().contains("999"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
