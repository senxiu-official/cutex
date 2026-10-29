use std::sync::{Arc, Mutex, OnceLock};

#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};

use crate::role_revision::{CutexSessionId, Rfc3339, Sha256};

use super::store::{now, request_sha256, AgentManagementSnapshot};
use super::*;

#[path = "provider_archive.rs"]
mod archive_operations;
pub use archive_operations::ManagedAgentArchiveRequest;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LifecycleFailure {
    pub code: String,
    pub detail: String,
    pub outcome_unknown: bool,
    pub known_native_session_id: Option<String>,
}

impl LifecycleFailure {
    pub fn definite(code: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            detail: detail.into(),
            outcome_unknown: false,
            known_native_session_id: None,
        }
    }

    pub fn outcome_unknown(code: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            detail: detail.into(),
            outcome_unknown: true,
            known_native_session_id: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeRecoveryOutcome {
    NoClaim,
    RecoveredExact,
    ClearedDeadClaim,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeBootstrapReconciliation {
    /// Every provider-owned native and runtime source was readable and none
    /// contained a session/runtime created by the historical bootstrap.
    ProvenAbsent { reason: String },
    /// Exact correlated runtime/session evidence exists.
    Present { reason: String },
    /// Evidence exists but cannot be correlated conclusively.
    Ambiguous { reason: String },
    /// A provider-owned evidence source could not be read or validated.
    Unavailable { reason: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeBootstrapIdentityReconciliation {
    /// Exactly one native thread was correlated to the reserved identity.
    Exact {
        native_session_id: String,
        reason: String,
    },
    /// All provider-owned evidence sources were readable and no matching
    /// native thread exists. This does not authorize a relaunch for an
    /// ambiguous historical attempt.
    Absent { reason: String },
    /// Evidence is present but not uniquely identity-correlated.
    Ambiguous { reason: String },
    /// A provider-owned source could not be read or validated.
    Unavailable { reason: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HistoricalRuntimeOccurrenceReconciliation {
    /// Every provider-owned source was readable and proves that no runtime,
    /// endpoint, process, or launch claim exists for the durable target.
    ProvenAbsent {
        fence: RuntimeOccurrenceFence,
        reason: String,
    },
    /// A live or durably claimed occurrence exists. Legacy actions without an
    /// exact occurrence identity must never act on it.
    Present { reason: String },
    /// Evidence exists but is incomplete or conflicts across sources.
    Ambiguous { reason: String },
    /// An authoritative source could not be read or validated.
    Unavailable { reason: String },
}

pub trait AgentLifecycle: Send + Sync {
    /// Resolve the launch backend after request hashing; preserve exact replay identity.
    fn effective_create_spec(&self, spec: &ManagedAgentSpec) -> Result<ManagedAgentSpec, LifecycleFailure> {
        Ok(spec.clone())
    }

    fn bootstrap_reviewed(
        &self,
        _permit: &BootstrapExecutionPermit<'_>,
    ) -> Result<String, LifecycleFailure> {
        Err(LifecycleFailure::definite(
            "reviewed_bootstrap_adapter_required",
            "no default launch is permitted",
        ))
    }
    fn confirm_reviewed_bootstrap(
        &self,
        _permit: &BootstrapExecutionPermit<'_>,
        _native: &str,
    ) -> Result<(), LifecycleFailure> {
        Err(LifecycleFailure::outcome_unknown(
            "reviewed_bootstrap_ack_required",
            "exact native persistence acknowledgement required",
        ))
    }
    fn adopt_reviewed(
        &self,
        _permit: &BootstrapExecutionPermit<'_>,
        _native: &str,
    ) -> Result<CutexSessionId, LifecycleFailure> {
        Err(LifecycleFailure::outcome_unknown(
            "reviewed_bootstrap_adapter_required",
            "atomic adoption and launch marker required",
        ))
    }
    fn online_reviewed(
        &self,
        _permit: &BootstrapExecutionPermit<'_>,
        _id: &CutexSessionId,
    ) -> Result<(), LifecycleFailure> {
        Err(LifecycleFailure::outcome_unknown(
            "reviewed_bootstrap_adapter_required",
            "no default launch is permitted",
        ))
    }
    fn prepare_private_cwd(&self, spec: &ManagedAgentSpec) -> Result<(), LifecycleFailure>;
    fn bootstrap_native(&self, spec: &ManagedAgentSpec) -> Result<String, LifecycleFailure>;
    /// A durable implementation must apply this frozen actor before its first
    /// Cutex record publication (Claude publishes during bootstrap itself).
    fn bootstrap_native_with_creator(&self, spec: &ManagedAgentSpec, _creator: Option<&crate::session::model::SessionCreatedBy>) -> Result<String, LifecycleFailure> {
        self.bootstrap_native(spec)
    }

    fn reconcile_pre_sid_bootstrap(
        &self,
        _spec: &ManagedAgentSpec,
        _started_at: &crate::role_revision::Rfc3339,
        _failed_at: &crate::role_revision::Rfc3339,
    ) -> Result<NativeBootstrapReconciliation, LifecycleFailure> {
        Err(LifecycleFailure::outcome_unknown(
            "native_bootstrap_reconciliation_unavailable",
            "lifecycle provider cannot prove the historical native bootstrap absent",
        ))
    }
    fn reconcile_interrupted_native_bootstrap(
        &self,
        _spec: &ManagedAgentSpec,
        _started_at: &Rfc3339,
        _failed_at: &Rfc3339,
    ) -> Result<NativeBootstrapIdentityReconciliation, LifecycleFailure> {
        Err(LifecycleFailure::outcome_unknown(
            "native_bootstrap_reconciliation_unavailable",
            "provider cannot establish interrupted creator termination",
        ))
    }
    fn reconcile_ambiguous_native_bootstrap(
        &self,
        _spec: &ManagedAgentSpec,
        _started_at: &crate::role_revision::Rfc3339,
        _failed_at: &crate::role_revision::Rfc3339,
    ) -> Result<NativeBootstrapIdentityReconciliation, LifecycleFailure> {
        Err(LifecycleFailure::outcome_unknown(
            "native_bootstrap_reconciliation_unavailable",
            "lifecycle provider cannot identify the historical native bootstrap",
        ))
    }
    fn reconcile_historical_runtime_occurrence(
        &self,
        _cutex_session_id: &CutexSessionId,
    ) -> Result<HistoricalRuntimeOccurrenceReconciliation, LifecycleFailure> {
        Err(LifecycleFailure::outcome_unknown(
            "runtime_occurrence_reconciliation_unavailable",
            "lifecycle provider cannot fence the historical runtime occurrence",
        ))
    }
    /// Capture before offline, or conservatively reconstruct a legacy timeout
    /// occurrence from its exact successful PID receipt and original window.
    fn capture_offline_occurrence(
        &self,
        _id: &CutexSessionId,
        _legacy_timeout: Option<(&crate::role_revision::Rfc3339, &[u32])>,
    ) -> Result<Option<RuntimeOccurrenceFence>, LifecycleFailure> {
        Ok(None)
    }
    /// Clear only the captured occurrence after authoritative physical absence.
    /// Must also accept its already-cleared form for crash recovery.
    fn reconcile_offline_scope_timeout(
        &self,
        _id: &CutexSessionId,
        _expected: &RuntimeOccurrenceFence,
    ) -> Result<RuntimeOccurrenceFence, LifecycleFailure> {
        Err(LifecycleFailure::outcome_unknown(
            "scope_timeout_reconciliation_unavailable",
            "the lifecycle adapter cannot prove the timed-out occurrence absent",
        ))
    }
    fn adopt_native(
        &self,
        native_session_id: &str,
        spec: &ManagedAgentSpec,
    ) -> Result<CutexSessionId, LifecycleFailure>;
    /// Native adoption receives the creation actor separately from the model's
    /// request. It must not infer one from the target Profile or project seat.
    fn adopt_native_with_creator(&self, native_session_id: &str, spec: &ManagedAgentSpec, _creator: Option<&crate::session::model::SessionCreatedBy>) -> Result<CutexSessionId, LifecycleFailure> {
        self.adopt_native(native_session_id, spec)
    }
    fn configure(
        &self,
        cutex_session_id: &CutexSessionId,
        native_session_id: &str,
        spec: &ManagedAgentSpec,
    ) -> Result<(), LifecycleFailure>;
    fn recover_runtime(
        &self,
        cutex_session_id: &CutexSessionId,
        native_session_id: &str,
        spec: &ManagedAgentSpec,
    ) -> Result<RuntimeRecoveryOutcome, LifecycleFailure>;
    /// `Some(detail)` when the durable Agent still uses a backend that can no
    /// longer start (legacy cute-alden without an explicit native contract).
    /// Checked before any action record or runtime recovery comparison.
    fn migration_required(&self, _cutex_session_id: &CutexSessionId) -> Option<String> {
        None
    }
    fn online(&self, cutex_session_id: &CutexSessionId) -> Result<(), LifecycleFailure>;
    fn online_managed(
        &self,
        _permit: &RuntimeExecutionPermit<'_>,
        cutex_session_id: &CutexSessionId,
    ) -> Result<(), LifecycleFailure> {
        self.online(cutex_session_id)
    }
    fn restart_managed_if_occurrence(
        &self,
        _permit: &RuntimeExecutionPermit<'_>,
        id: &CutexSessionId,
        expected: &RuntimeOccurrenceFence,
    ) -> Result<(AgentRuntimeObservation, AgentRuntimeObservation), LifecycleFailure> {
        self.restart_if_occurrence(id, expected)
    }

    fn offline(&self, cutex_session_id: &CutexSessionId) -> Result<(), LifecycleFailure>;
    fn offline_if_occurrence(
        &self,
        cutex_session_id: &CutexSessionId,
        expected: &RuntimeOccurrenceFence,
    ) -> Result<(), LifecycleFailure> {
        let observed = self.observe(cutex_session_id)?;
        if observed.runtime_generation != expected.runtime_generation
            || observed.runtime_agent_ids
                != expected
                    .current_runtime_agent_id
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
            || observed.agent_bus_endpoint_ids != expected.agent_bus_endpoint_ids
            || observed.app_server_runtime != expected.app_server_connected
        {
            return Err(LifecycleFailure::outcome_unknown(
                "runtime_occurrence_changed",
                "runtime occurrence changed before the fenced offline effect",
            ));
        }
        self.offline(cutex_session_id)
    }
    fn restart_if_occurrence(
        &self,
        _cutex_session_id: &CutexSessionId,
        _expected: &RuntimeOccurrenceFence,
    ) -> Result<(AgentRuntimeObservation, AgentRuntimeObservation), LifecycleFailure> {
        Err(LifecycleFailure::outcome_unknown(
            "fenced_restart_unavailable",
            "lifecycle provider cannot atomically fence a historical restart",
        ))
    }
    fn retire(&self, cutex_session_id: &CutexSessionId) -> Result<(), LifecycleFailure>;
    /// Optional isolated durable store supplied by a lifecycle adapter. The
    /// production provider uses its configured session store instead.
    fn archive_session_store_path(&self) -> Option<std::path::PathBuf> { None }
    /// Optional cache invalidation after the durable archive transaction.
    fn archive_committed(&self, _record: &crate::session::model::CutexSessionRecord) {}
    fn observe(
        &self,
        cutex_session_id: &CutexSessionId,
    ) -> Result<AgentRuntimeObservation, LifecycleFailure>;
    fn send_message(
        &self,
        system: &crate::agent_bus::identity::AgentManagementSystemPrincipal,
        metadata: &AgentManagementMessageMetadata,
        to_agent: &CutexSessionId,
        exact_message: &str,
        external_message_id: &str,
    ) -> Result<String, LifecycleFailure>;
}

pub trait AgentManagementPhaseObserver: Send + Sync {
    fn phase_committed(&self, event: &AgentManagementPhaseEvent);
}

impl<F> AgentManagementPhaseObserver for F
where
    F: Fn(&AgentManagementPhaseEvent) + Send + Sync,
{
    fn phase_committed(&self, event: &AgentManagementPhaseEvent) {
        self(event);
    }
}

#[derive(Default)]
struct NoopPhaseObserver;

impl AgentManagementPhaseObserver for NoopPhaseObserver {
    fn phase_committed(&self, _event: &AgentManagementPhaseEvent) {}
}

/// r254 (design 044 §6): one Project authority transition, as the provider
/// is about to commit it. `previous` is the authority the CAS replaces
/// (`None` for the first bind of a project).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorityTransitionIntent {
    pub action_id: AgentActionId,
    pub request_sha256: Sha256,
    pub project_id: ProjectId,
    pub previous: Option<ProjectAuthority>,
    /// `None`: the owner cleared the seat (`clear_director`).
    pub next: Option<ProjectAuthority>,
}

/// r254: the Director-address extension around the authority CAS. The two
/// stores are separate persistence domains: `prepare` durably records the
/// intent BEFORE the CAS is written (an error refuses the bind, nothing is
/// committed); `finalize` runs after the authority receipt is persisted, and
/// again on every same-action replay, so a crash between the two is completed
/// idempotently from the real receipt. `prepare` runs under the roster
/// mutation lock (lock order: roster, then the extension store) and must not
/// do native I/O.
pub trait AuthorityTransitionHook: Send + Sync {
    fn prepare(
        &self,
        intent: &AuthorityTransitionIntent,
        snapshot: &AgentManagementSnapshot,
    ) -> Result<(), AgentManagementError>;
    fn finalize(
        &self,
        receipt: &ProjectAuthorityReceipt,
        snapshot: &AgentManagementSnapshot,
    ) -> Result<(), AgentManagementError>;
    /// After an owner project mutation (create with a Director / assign /
    /// rotate / clear) committed, and on its replay: the witness is the
    /// durable `human_management_project_mutations[action_id]` record.
    fn finalize_owner_change(
        &self,
        _action_id: &AgentActionId,
        _snapshot: &AgentManagementSnapshot,
    ) -> Result<(), AgentManagementError> {
        Ok(())
    }
    /// Startup: settle prepared intents from real receipts and account
    /// authority changes that bypassed the hook.
    fn recover(&self, _snapshot: &AgentManagementSnapshot) -> Result<(), AgentManagementError> {
        Ok(())
    }
}

pub struct AgentManagementProvider {
    store: AgentManagementStore,
    current_names_path: Option<std::path::PathBuf>,
    phase_observer: Arc<dyn AgentManagementPhaseObserver>,
    authority_hook: Option<Arc<dyn AuthorityTransitionHook>>,
    #[cfg(test)]
    fail_after_predecessor_close_once: Arc<AtomicBool>,
}

pub(super) fn provider_execution_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

impl AgentManagementProvider {
    pub fn open(root: impl Into<std::path::PathBuf>) -> Result<Self, AgentManagementError> {
        let root = root.into();
        Ok(Self {
            store: AgentManagementStore::open(&root)?,
            current_names_path: None,
            phase_observer: Arc::new(NoopPhaseObserver),
            authority_hook: None,
            #[cfg(test)]
            fail_after_predecessor_close_once: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn open_default() -> anyhow::Result<Self> {
        Ok(Self {
            store: AgentManagementStore::open_default()?,
            current_names_path: Some(crate::session::store::cutex_sessions_path()?),
            phase_observer: Arc::new(NoopPhaseObserver),
            authority_hook: crate::director_address::management::default_authority_hook(),
            #[cfg(test)]
            fail_after_predecessor_close_once: Arc::new(AtomicBool::new(false)),
        })
    }

    pub(in crate::agent_management) fn authority_hook(&self) -> Option<Arc<dyn AuthorityTransitionHook>> {
        self.authority_hook.clone()
    }

    /// Runs the authority hook's startup recovery under the provider
    /// execution lock, so no bind can interleave with it.
    pub fn recover_authority_hook(&self) -> Result<(), AgentManagementError> {
        let Some(hook) = self.authority_hook.clone() else { return Ok(()) };
        let _execution = provider_execution_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Same fence as a bind: no other process can bind meanwhile.
        let _mutation = self.store.lock_mutations()?;
        hook.recover(&self.store.snapshot()?)
    }

    pub fn with_authority_hook(mut self, hook: Arc<dyn AuthorityTransitionHook>) -> Self {
        self.authority_hook = Some(hook);
        self
    }

    pub fn with_phase_observer(mut self, observer: Arc<dyn AgentManagementPhaseObserver>) -> Self {
        self.phase_observer = observer;
        self
    }

    /// Explicit durable observation source for current reads, never action replay.
    /// Production open_default always configures this; isolated providers can
    /// supply their own store without consulting global session state.
    pub fn with_current_names_path(mut self, path: std::path::PathBuf) -> Self {
        self.current_names_path = Some(path);
        self
    }

    pub(super) fn require_not_reversibly_archived(
        &self,
        id: &CutexSessionId,
    ) -> Result<(), AgentManagementError> {
        if self
            .current_name_snapshot()?
            .reversible_archive_projection
            .get(id)
            == Some(&true)
        {
            return Err(AgentManagementError::Conflict(
                "agent_archived_restore_required",
            ));
        }
        Ok(())
    }

    pub(crate) fn current_name_snapshot(
        &self,
    ) -> Result<AgentManagementSnapshot, AgentManagementError> {
        let mut snapshot = self.store.snapshot()?;
        // Custom lifecycle adapters may use an isolated store. Their committed
        // archive receipts still suppress ordinary lifecycle/name reservations.
        if self.current_names_path.is_none() {
            let mut receipts = snapshot.agent_archive_actions.values()
                .filter(|r| r.stage == AgentArchiveStage::Committed)
                .collect::<Vec<_>>();
            receipts.sort_by_key(|r| r.result.as_ref().map(|v| v.durable_revision()).unwrap_or(0));
            let projection = receipts.into_iter().filter_map(|r| r.result.as_ref())
                .filter_map(|r| CutexSessionId::new(r.cutex_session_id.clone()).ok().map(|id| (id, r.is_retired())))
                .collect();
            snapshot.reversible_archive_projection = projection;
        }
        if let Some(path) = &self.current_names_path {
            let sessions = crate::session::store::load_cutex_session_store_from_path(path)
                .map_err(|_| AgentManagementError::PersistenceUnavailable)?;
            for agent in snapshot.agents.values_mut() {
                let record = sessions
                    .sessions
                    .get(agent.cutex_session_id.as_str())
                    .filter(|record| record.cutex_session_id == agent.cutex_session_id.as_str())
                    .ok_or_else(|| {
                        AgentManagementError::OwnerActionRequired(format!(
                            "durable_name_observation_unavailable: {}",
                            agent.cutex_session_id.as_str()
                        ))
                    })?;
                if record.explicit_launch.as_ref().is_some_and(|contract| {
                    contract.native_id == agent.native_session_id
                        && record.codex_session_id.as_deref()
                            == Some(agent.native_session_id.as_str())
                }) && record.runtime_backend
                    == crate::session::model::CutexSessionRuntimeBackend::Host
                {
                    // Current configuration belongs to the durable native record.
                    // Creation specs/receipts remain immutable provenance, not a
                    // second authority that can reject legitimate Human edits.
                    agent.spec.runtime_backend = "host".into();
                    agent.spec.cwd =
                        crate::session::metadata::cutex_session_launch_cwd(record).into();
                    agent.spec.profile = record.profile.clone();
                    agent.spec.model = record.model_defaults.clone().unwrap_or_default();
                    agent.spec.reasoning = record.reasoning_defaults.clone().unwrap_or_default();
                    agent.spec.permissions = record.permission_defaults.clone().unwrap_or_default();
                    agent.spec.approval_policy = record.approval_policy.clone().unwrap_or_default();
                    agent.spec.sandbox_mode = record.sandbox_mode.clone().unwrap_or_default();
                    agent.spec.groups = record.agent_groups.clone();
                    agent.spec.expose_to_im = record.exposed_to_backend;
                    agent.spec.pin = record.quick_action
                        == crate::session::model::CutexSessionQuickActionMode::Pinned;
                }
                snapshot
                    .reversible_archive_projection
                    .insert(agent.cutex_session_id.clone(), record.is_retired());
                if let Some(name) = &record.formal_agent_name {
                    if name.trim().is_empty() || name.chars().any(char::is_control) {
                        return Err(AgentManagementError::OwnerActionRequired(format!(
                            "malformed_formal_agent_name: {}",
                            agent.cutex_session_id.as_str()
                        )));
                    }
                    agent.spec.name = name.clone();
                }
            }
        }
        Ok(snapshot)
    }

    #[cfg(test)]
    fn with_fail_after_predecessor_close_once(self) -> Self {
        self.fail_after_predecessor_close_once
            .store(true, Ordering::SeqCst);
        self
    }

    #[cfg(test)]
    fn inject_process_loss_after_predecessor_close(
        &self,
        request: &AuthorizedAgentManagementRequest<'_>,
    ) -> Option<AgentManagementResponse> {
        self.fail_after_predecessor_close_once
            .swap(false, Ordering::SeqCst)
            .then(|| {
                no_write(
                    &request.action_id,
                    "injected_process_loss",
                    "test-only process loss after predecessor close",
                )
            })
    }

    #[cfg(not(test))]
    fn inject_process_loss_after_predecessor_close(
        &self,
        _request: &AuthorizedAgentManagementRequest<'_>,
    ) -> Option<AgentManagementResponse> {
        None
    }

    pub fn store(&self) -> &AgentManagementStore {
        &self.store
    }

    pub(super) fn notify_phase(&self, event: &AgentManagementPhaseEvent) {
        self.phase_observer.phase_committed(event);
    }

    pub fn bind_project_authority(
        &self,
        request: &ProjectAuthorityRequest,
    ) -> Result<ProjectAuthorityReceipt, AgentManagementError> {
        let _execution = provider_execution_lock()
            .lock()
            .map_err(|_| AgentManagementError::PersistenceUnavailable)?;
        let _mutation = self.store.lock_mutations()?;
        if request.expected_authority_epoch == Some(0)
            || request.expected_authorized_director_session.is_some()
                != request.expected_authority_epoch.is_some()
        {
            return Err(AgentManagementError::InvalidRequest(
                "authority_expectations_must_be_complete",
            ));
        }
        let digest = request_sha256(request)?;
        let hook = self.authority_hook.clone();
        // r219: the Agent Management authority is the only Director record;
        // there is no Task Service seat copy to transfer.
        let receipt = self.store.with_state(true, |mut state| {
            if state
                .durable_import_actions
                .contains_key(&request.action_id)
                || state.actions.contains_key(&request.action_id)
                || state
                    .human_management_operator_actions
                    .contains_key(&request.action_id)
                || state
                    .human_management_project_mutations
                    .contains_key(&request.action_id)
                || state
                    .legacy_director_ownership_import_receipts
                    .contains_key(&request.action_id)
                || state
                    .reservation_reconciliation_receipts
                    .contains_key(&request.action_id)
            {
                return Err(AgentManagementError::Conflict("action_id_domain_conflict"));
            }
            if let Some(receipt) = state.authority_receipts.get(&request.action_id).cloned() {
                return if receipt.request_sha256 == digest {
                    Ok((state, receipt, false))
                } else {
                    Err(AgentManagementError::Conflict("action_id_payload_conflict"))
                };
            }
            let epoch = match state.projects.get(&request.project_id) {
                None => {
                    if request.expected_authorized_director_session.is_some() {
                        return Err(AgentManagementError::Conflict(
                            "project_authority_not_initialized",
                        ));
                    }
                    1
                }
                Some(current) => {
                    if request.expected_authorized_director_session.as_ref()
                        != Some(&current.authorized_director_session)
                        || request.expected_authority_epoch != Some(current.authority_epoch)
                    {
                        return Err(AgentManagementError::Conflict("stale_project_authority"));
                    }
                    current
                        .authority_epoch
                        .checked_add(1)
                        .filter(|epoch| *epoch <= crate::role_revision::MAX_JSON_SAFE_INTEGER)
                        .ok_or(AgentManagementError::Conflict("authority_epoch_overflow"))?
                }
            };
            let authority = ProjectAuthority {
                project_id: request.project_id.clone(),
                authorized_director_session: request.authorized_director_session.clone(),
                authority_epoch: epoch,
                updated_at: now(),
            };
            if state
                .operator_grants
                .get(&request.project_id)
                .is_some_and(|grants| grants.contains_key(&request.authorized_director_session))
            {
                return Err(AgentManagementError::Conflict(
                    "primary_director_cannot_be_operator",
                ));
            }
            let receipt = ProjectAuthorityReceipt {
                schema: AgentManagementReceiptSchema::V1,
                action_id: request.action_id.clone(),
                request_sha256: digest.clone(),
                authority: authority.clone(),
            };
            if let Some(hook) = &hook {
                // Prepared sidecar intent before the CAS write (design 044 §6.1).
                hook.prepare(
                    &AuthorityTransitionIntent {
                        action_id: request.action_id.clone(),
                        request_sha256: digest.clone(),
                        project_id: request.project_id.clone(),
                        previous: state.projects.get(&request.project_id).cloned(),
                        next: Some(authority.clone()),
                    },
                    &state,
                )?;
            }
            state.projects.insert(request.project_id.clone(), authority);
            state
                .authority_receipts
                .insert(request.action_id.clone(), receipt.clone());
            Ok((state, receipt, true))
        })?;
        if let Some(hook) = &hook {
            // The receipt is the commit witness; a finalize failure leaves the
            // prepared intent for recovery (same-action replay or startup).
            match self.store.snapshot() {
                Ok(snapshot) => {
                    if let Err(error) = hook.finalize(&receipt, &snapshot) {
                        eprintln!("director-address finalize of {} deferred to recovery: {error:?}", receipt.action_id.as_str());
                    }
                }
                Err(error) => eprintln!("director-address finalize: roster snapshot unavailable: {error:?}"),
            }
        }
        Ok(receipt)
    }

    /// Atomically imports the one missing ownership record for an exact legacy
    /// Director. This is called only by the root administration adapter; it is
    /// deliberately not part of `AgentOperation` or the ambient Agent tool.
    pub fn import_legacy_director_ownership<F>(
        &self,
        request: &LegacyDirectorOwnershipImportRequest,
        load_evidence: F,
    ) -> Result<(LegacyDirectorOwnershipImportReceipt, bool), AgentManagementError>
    where
        F: FnOnce() -> Result<LegacyDirectorOwnershipEvidence, AgentManagementError>,
    {
        let _execution = provider_execution_lock()
            .lock()
            .map_err(|_| AgentManagementError::PersistenceUnavailable)?;
        let _mutation = self.store.lock_mutations()?;
        if request.expected_authority_epoch == 0
            || request.expected_authority_epoch > crate::role_revision::MAX_JSON_SAFE_INTEGER
        {
            return Err(AgentManagementError::InvalidRequest(
                "invalid_authority_epoch",
            ));
        }
        if request.expected_authorized_director_session != request.director_cutex_session_id {
            return Err(AgentManagementError::InvalidRequest(
                "expected_authority_must_name_imported_director",
            ));
        }
        let digest = request_sha256(request)?;
        self.store.with_state(true, |mut state| {
            if state
                .durable_import_actions
                .contains_key(&request.action_id)
                || state.actions.contains_key(&request.action_id)
                || state
                    .human_management_operator_actions
                    .contains_key(&request.action_id)
                || state
                    .human_management_project_mutations
                    .contains_key(&request.action_id)
                || state.authority_receipts.contains_key(&request.action_id)
                || state
                    .reservation_reconciliation_receipts
                    .contains_key(&request.action_id)
            {
                return Err(AgentManagementError::Conflict("action_id_domain_conflict"));
            }
            if let Some(receipt) = state
                .legacy_director_ownership_import_receipts
                .get(&request.action_id)
                .cloned()
            {
                return if receipt.request_sha256 == digest {
                    Ok((state, (receipt, true), false))
                } else {
                    Err(AgentManagementError::Conflict("action_id_payload_conflict"))
                };
            }

            let authority = state.projects.get(&request.project_id).cloned().ok_or(
                AgentManagementError::Conflict("project_authority_not_initialized"),
            )?;
            if authority.authorized_director_session != request.director_cutex_session_id
                || authority.authorized_director_session
                    != request.expected_authorized_director_session
                || authority.authority_epoch != request.expected_authority_epoch
            {
                return Err(AgentManagementError::Conflict("stale_project_authority"));
            }
            if state.projects.iter().any(|(project_id, candidate)| {
                project_id != &request.project_id
                    && candidate.authorized_director_session == request.director_cutex_session_id
            }) {
                return Err(AgentManagementError::Conflict(
                    "director_authorized_for_multiple_projects",
                ));
            }
            if let Some(existing) = state.agents.get(&request.director_cutex_session_id) {
                let reason = if existing.project_id.as_ref() != Some(&request.project_id) {
                    "director_owned_by_another_project"
                } else if existing.retired_at.is_some() {
                    "director_ownership_record_retired"
                } else {
                    "director_ownership_record_exists"
                };
                return Err(AgentManagementError::Conflict(reason));
            }

            let evidence = load_evidence()?;
            if evidence.director_cutex_session_id != request.director_cutex_session_id {
                return Err(AgentManagementError::Conflict(
                    "durable_session_identity_mismatch",
                ));
            }
            if evidence.native_session_id.trim().is_empty()
                || evidence.native_session_id.trim() != evidence.native_session_id
            {
                return Err(AgentManagementError::InvalidRequest(
                    "invalid_native_session_id",
                ));
            }
            if evidence.durable_session_revision == 0
                || evidence.durable_session_revision > crate::role_revision::MAX_JSON_SAFE_INTEGER
                || evidence.runtime_generation > crate::role_revision::MAX_JSON_SAFE_INTEGER
            {
                return Err(AgentManagementError::Conflict(
                    "invalid_durable_lifecycle_evidence",
                ));
            }
            evidence.spec.validate()?;
            if state.agents.values().any(|agent| {
                agent.native_session_id == evidence.native_session_id
                    && agent.cutex_session_id != request.director_cutex_session_id
            }) {
                return Err(AgentManagementError::Conflict(
                    "native_session_owned_by_another_agent",
                ));
            }

            let agent = ManagedAgentRecord {
                project_id: Some(request.project_id.clone()),
                created_by_director_session: Some(request.director_cutex_session_id.clone()),
                created_by_operator_session: None,
                cutex_session_id: request.director_cutex_session_id.clone(),
                native_session_id: evidence.native_session_id,
                spec: evidence.spec,
                created_at: now(),
                retired_at: None,
            };
            let receipt = LegacyDirectorOwnershipImportReceipt {
                schema: LegacyDirectorOwnershipImportReceiptSchema::V1,
                action_id: request.action_id.clone(),
                request_sha256: digest.clone(),
                authority: authority.clone(),
                agent: agent.clone(),
                durable_session_revision: evidence.durable_session_revision,
                runtime_generation: evidence.runtime_generation,
            };
            state
                .agents
                .insert(request.director_cutex_session_id.clone(), agent);
            state
                .legacy_director_ownership_import_receipts
                .insert(request.action_id.clone(), receipt.clone());
            Ok((state, (receipt, false), true))
        })
    }

    /// Root-only recovery for one legacy reservation whose immutable failure
    /// and every provider-owned successor source prove that no effect occurred.
    /// The evidence callback is invoked while both the provider execution lock
    /// and durable store lock are held.
    pub fn reconcile_agent_reservation<F>(
        &self,
        request: &AgentReservationReconciliationRequest,
        reconcile: F,
    ) -> Result<(AgentReservationReconciliationReceipt, bool), AgentManagementError>
    where
        F: FnOnce(
            &str,
            &str,
            &crate::role_revision::Rfc3339,
            &crate::role_revision::Rfc3339,
        ) -> AgentReservationReconciliationEvidence,
    {
        let _execution = provider_execution_lock()
            .lock()
            .map_err(|_| AgentManagementError::PersistenceUnavailable)?;
        validate_reservation_reconciliation_request(request)?;
        let digest = request_sha256(request)?;
        let (receipt, replayed, phase_event) =
            self.store.with_state(true, |mut state| {
                if state
                    .durable_import_actions
                    .contains_key(&request.action_id)
                    || state.actions.contains_key(&request.action_id)
                    || state
                        .human_management_operator_actions
                        .contains_key(&request.action_id)
                    || state
                        .human_management_project_mutations
                        .contains_key(&request.action_id)
                    || state.authority_receipts.contains_key(&request.action_id)
                    || state
                        .legacy_director_ownership_import_receipts
                        .contains_key(&request.action_id)
                {
                    return Err(AgentManagementError::Conflict("action_id_domain_conflict"));
                }
                if let Some(receipt) = state
                    .reservation_reconciliation_receipts
                    .get(&request.action_id)
                    .cloned()
                {
                    return if receipt.request_sha256 == digest {
                        Ok((state, (receipt, true, None), false))
                    } else {
                        Err(AgentManagementError::Conflict("action_id_payload_conflict"))
                    };
                }

                let authority = state.projects.get(&request.project_id).ok_or(
                    AgentManagementError::Conflict("project_authority_not_initialized"),
                )?;
                if authority.authorized_director_session
                    != request.expected_authorized_director_session
                    || authority.authority_epoch != request.expected_authority_epoch
                {
                    return Err(AgentManagementError::Conflict("stale_project_authority"));
                }

                let target = state
                    .actions
                    .get(&request.target_action_id)
                    .cloned()
                    .ok_or(AgentManagementError::NotFound("target_action_not_found"))?;
                validate_reconciliation_target(&state, request, &target)?;
                // Agents share cwds and a closed Agent's name may be reused:
                // an ownership record created before the reservation is
                // another Agent, not its result.
                if state.agents.values().any(|agent| {
                    (agent.spec.name == request.expected_reserved_agent_name
                        || agent.spec.cwd == request.expected_reserved_agent_cwd)
                        && !record_predates(&agent.created_at, &target.created_at)
                }) {
                    return Err(AgentManagementError::ReconciliationPresent(
                    "a durable Agent Management ownership record matches the reserved name or cwd"
                        .to_string(),
                ));
                }
                let absence_evidence = match reconcile(
                    &request.expected_reserved_agent_name,
                    &request.expected_reserved_agent_cwd,
                    &target.created_at,
                    &target.updated_at,
                ) {
                    AgentReservationReconciliationEvidence::ProvenAbsent { reason } => reason,
                    AgentReservationReconciliationEvidence::Present { reason } => {
                        return Err(AgentManagementError::ReconciliationPresent(reason))
                    }
                    AgentReservationReconciliationEvidence::Ambiguous { reason } => {
                        return Err(AgentManagementError::ReconciliationAmbiguous(reason))
                    }
                    AgentReservationReconciliationEvidence::Unavailable { reason } => {
                        return Err(AgentManagementError::ReconciliationUnavailable(reason))
                    }
                };

                let committed_at = now();
                let target_response = no_write(
                    &request.target_action_id,
                    "reservation_reconciled_no_write",
                    &format!(
                        "root reconciliation {} proved the legacy pre-effect reservation absent",
                        request.action_id
                    ),
                );
                let target = state
                    .actions
                    .get_mut(&request.target_action_id)
                    .ok_or(AgentManagementError::InvalidStore)?;
                target.phase_sequence = target
                    .phase_sequence
                    .checked_add(1)
                    .filter(|value| *value <= crate::role_revision::MAX_JSON_SAFE_INTEGER)
                    .ok_or(AgentManagementError::Conflict("phase_sequence_overflow"))?;
                target.phase = AgentActionPhase::NoWrite;
                target.response = Some(target_response.clone());
                target.updated_at = committed_at.clone();
                let target = target.clone();

                let phase_event_id = format!(
                    "agent-management:{}:phase:{}",
                    target.action_id, target.phase_sequence
                );
                let phase_event = AgentManagementPhaseEvent {
                    event_id: phase_event_id.clone(),
                    action_id: target.action_id.clone(),
                    project_id: target.project_id.clone(),
                    operation: target.operation,
                    phase: AgentActionPhase::NoWrite,
                    phase_sequence: target.phase_sequence,
                    committed_at: committed_at.clone(),
                    presentation_owner_cutex_session_id: target.caller_cutex_session.clone(),
                    subject_cutex_session_id: None,
                    subject_agent_name: target.reserved_agent_name.clone(),
                    predecessor_cutex_session_id: None,
                    successor_cutex_session_id: None,
                    replace_policy: None,
                    rotation_mode: None,
                    authority_epoch: (target.operation == AgentOperationKind::DirectorRotate)
                        .then_some(request.expected_authority_epoch),
                };
                if state
                    .phase_events
                    .insert(phase_event_id, phase_event.clone())
                    .is_some()
                {
                    return Err(AgentManagementError::InvalidStore);
                }

                let reconciliation_event_id = format!(
                    "agent-management:{}:reservation-reconciliation",
                    request.action_id
                );
                let reconciliation_event = AgentReservationReconciliationEvent {
                    event_id: reconciliation_event_id.clone(),
                    action_id: request.action_id.clone(),
                    request_sha256: digest.clone(),
                    project_id: request.project_id.clone(),
                    target_action_id: request.target_action_id.clone(),
                    target_request_sha256: target.request_sha256.clone(),
                    reserved_agent_name: request.expected_reserved_agent_name.clone(),
                    reserved_agent_cwd: request.expected_reserved_agent_cwd.clone(),
                    previous_phase: AgentActionPhase::OwnerActionRequired,
                    terminal_phase: AgentActionPhase::NoWrite,
                    absence_evidence,
                    committed_at: committed_at.clone(),
                };
                if state
                    .reservation_reconciliation_events
                    .insert(reconciliation_event_id.clone(), reconciliation_event)
                    .is_some()
                {
                    return Err(AgentManagementError::InvalidStore);
                }
                let receipt = AgentReservationReconciliationReceipt {
                    schema: AgentReservationReconciliationReceiptSchema::V1,
                    action_id: request.action_id.clone(),
                    request_sha256: digest.clone(),
                    project_id: request.project_id.clone(),
                    target_action_id: request.target_action_id.clone(),
                    target_request_sha256: target.request_sha256,
                    reserved_agent_name: request.expected_reserved_agent_name.clone(),
                    reserved_agent_cwd: request.expected_reserved_agent_cwd.clone(),
                    reconciled_at: committed_at,
                    reconciliation_event_id,
                    target_response,
                };
                state
                    .reservation_reconciliation_receipts
                    .insert(request.action_id.clone(), receipt.clone());
                Ok((state, (receipt, false, Some(phase_event)), true))
            })?;
        if let Some(event) = phase_event.as_ref() {
            self.notify_phase(event);
        }
        Ok((receipt, replayed))
    }

    pub fn execute(
        &self,
        invocation: &AgentManagementInvocation,
        request: &AgentManagementRequest,
        lifecycle: &dyn AgentLifecycle,
    ) -> AgentManagementResponse {
        let response = self.execute_with_lifecycle_locks(invocation, request, lifecycle);
        // Provider/session locks are gone before asks settlement.
        self.settle_soft_archive_for_request(request, lifecycle);
        response
    }

    fn execute_with_lifecycle_locks(
        &self,
        invocation: &AgentManagementInvocation,
        request: &AgentManagementRequest,
        lifecycle: &dyn AgentLifecycle,
    ) -> AgentManagementResponse {
        let _execution = match provider_execution_lock().lock() {
            Ok(execution) => execution,
            Err(_) => {
                return no_write(
                    &request.action_id,
                    "persistence_unavailable",
                    "Agent Management execution lock is unavailable",
                )
            }
        };
        let _mutation = match self.store.lock_mutations() {
            Ok(mutation) => mutation,
            Err(error) => return error_response(&request.action_id, error),
        };
        if let Err(error) = request.validate() {
            return error_response(&request.action_id, error);
        }
        let digest = match request_sha256(request) {
            Ok(digest) => digest,
            Err(error) => return error_response(&request.action_id, error),
        };
        let request = match self.authorize_request(invocation, request, &digest) {
            Ok(request) => request,
            Err(error) => return error_response(&request.action_id, error),
        };
        match self.store.snapshot() {
            Ok(state) if state.bootstrap_intents.contains_key(&request.action_id) => {
                let intent = &state.bootstrap_intents[&request.action_id];
                if intent.request != *request.request
                    || intent.director != invocation.caller_cutex_session
                {
                    return no_write(
                        &request.action_id,
                        "bootstrap_intent_conflict",
                        "exact reviewed action and Director required",
                    );
                }
                if state
                    .actions
                    .get(&request.action_id)
                    .is_none_or(|a| a.response.is_none())
                {
                    if state
                        .actions
                        .get(&request.action_id)
                        .is_none_or(|a| a.phase == AgentActionPhase::Prepared)
                        && intent.expires_at_unix <= chrono::Utc::now().timestamp()
                    {
                        return no_write(
                            &request.action_id,
                            "bootstrap_intent_expired",
                            "review expired before lifecycle effects",
                        );
                    }
                    if let Err(error) = intent
                        .validate_authority(&state)
                        .and_then(|_| intent.validate_evidence())
                    {
                        return no_write(
                            &request.action_id,
                            "bootstrap_intent_stale",
                            &error.to_string(),
                        );
                    }
                }
            }
            Err(error) => return error_response(&request.action_id, error),
            Ok(_) if request.operation.bootstrap_intent().is_some() => {
                return no_write(
                    &request.action_id,
                    "bootstrap_intent_missing",
                    "Human-reviewed intent is absent; no default launch permitted",
                );
            }
            _ => {}
        }
        let historical_continuation =
            match self.authorize_historical_bootstrap_continuation(&request, lifecycle) {
                Ok(continuation) => continuation,
                Err(response) => return response,
            };
        if let Some(response) = self.migration_precheck(&request, lifecycle) {
            return response;
        }
        let action = match self.begin_action(invocation, &request, &digest, historical_continuation)
        {
            Ok(BeginAction::Replay(response)) => return response,
            Ok(BeginAction::Execute(action)) => action,
            Err(error) => return error_response(&request.action_id, error),
        };
        match self.execute_started(invocation, &request, &digest, action, lifecycle) {
            Ok(response) => response,
            Err(error) => {
                if matches!(error, AgentManagementError::OwnerActionRequired(_)) {
                    self.owner_action_required(&request, &error.to_string())
                        .unwrap_or_else(|store_error| {
                            error_response(&request.action_id, store_error)
                        })
                } else {
                    self.terminalize_pre_effect_no_write(&request, error)
                }
            }
        }
    }

    /// r219: `director_rotate` was removed. A rotation an older release left
    /// unfinished (no terminal response) is finished here without any further
    /// effect: no Director seat transfer, no session change, no message. Both
    /// outcomes carry code `director_rotate_removed`: a rotation that provably
    /// never left `prepared` closes as no-write (its successor name / cwd
    /// reservation is released); any other ends in `failure` with an
    /// owner-visible failure record. Idempotent; returns how many it finished.
    pub fn finish_removed_director_rotations(&self) -> Result<usize, AgentManagementError> {
        fn pending(state: &AgentManagementSnapshot) -> Vec<AgentActionId> {
            state
                .actions
                .values()
                .filter(|action| {
                    action.operation == AgentOperationKind::DirectorRotate
                        && action.response.is_none()
                })
                .map(|action| action.action_id.clone())
                .collect()
        }
        if pending(&self.store.snapshot()?).is_empty() {
            return Ok(0);
        }
        let _execution = provider_execution_lock()
            .lock()
            .map_err(|_| AgentManagementError::PersistenceUnavailable)?;
        let _mutation = self.store.lock_mutations()?;
        let events = self.store.with_state(true, |mut state| {
            let mut events = Vec::new();
            for action_id in pending(&state) {
                events.push(finish_removed_director_rotation(&mut state, &action_id)?);
            }
            let write = !events.is_empty();
            Ok((state, events, write))
        })?;
        for event in &events {
            self.notify_phase(event);
        }
        Ok(events.len())
    }

    /// A legacy cute-alden Agent cannot start, so Online/Restart refuse with
    /// `migration_required` before recording an action or comparing the
    /// recovery spec (legacy records often lack later fields such as
    /// `sandbox_mode`). Existing actions keep their exact replay semantics.
    fn migration_precheck(
        &self,
        request: &AuthorizedAgentManagementRequest<'_>,
        lifecycle: &dyn AgentLifecycle,
    ) -> Option<AgentManagementResponse> {
        let (AgentOperation::Online { cutex_session_id }
        | AgentOperation::Restart { cutex_session_id }) = &request.operation
        else {
            return None;
        };
        if self
            .store
            .snapshot()
            .ok()?
            .actions
            .contains_key(&request.action_id)
        {
            return None;
        }
        // Only for an Agent this Director may operate; others keep the
        // ordinary ownership refusal.
        self.active_agent(&request.project_id, cutex_session_id).ok()?;
        let detail = lifecycle.migration_required(cutex_session_id)?;
        Some(no_write(
            &request.action_id,
            "migration_required",
            &format!(
                "{detail} The owner must migrate or archive this Agent; a Director cannot start it."
            ),
        ))
    }

    #[allow(clippy::result_large_err)] // The typed fence response is returned directly, never copied.
    fn authorize_historical_bootstrap_continuation(
        &self,
        request: &AuthorizedAgentManagementRequest<'_>,
        lifecycle: &dyn AgentLifecycle,
    ) -> Result<HistoricalBootstrapContinuation, AgentManagementResponse> {
        let snapshot = self
            .store
            .snapshot()
            .map_err(|error| error_response(&request.action_id, error))?;
        if snapshot.bootstrap_intents.contains_key(&request.action_id) {
            return Ok(HistoricalBootstrapContinuation::None);
        }
        let Some(action) = snapshot.actions.get(&request.action_id) else {
            return Ok(HistoricalBootstrapContinuation::None);
        };
        if let Some(pids) = offline_scope_timeout_candidate(action) {
            let AgentOperation::Offline { cutex_session_id } = &request.operation else {
                return Ok(HistoricalBootstrapContinuation::None);
            };
            let fail = |error: LifecycleFailure| {
                reconciliation_fence_response(
                    action,
                    "unavailable",
                    &format!("{}: {}", error.code, error.detail),
                )
            };
            let agent = self
                .active_agent(&request.project_id, cutex_session_id)
                .map_err(|error| {
                    reconciliation_fence_response(action, "ambiguous", &error.to_string())
                })?;
            let before = lifecycle.observe(cutex_session_id).map_err(fail)?;
            validate_managed_observation_identity(&agent, &before).map_err(|error| {
                reconciliation_fence_response(action, "ambiguous", &error.to_string())
            })?;
            let expected = match action.historical_runtime_occurrence_fence.clone() {
                Some(fence) => fence,
                None => lifecycle
                    .capture_offline_occurrence(cutex_session_id, Some((&action.created_at, &pids)))
                    .map_err(fail)?
                    .ok_or_else(|| {
                        reconciliation_fence_response(
                            action,
                            "unavailable",
                            "exact original scope timeout occurrence is unavailable",
                        )
                    })?,
            };
            // Preserve the original occurrence before clearing it. The existing
            // field is readable by older binaries; no store schema is added.
            self.store
                .with_state(true, |mut state| {
                    let current = state
                        .actions
                        .get_mut(&request.action_id)
                        .ok_or(AgentManagementError::InvalidStore)?;
                    if current.historical_runtime_occurrence_fence.as_ref() == Some(&expected) {
                        return Ok((state, (), false));
                    }
                    current.historical_runtime_occurrence_fence = Some(expected.clone());
                    Ok((state, (), true))
                })
                .map_err(|error| error_response(&request.action_id, error))?;
            let absent = lifecycle
                .reconcile_offline_scope_timeout(cutex_session_id, &expected)
                .map_err(fail)?;
            if !absent.is_proven_absent()
                || absent.runtime_generation != expected.runtime_generation
            {
                return Err(reconciliation_fence_response(
                    action,
                    "ambiguous",
                    "scope timeout reconciliation returned a changed or live occurrence",
                ));
            }
            return Ok(HistoricalBootstrapContinuation::RetryLifecycleAfterOffline(
                absent,
            ));
        }
        if legacy_offline_revision_conflict_candidate(action) {
            let cutex_session_id = match &request.operation {
                AgentOperation::Offline { cutex_session_id }
                | AgentOperation::Restart { cutex_session_id }
                | AgentOperation::Close { cutex_session_id } => cutex_session_id,
                _ => return Ok(HistoricalBootstrapContinuation::None),
            };
            let agent = self
                .active_agent(&request.project_id, cutex_session_id)
                .map_err(|error| {
                    reconciliation_fence_response(action, "ambiguous", &error.to_string())
                })?;
            let before = lifecycle.observe(cutex_session_id).map_err(|error| {
                reconciliation_fence_response(
                    action,
                    "unavailable",
                    &format!("{}: {}", error.code, error.detail),
                )
            })?;
            validate_managed_observation_identity(&agent, &before).map_err(|error| {
                reconciliation_fence_response(action, "ambiguous", &error.to_string())
            })?;
            let reconciliation = lifecycle
                .reconcile_historical_runtime_occurrence(cutex_session_id)
                .unwrap_or_else(
                    |error| HistoricalRuntimeOccurrenceReconciliation::Unavailable {
                        reason: format!("{}: {}", error.code, error.detail),
                    },
                );
            return match reconciliation {
                HistoricalRuntimeOccurrenceReconciliation::ProvenAbsent { fence, .. }
                    if fence.is_proven_absent()
                        && before.runtime_generation == fence.runtime_generation
                        && before.runtime_agent_ids.is_empty()
                        && before.agent_bus_endpoint_ids.is_empty()
                        && !before.app_server_runtime =>
                {
                    Ok(HistoricalBootstrapContinuation::RetryLifecycleAfterOffline(
                        fence,
                    ))
                }
                HistoricalRuntimeOccurrenceReconciliation::ProvenAbsent { .. } => {
                    Err(reconciliation_fence_response(
                        action,
                        "ambiguous",
                        "provider absence proof conflicts with the durable runtime observation",
                    ))
                }
                HistoricalRuntimeOccurrenceReconciliation::Present { reason } => {
                    Err(reconciliation_fence_response(action, "present", &reason))
                }
                HistoricalRuntimeOccurrenceReconciliation::Ambiguous { reason } => {
                    Err(reconciliation_fence_response(action, "ambiguous", &reason))
                }
                HistoricalRuntimeOccurrenceReconciliation::Unavailable { reason } => Err(
                    reconciliation_fence_response(action, "unavailable", &reason),
                ),
            };
        }
        let AgentOperation::Create { spec, .. } = &request.operation else {
            return Ok(HistoricalBootstrapContinuation::None);
        };
        if legacy_pre_sid_retry_candidate(action) {
            let reconciliation = lifecycle
                .reconcile_pre_sid_bootstrap(spec, &action.created_at, &action.updated_at)
                .unwrap_or_else(|error| NativeBootstrapReconciliation::Unavailable {
                    reason: format!("{}: {}", error.code, error.detail),
                });
            return match reconciliation {
                NativeBootstrapReconciliation::ProvenAbsent { .. } => {
                    Ok(HistoricalBootstrapContinuation::RetryProvenAbsent)
                }
                NativeBootstrapReconciliation::Present { reason } => {
                    Err(reconciliation_fence_response(action, "present", &reason))
                }
                NativeBootstrapReconciliation::Ambiguous { reason } => {
                    Err(reconciliation_fence_response(action, "ambiguous", &reason))
                }
                NativeBootstrapReconciliation::Unavailable { reason } => Err(
                    reconciliation_fence_response(action, "unavailable", &reason),
                ),
            };
        }
        let interrupted = interrupted_bootstrap_candidate(action);
        if !interrupted && !legacy_ambiguous_sid_recovery_candidate(action) {
            return Ok(HistoricalBootstrapContinuation::None);
        }
        let Some((started_at, failed_at)) = latest_native_bootstrap_window(&snapshot, action)
        else {
            return Err(reconciliation_fence_response(
                action,
                "unavailable",
                "the most recent native bootstrap attempt window is unavailable",
            ));
        };
        let reconciliation = if interrupted {
            lifecycle.reconcile_interrupted_native_bootstrap(spec, &started_at, &failed_at)
        } else {
            lifecycle.reconcile_ambiguous_native_bootstrap(spec, &started_at, &failed_at)
        }
        .unwrap_or_else(|error| NativeBootstrapIdentityReconciliation::Unavailable {
            reason: format!("{}: {}", error.code, error.detail),
        });
        match reconciliation {
            NativeBootstrapIdentityReconciliation::Exact {
                native_session_id,
                reason,
            } => match uuid::Uuid::parse_str(&native_session_id) {
                Ok(native_session_id) => Ok(HistoricalBootstrapContinuation::CaptureExactSid(
                    native_session_id.to_string(),
                )),
                Err(_) => Err(reconciliation_fence_response(
                    action,
                    "ambiguous",
                    &format!("reconciled native session identity is invalid: {reason}"),
                )),
            },
            NativeBootstrapIdentityReconciliation::Absent { .. } if interrupted => {
                Ok(HistoricalBootstrapContinuation::RetryProvenAbsent)
            }
            NativeBootstrapIdentityReconciliation::Absent { reason } => {
                Err(reconciliation_fence_response(action, "absent", &reason))
            }
            NativeBootstrapIdentityReconciliation::Ambiguous { reason } => {
                Err(reconciliation_fence_response(action, "ambiguous", &reason))
            }
            NativeBootstrapIdentityReconciliation::Unavailable { reason } => Err(
                reconciliation_fence_response(action, "unavailable", &reason),
            ),
        }
    }

    fn authorize_request<'a>(
        &self,
        invocation: &AgentManagementInvocation,
        request: &'a AgentManagementRequest,
        digest: &Sha256,
    ) -> Result<AuthorizedAgentManagementRequest<'a>, AgentManagementError> {
        let snapshot = self.store.snapshot()?;
        if let Some(existing) = snapshot.actions.get(&request.action_id) {
            if existing.caller_cutex_session != invocation.caller_cutex_session {
                return Err(AgentManagementError::NotAuthorizedDirector);
            }
            if &existing.request_sha256 != digest {
                return Err(AgentManagementError::Conflict("action_id_payload_conflict"));
            }
            // An exact immutable receipt remains replayable by its original
            // authenticated caller after live project authority moved on.
            // This grants no new action authority.
            if existing.response.is_none()
                || legacy_pre_sid_retry_candidate(existing)
                || legacy_ambiguous_sid_recovery_candidate(existing)
                || legacy_offline_revision_conflict_candidate(existing)
                || offline_scope_timeout_candidate(existing).is_some()
            {
                let role = authorize_operation(
                    &snapshot,
                    &invocation.caller_cutex_session,
                    &existing.project_id,
                    &request.operation,
                )
                .map_err(|error| {
                    if error == AgentManagementError::ProjectNotAuthorized {
                        AgentManagementError::NotAuthorizedDirector
                    } else {
                        error
                    }
                })?;
                return Ok(AuthorizedAgentManagementRequest {
                    project_id: existing.project_id.clone(),
                    role,
                    request,
                });
            }
            return Ok(AuthorizedAgentManagementRequest {
                project_id: existing.project_id.clone(),
                role: AgentManagementRole::ReceiptReplay,
                request,
            });
        }
        let (project_id, role) =
            authorize_new_request(&snapshot, &invocation.caller_cutex_session, request)?;
        Ok(AuthorizedAgentManagementRequest {
            role,
            project_id,
            request,
        })
    }

    fn begin_action(
        &self,
        invocation: &AgentManagementInvocation,
        request: &AuthorizedAgentManagementRequest<'_>,
        digest: &Sha256,
        historical_continuation: HistoricalBootstrapContinuation,
    ) -> Result<BeginAction, AgentManagementError> {
        let mut released_events = Vec::new();
        self.store
            .with_state(true, |mut state| {
                if state
                    .durable_import_actions
                    .contains_key(&request.action_id)
                    || state.authority_receipts.contains_key(&request.action_id)
                    || state
                        .human_management_operator_actions
                        .contains_key(&request.action_id)
                    || state
                        .human_management_project_mutations
                        .contains_key(&request.action_id)
                    || state
                        .legacy_director_ownership_import_receipts
                        .contains_key(&request.action_id)
                    || state
                        .reservation_reconciliation_receipts
                        .contains_key(&request.action_id)
                {
                    return Err(AgentManagementError::Conflict("action_id_domain_conflict"));
                }
                if let Some(existing) = state.actions.get(&request.action_id).cloned() {
                    if &existing.request_sha256 != digest {
                        return Err(AgentManagementError::Conflict("action_id_payload_conflict"));
                    }
                    if existing.caller_cutex_session != invocation.caller_cutex_session
                        || existing.project_id != request.project_id
                    {
                        return Err(AgentManagementError::Conflict("action_identity_conflict"));
                    }
                    if historical_continuation == HistoricalBootstrapContinuation::RetryProvenAbsent
                        && (legacy_pre_sid_retry_candidate(&existing)
                            || interrupted_bootstrap_candidate(&existing))
                    {
                        let action = state
                            .actions
                            .get_mut(&request.action_id)
                            .ok_or(AgentManagementError::InvalidStore)?;
                        action.response = None;
                        action.native_bootstrap_retryable = true;
                        let (action, event) = Self::record_phase_event(
                            &mut state,
                            request,
                            AgentActionPhase::NativeBootstrapPending,
                            true,
                        )?;
                        return Ok((state, (BeginAction::Execute(action), Some(event)), true));
                    }
                    if let HistoricalBootstrapContinuation::RetryLifecycleAfterOffline(fence) =
                        &historical_continuation
                    {
                        if !legacy_offline_revision_conflict_candidate(&existing)
                            && offline_scope_timeout_candidate(&existing).is_none()
                        {
                            return Ok(match existing.response {
                                Some(response) => {
                                    (state, (BeginAction::Replay(response), None), false)
                                }
                                None => (state, (BeginAction::Execute(existing), None), false),
                            });
                        }
                        let action = state
                            .actions
                            .get_mut(&request.action_id)
                            .ok_or(AgentManagementError::InvalidStore)?;
                        action.response = None;
                        action.historical_runtime_occurrence_fence = Some(fence.clone());
                        let (action, event) = Self::record_phase_event(
                            &mut state,
                            request,
                            AgentActionPhase::Prepared,
                            true,
                        )?;
                        return Ok((state, (BeginAction::Execute(action), Some(event)), true));
                    }
                    if let HistoricalBootstrapContinuation::CaptureExactSid(native_session_id) =
                        &historical_continuation
                    {
                        if legacy_ambiguous_sid_recovery_candidate(&existing)
                            || interrupted_bootstrap_candidate(&existing)
                        {
                            let action = state
                                .actions
                                .get_mut(&request.action_id)
                                .ok_or(AgentManagementError::InvalidStore)?;
                            action.response = None;
                            action.native_bootstrap_retryable = false;
                            action.known_native_session_id = Some(native_session_id.clone());
                            let (action, event) = Self::record_phase_event(
                                &mut state,
                                request,
                                AgentActionPhase::NativeSessionCaptured,
                                true,
                            )?;
                            return Ok((state, (BeginAction::Execute(action), Some(event)), true));
                        }
                    }
                    return Ok(match existing.response {
                        Some(response) => (state, (BeginAction::Replay(response), None), false),
                        None => (state, (BeginAction::Execute(existing), None), false),
                    });
                }
                let authority = state
                    .projects
                    .get(&request.project_id)
                    .ok_or(AgentManagementError::Unauthorized)?;
                let role = authorize_operation(
                    &state,
                    &invocation.caller_cutex_session,
                    &request.project_id,
                    &request.operation,
                )?;
                if role != request.role || authority.project_id != request.project_id {
                    return Err(AgentManagementError::Unauthorized);
                }
                let reservation = operation_reservation(request);
                if let Some(spec) = reservation {
                    let colliding = state
                        .actions
                        .values()
                        .filter(|action| {
                            !matches!(
                                action.phase,
                                AgentActionPhase::Complete | AgentActionPhase::NoWrite
                            ) && action.reserved_agent_name.as_deref()
                                == Some(spec.name.as_str())
                        })
                        .cloned()
                        .collect::<Vec<_>>();
                    if let Some(blocking) = colliding.iter().find(|action| {
                        action.project_id != request.project_id
                            || !pre_effect_preflight_reservation(&state, action)
                    }) {
                        return Err(AgentManagementError::ConflictDetail(
                            unresolved_reservation_detail(blocking, spec),
                        ));
                    }
                    // Every collider is a same-project Create rejected by
                    // the pure preflight, or a Replace stopped before its
                    // successor existed: release it.
                    for stale in &colliding {
                        released_events.push(release_pre_effect_reservation(
                            &mut state,
                            stale,
                            &request.action_id,
                        )?);
                    }
                }
                let timestamp = now();
                let external_message_id = operation_message(request)
                    .map(|_| format!("agent-management:{}:start", request.action_id.as_str()));
                let action = AgentActionRecord {
                    action_id: request.action_id.clone(),
                    request_sha256: digest.clone(),
                    operation: request.operation.kind(),
                    project_id: request.project_id.clone(),
                    caller_cutex_session: invocation.caller_cutex_session.clone(),
                    created_by: matches!(request.operation.kind(), AgentOperationKind::Create | AgentOperationKind::Replace)
                        .then(|| captured_creator(&invocation.caller_cutex_session, role)).flatten(),
                    phase: AgentActionPhase::Prepared,
                    phase_sequence: 0,
                    reserved_agent_name: reservation.map(|spec| spec.name.clone()),
                    reserved_agent_cwd: reservation.map(|spec| spec.cwd.clone()),
                    known_successor_cutex_session: None,
                    known_native_session_id: None,
                    native_bootstrap_retryable: false,
                    historical_runtime_occurrence_fence: None,
                    external_message_id,
                    response: None,
                    created_at: timestamp.clone(),
                    updated_at: timestamp,
                };
                state
                    .actions
                    .insert(request.action_id.clone(), action.clone());
                let (action, event) = Self::record_phase_event(
                    &mut state,
                    request,
                    AgentActionPhase::Prepared,
                    true,
                )?;
                Ok((state, (BeginAction::Execute(action), Some(event)), true))
            })
            .map(|(begin, event)| {
                for released in &released_events {
                    self.notify_phase(released);
                }
                if let Some(event) = event.as_ref() {
                    self.notify_phase(event);
                }
                begin
            })
    }

    fn execute_started(
        &self,
        invocation: &AgentManagementInvocation,
        request: &AuthorizedAgentManagementRequest<'_>,
        digest: &Sha256,
        action: AgentActionRecord,
        lifecycle: &dyn AgentLifecycle,
    ) -> Result<AgentManagementResponse, AgentManagementError> {
        match &request.operation {
            AgentOperation::Create {
                spec,
                start_mode,
                frozen_message,
                ..
            } => {
                let created = self.create_steps(
                    invocation,
                    request,
                    action,
                    spec,
                    *start_mode,
                    frozen_message.as_deref(),
                    lifecycle,
                )?;
                self.complete(
                    request,
                    digest,
                    AgentManagementResult::Created {
                        agent: created.agent,
                        observation: created.observation,
                        message_id: created.message_id,
                    },
                )
            }
            AgentOperation::QueryManaged => {
                let snapshot = self.current_name_snapshot()?;
                let authority = snapshot
                    .projects
                    .get(&request.project_id)
                    .cloned()
                    .ok_or(AgentManagementError::Unauthorized)?;
                let agents = snapshot
                    .agents
                    .values()
                    .filter(|agent| {
                        super::projects::current_project_id(&snapshot, agent).as_ref()
                            == Some(&request.project_id)
                            && agent.retired_at.is_none()
                            && !snapshot
                                .reversible_archive_projection
                                .get(&agent.cutex_session_id)
                                .copied()
                                .unwrap_or(false)
                    })
                    .cloned()
                    .collect();
                let operators = operator_roster(&snapshot, &request.project_id);
                self.complete(
                    request,
                    digest,
                    AgentManagementResult::QueryManaged {
                        authority,
                        agents,
                        operators,
                    },
                )
            }
            AgentOperation::Online { cutex_session_id } => {
                let (agent, observation) =
                    self.online_existing(request, cutex_session_id, lifecycle)?;
                self.complete(
                    request,
                    digest,
                    AgentManagementResult::Lifecycle { agent, observation },
                )
            }
            AgentOperation::Offline { cutex_session_id } => {
                // Validate target ownership before recording any occurrence.
                self.active_agent(&request.project_id, cutex_session_id)?;
                if action.historical_runtime_occurrence_fence.is_none() {
                    if let Some(fence) = lifecycle
                        .capture_offline_occurrence(cutex_session_id, None)
                        .map_err(lifecycle_error)?
                    {
                        self.store.with_state(true, |mut state| {
                            state
                                .actions
                                .get_mut(&request.action_id)
                                .ok_or(AgentManagementError::InvalidStore)?
                                .historical_runtime_occurrence_fence = Some(fence);
                            Ok((state, (), true))
                        })?;
                    }
                }
                let (agent, observation) = self.offline_existing(
                    request,
                    cutex_session_id,
                    action.historical_runtime_occurrence_fence.as_ref(),
                    lifecycle,
                )?;
                self.complete(
                    request,
                    digest,
                    AgentManagementResult::Lifecycle { agent, observation },
                )
            }
            AgentOperation::Restart { cutex_session_id } => {
                let agent = self.active_agent(&request.project_id, cutex_session_id)?;
                let (before, after) = match action.historical_runtime_occurrence_fence.as_ref() {
                    Some(fence) => lifecycle
                        .restart_managed_if_occurrence(
                            &RuntimeExecutionPermit {
                                provider: self,
                                action: &request.action_id,
                                id: cutex_session_id,
                            },
                            cutex_session_id,
                            fence,
                        )
                        .map_err(lifecycle_error)?,
                    None => {
                        recover_runtime_for_agent(&agent, lifecycle)?;
                        let before = lifecycle
                            .observe(cutex_session_id)
                            .map_err(lifecycle_error)?;
                        lifecycle
                            .offline(cutex_session_id)
                            .map_err(lifecycle_error)?;
                        lifecycle
                            .online_managed(
                                &RuntimeExecutionPermit {
                                    provider: self,
                                    action: &request.action_id,
                                    id: cutex_session_id,
                                },
                                cutex_session_id,
                            )
                            .map_err(lifecycle_error)?;
                        let after = lifecycle
                            .observe(cutex_session_id)
                            .map_err(lifecycle_error)?;
                        (before, after)
                    }
                };
                validate_ready(&agent, &after)?;
                validate_identity_preserved(&before, &after)?;
                if after.runtime_generation <= before.runtime_generation {
                    return Err(AgentManagementError::OwnerActionRequired(
                        "restart did not advance the runtime occurrence".to_string(),
                    ));
                }
                self.complete(
                    request,
                    digest,
                    AgentManagementResult::Lifecycle {
                        agent,
                        observation: after,
                    },
                )
            }
            AgentOperation::Close { cutex_session_id } => {
                let (agent, observation) = self.close_existing(
                    request,
                    cutex_session_id,
                    action.historical_runtime_occurrence_fence.as_ref(),
                    lifecycle,
                )?;
                self.complete(
                    request,
                    digest,
                    AgentManagementResult::Lifecycle { agent, observation },
                )
            }
            AgentOperation::Replace {
                predecessor_cutex_session_id,
                policy,
                successor,
                start_mode,
                frozen_message,
                ..
            } => {
                let mut action = action;
                if action.phase == AgentActionPhase::Prepared {
                    self.active_agent(&request.project_id, predecessor_cutex_session_id)?;
                    self.reject_active_collision_except(
                        successor,
                        (*policy == AgentReplacePolicy::CloseBeforeCreate)
                            .then_some(predecessor_cutex_session_id),
                        &action.caller_cutex_session,
                    )?;
                }
                if *policy == AgentReplacePolicy::CloseBeforeCreate
                    && matches!(
                        action.phase,
                        AgentActionPhase::Prepared | AgentActionPhase::PredecessorClosing
                    )
                {
                    if action.phase == AgentActionPhase::Prepared {
                        self.set_phase(request, AgentActionPhase::PredecessorClosing)?;
                    }
                    if let Some(response) = self.close_or_reconcile_expected_predecessor(
                        request,
                        predecessor_cutex_session_id,
                        lifecycle,
                    )? {
                        return Ok(response);
                    }
                    action = self.set_phase(request, AgentActionPhase::PredecessorClosed)?;
                }
                let created = if *policy == AgentReplacePolicy::CloseAfterReady
                    && matches!(
                        action.phase,
                        AgentActionPhase::PredecessorClosing | AgentActionPhase::PredecessorClosed
                    ) {
                    self.recover_created_agent(request, &action, *start_mode, lifecycle)?
                } else {
                    self.create_steps(
                        invocation,
                        request,
                        action.clone(),
                        successor,
                        *start_mode,
                        frozen_message.as_deref(),
                        lifecycle,
                    )?
                };
                if *policy == AgentReplacePolicy::CloseAfterReady {
                    if action.phase != AgentActionPhase::PredecessorClosed {
                        if action.phase != AgentActionPhase::PredecessorClosing {
                            self.set_phase(request, AgentActionPhase::PredecessorClosing)?;
                        }
                        if let Some(response) = self.close_or_reconcile_expected_predecessor(
                            request,
                            predecessor_cutex_session_id,
                            lifecycle,
                        )? {
                            return Ok(response);
                        }
                        self.set_phase(request, AgentActionPhase::PredecessorClosed)?;
                    }
                }
                self.complete(
                    request,
                    digest,
                    AgentManagementResult::Replaced {
                        predecessor_cutex_session_id: predecessor_cutex_session_id.clone(),
                        successor: created.agent,
                        observation: created.observation,
                        message_id: created.message_id,
                    },
                )
            }
            AgentOperation::GrantOperator {
                operator_cutex_session_id,
                expected_grant_revision,
            } => self.complete_operator_grant(
                invocation,
                request,
                digest,
                operator_cutex_session_id,
                *expected_grant_revision,
            ),
            AgentOperation::RevokeOperator {
                operator_cutex_session_id,
                expected_grant_revision,
            } => self.complete_operator_revoke(
                invocation,
                request,
                digest,
                operator_cutex_session_id,
                *expected_grant_revision,
            ),
            // r219: refused by `validate` before any action is recorded.
            AgentOperation::DirectorRotate { .. } => {
                Err(AgentManagementError::Removed(DIRECTOR_ROTATE_REMOVED))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn create_steps(
        &self,
        invocation: &AgentManagementInvocation,
        request: &AuthorizedAgentManagementRequest<'_>,
        mut action: AgentActionRecord,
        spec: &ManagedAgentSpec,
        start_mode: AgentStartMode,
        message: Option<&str>,
        lifecycle: &dyn AgentLifecycle,
    ) -> Result<CreatedAgent, AgentManagementError> {
        let bootstrap_state = self.store.snapshot()?;
        let permit = bootstrap_state
            .bootstrap_intents
            .get(&request.action_id)
            .map(|intent| BootstrapExecutionPermit {
                provider: self,
                intent,
            });
        let effective_spec;
        let spec = if permit.is_none() {
            // The effective spec is a pure preflight: a definite rejection
            // happens before any lifecycle effect, so it terminalizes as
            // NoWrite and releases the name reservation instead of
            // parking it behind owner action.
            effective_spec = lifecycle
                .effective_create_spec(spec)
                .map_err(preflight_lifecycle_error)?;
            &effective_spec
        } else { spec };
        // Only this invocation's positive native ACK skips the recovery read.
        // A journaled known ID alone (including an uncertain failure) cannot.
        let mut bootstrap_acknowledged = false;
        if action.known_successor_cutex_session.is_none()
            && action.known_native_session_id.is_none()
            && matches!(
                action.phase,
                AgentActionPhase::Prepared | AgentActionPhase::PredecessorClosed
            )
        {
            self.reject_active_collision(spec, &action.caller_cutex_session)?;
            lifecycle
                .prepare_private_cwd(spec)
                .map_err(lifecycle_error)?;
            action = self.set_phase(request, AgentActionPhase::PrivateCwdReady)?;
        }
        if action.known_native_session_id.is_none()
            && matches!(
                action.phase,
                AgentActionPhase::PrivateCwdReady | AgentActionPhase::NativeBootstrapPending
            )
            && (action.phase == AgentActionPhase::PrivateCwdReady
                || action.native_bootstrap_retryable)
        {
            if action.phase == AgentActionPhase::PrivateCwdReady {
                self.set_phase(request, AgentActionPhase::NativeBootstrapPending)?;
            } else {
                self.consume_native_bootstrap_retry(request)?;
            }
            if permit
                .as_ref()
                .is_some_and(|p| p.intent.expires_at_unix <= chrono::Utc::now().timestamp())
            {
                return Err(AgentManagementError::OwnerActionRequired(
                    "bootstrap intent expired; no native create attempted".into(),
                ));
            }
            match if let Some(permit) = &permit {
                lifecycle.bootstrap_reviewed(permit)
            } else {
                lifecycle.bootstrap_native_with_creator(spec, action.created_by.as_ref())
            } {
                Ok(native_session_id) => {
                    action = self.capture_native_session(request, &native_session_id)?;
                    bootstrap_acknowledged = permit.is_some();
                    #[cfg(feature = "stock-launch-test-hook")]
                    if permit.is_some()
                        && bootstrap_test_fault(
                            "CUTEX_BOOTSTRAP_TEST_POST_ID_ACTION",
                            &request.action_id,
                        )
                    {
                        // Abrupt creator exit: no Rust destructors and no
                        // host coredump service or large core artifact.
                        std::process::exit(86);
                    }
                }
                Err(error) => {
                    if let Some(native_session_id) = error.known_native_session_id.as_deref() {
                        self.capture_native_session(request, native_session_id)?;
                    } else if !error.outcome_unknown {
                        self.mark_native_bootstrap_retryable(request)?;
                    }
                    return Err(lifecycle_error(error));
                }
            }
        } else if action.known_native_session_id.is_none()
            && action.phase == AgentActionPhase::NativeBootstrapPending
        {
            return Err(AgentManagementError::OwnerActionRequired(
                "native bootstrap outcome is unknown; no second Agent was created".to_string(),
            ));
        }
        let native_session_id = action.known_native_session_id.clone().ok_or_else(|| {
            AgentManagementError::OwnerActionRequired(
                "captured native session identity is unavailable".to_string(),
            )
        })?;
        if action.known_successor_cutex_session.is_none() {
            let cutex_session_id = if let Some(permit) = &permit {
                if !bootstrap_acknowledged {
                    lifecycle
                        .confirm_reviewed_bootstrap(permit, &native_session_id)
                        .map_err(lifecycle_error)?;
                }
                lifecycle.adopt_reviewed(permit, &native_session_id)
            } else {
                lifecycle.adopt_native_with_creator(&native_session_id, spec, action.created_by.as_ref())
            }
            .map_err(lifecycle_error)?;
            action = self.capture_adopted_agent(
                invocation,
                request,
                &cutex_session_id,
                &native_session_id,
                spec,
            )?;
        }
        let cutex_session_id = action
            .known_successor_cutex_session
            .clone()
            .ok_or(AgentManagementError::InvalidStore)?;
        if matches!(action.phase, AgentActionPhase::Adopted) {
            if permit.is_none() {
                lifecycle
                    .configure(&cutex_session_id, &native_session_id, spec)
                    .map_err(lifecycle_error)?;
            }
            action = self.set_phase(request, AgentActionPhase::Configured)?;
        }
        if matches!(action.phase, AgentActionPhase::Configured) {
            if let Some(permit) = &permit {
                lifecycle
                    .online_reviewed(permit, &cutex_session_id)
                    .map_err(lifecycle_error)?;
            } else {
                let agent = self.active_agent(&request.project_id, &cutex_session_id)?;
                recover_runtime_for_agent(&agent, lifecycle)?;
                lifecycle
                    .online_managed(
                        &RuntimeExecutionPermit {
                            provider: self,
                            action: &request.action_id,
                            id: &cutex_session_id,
                        },
                        &cutex_session_id,
                    )
                    .map_err(lifecycle_error)?;
            }
            action = self.set_phase(request, AgentActionPhase::Online)?;
        } else if matches!(action.phase, AgentActionPhase::Online) {
            if let Some(permit) = &permit {
                lifecycle
                    .online_reviewed(permit, &cutex_session_id)
                    .map_err(lifecycle_error)?;
            } else {
                let agent = self.active_agent(&request.project_id, &cutex_session_id)?;
                recover_runtime_for_agent(&agent, lifecycle)?;
                lifecycle
                    .online_managed(
                        &RuntimeExecutionPermit {
                            provider: self,
                            action: &request.action_id,
                            id: &cutex_session_id,
                        },
                        &cutex_session_id,
                    )
                    .map_err(lifecycle_error)?;
            }
        }
        let observation = lifecycle
            .observe(&cutex_session_id)
            .map_err(lifecycle_error)?;
        let agent = self.active_agent(&request.project_id, &cutex_session_id)?;
        if let Some(permit) = &permit {
            validate_ready_with_groups(&agent, &observation, &permit.intent.runtime_groups)?;
        } else {
            validate_ready(&agent, &observation)?;
        }
        if matches!(action.phase, AgentActionPhase::Online) {
            action = self.set_phase(request, AgentActionPhase::Ready)?;
        }
        let message_id = if start_mode == AgentStartMode::CustomMessage {
            let exact_message = message.ok_or(AgentManagementError::InvalidRequest(
                "custom_message_requires_frozen_message",
            ))?;
            let external_message_id = action
                .external_message_id
                .clone()
                .ok_or(AgentManagementError::InvalidStore)?;
            if action.phase == AgentActionPhase::Ready {
                action = self.set_phase(request, AgentActionPhase::MessagePending)?;
            }
            if action.phase == AgentActionPhase::MessagePending {
                let primary_director = self
                    .store
                    .snapshot()?
                    .projects
                    .get(&request.project_id)
                    .map(|authority| authority.authorized_director_session.clone())
                    .ok_or(AgentManagementError::ProjectNotAuthorized)?;
                let metadata = AgentManagementMessageMetadata {
                    schema: AgentManagementSchema::V1,
                    requested_by_director: primary_director,
                    requested_by_operator: (request.role == AgentManagementRole::Operator)
                        .then(|| invocation.caller_cutex_session.clone()),
                };
                let system = crate::agent_bus::identity::agent_management_system_principal();
                let message_id = lifecycle
                    .send_message(
                        &system,
                        &metadata,
                        &cutex_session_id,
                        exact_message,
                        &external_message_id,
                    )
                    .map_err(lifecycle_error)?;
                self.set_phase(request, AgentActionPhase::MessageQueued)?;
                Some(message_id)
            } else if action.phase == AgentActionPhase::MessageQueued {
                Some(external_message_id)
            } else {
                return Err(AgentManagementError::InvalidStore);
            }
        } else {
            None
        };
        Ok(CreatedAgent {
            agent,
            observation,
            message_id,
        })
    }

    fn recover_created_agent(
        &self,
        request: &AuthorizedAgentManagementRequest<'_>,
        action: &AgentActionRecord,
        start_mode: AgentStartMode,
        lifecycle: &dyn AgentLifecycle,
    ) -> Result<CreatedAgent, AgentManagementError> {
        let successor = action
            .known_successor_cutex_session
            .as_ref()
            .ok_or(AgentManagementError::InvalidStore)?;
        let agent = self.active_agent(&request.project_id, successor)?;
        let state = self.store.snapshot()?;
        let intent = state.bootstrap_intents.get(&request.action_id);
        if let Some(intent) = intent {
            // A creator restart loses its in-process connection, not the
            // captured successor. Reconcile the original runtime receipt and
            // owned occurrence through the same sealed launch permit. This
            // never enters generic online or creates another native thread.
            lifecycle
                .online_reviewed(
                    &BootstrapExecutionPermit {
                        provider: self,
                        intent,
                    },
                    successor,
                )
                .map_err(lifecycle_error)?;
        }
        let observation = lifecycle.observe(successor).map_err(lifecycle_error)?;
        if let Some(intent) = intent {
            validate_ready_with_groups(&agent, &observation, &intent.runtime_groups)?;
        } else {
            validate_ready(&agent, &observation)?;
        }
        Ok(CreatedAgent {
            agent,
            observation,
            message_id: (start_mode == AgentStartMode::CustomMessage)
                .then(|| action.external_message_id.clone())
                .flatten(),
        })
    }

    fn active_agent(
        &self,
        project_id: &ProjectId,
        cutex_session_id: &CutexSessionId,
    ) -> Result<ManagedAgentRecord, AgentManagementError> {
        let snapshot = self.current_name_snapshot()?;
        if snapshot
            .reversible_archive_projection
            .get(cutex_session_id)
            .copied()
            .unwrap_or(false)
        {
            return Err(AgentManagementError::OwnerActionRequired(
                "Agent is reversibly archived; explicitly Restore before ordinary lifecycle".into(),
            ));
        }
        let agent = snapshot
            .agents
            .get(cutex_session_id)
            .cloned()
            .ok_or_else(|| {
                AgentManagementError::OwnerActionRequired(
                    "Agent has no explicit Agent Management ownership record".to_string(),
                )
            })?;
        if super::projects::current_project_id(&snapshot, &agent).as_ref() != Some(project_id) {
            return Err(AgentManagementError::Unauthorized);
        }
        if agent.retired_at.is_some() {
            return Err(AgentManagementError::OwnerActionRequired(
                "retired Agent history is immutable and outside ordinary lifecycle".to_string(),
            ));
        }
        Ok(agent)
    }

    fn online_existing(
        &self,
        request: &AuthorizedAgentManagementRequest<'_>,
        cutex_session_id: &CutexSessionId,
        lifecycle: &dyn AgentLifecycle,
    ) -> Result<(ManagedAgentRecord, AgentRuntimeObservation), AgentManagementError> {
        let agent = self.active_agent(&request.project_id, cutex_session_id)?;
        recover_runtime_for_agent(&agent, lifecycle)?;
        let before = lifecycle
            .observe(cutex_session_id)
            .map_err(lifecycle_error)?;
        lifecycle
            .online_managed(
                &RuntimeExecutionPermit {
                    provider: self,
                    action: &request.action_id,
                    id: cutex_session_id,
                },
                cutex_session_id,
            )
            .map_err(lifecycle_error)?;
        let after = lifecycle
            .observe(cutex_session_id)
            .map_err(lifecycle_error)?;
        validate_identity_preserved(&before, &after)?;
        validate_ready(&agent, &after)?;
        Ok((agent, after))
    }

    fn offline_existing(
        &self,
        request: &AuthorizedAgentManagementRequest<'_>,
        cutex_session_id: &CutexSessionId,
        occurrence_fence: Option<&RuntimeOccurrenceFence>,
        lifecycle: &dyn AgentLifecycle,
    ) -> Result<(ManagedAgentRecord, AgentRuntimeObservation), AgentManagementError> {
        let agent = self.active_agent(&request.project_id, cutex_session_id)?;
        let before = lifecycle
            .observe(cutex_session_id)
            .map_err(lifecycle_error)?;
        match occurrence_fence {
            Some(fence) => lifecycle
                .offline_if_occurrence(cutex_session_id, fence)
                .map_err(lifecycle_error)?,
            None => lifecycle
                .offline(cutex_session_id)
                .map_err(lifecycle_error)?,
        }
        let after = lifecycle
            .observe(cutex_session_id)
            .map_err(lifecycle_error)?;
        validate_identity_preserved(&before, &after)?;
        if after.app_server_runtime
            || !after.runtime_agent_ids.is_empty()
            || !after.agent_bus_endpoint_ids.is_empty()
        {
            return Err(AgentManagementError::OwnerActionRequired(
                "offline Agent still exposes a runtime endpoint".to_string(),
            ));
        }
        Ok((agent, after))
    }

    fn close_existing(
        &self,
        request: &AuthorizedAgentManagementRequest<'_>,
        cutex_session_id: &CutexSessionId,
        occurrence_fence: Option<&RuntimeOccurrenceFence>,
        lifecycle: &dyn AgentLifecycle,
    ) -> Result<(ManagedAgentRecord, AgentRuntimeObservation), AgentManagementError> {
        self.soft_archive_existing(request, cutex_session_id, occurrence_fence, lifecycle)
    }
    /// Completes one predecessor close owned by the exact durable action. The
    /// `PredecessorClosing` intent is committed before this seam is entered.
    /// On replay, an exact already-retired predecessor may be adopted as the
    /// outcome of that intent only after its complete managed identity and
    /// endpoint-free retired state are proven.
    fn close_or_reconcile_expected_predecessor(
        &self,
        request: &AuthorizedAgentManagementRequest<'_>,
        cutex_session_id: &CutexSessionId,
        lifecycle: &dyn AgentLifecycle,
    ) -> Result<Option<AgentManagementResponse>, AgentManagementError> {
        if !operation_closes_expected_predecessor(request, cutex_session_id) {
            return Err(AgentManagementError::InvalidStore);
        }
        let snapshot = self.store.snapshot()?;
        let action = snapshot
            .actions
            .get(&request.action_id)
            .ok_or(AgentManagementError::InvalidStore)?;
        if action.phase != AgentActionPhase::PredecessorClosing
            || action.project_id != request.project_id
        {
            return Err(AgentManagementError::InvalidStore);
        }
        let expected = snapshot.agents.get(cutex_session_id).cloned().ok_or(
            AgentManagementError::OwnerActionRequired(
                "expected predecessor ownership record is unavailable".to_string(),
            ),
        )?;
        if super::projects::current_project_id(&snapshot, &expected).as_ref()
            != Some(&request.project_id)
        {
            return Err(AgentManagementError::Unauthorized);
        }
        let observation = lifecycle
            .observe(cutex_session_id)
            .map_err(lifecycle_error)?;
        self.validate_current_managed_observation(&expected, &observation)?;
        let externally_closed = !observation.active
            && !observation.app_server_runtime
            && observation.runtime_agent_ids.is_empty()
            && observation.agent_bus_endpoint_ids.is_empty();

        if expected.retired_at.is_some() {
            return if externally_closed {
                Ok(None)
            } else {
                Err(AgentManagementError::OwnerActionRequired(
                    "retired predecessor still exposes active lifecycle state".to_string(),
                ))
            };
        }
        // A session-only legacy close without our durable soft intent cannot
        // be reclassified as reversible, nor may an agent finish a permanent
        // tombstone. Leave the ambiguous historical action for the owner.
        if externally_closed && !self.has_soft_archive_intent(request, cutex_session_id)? {
            return Err(AgentManagementError::Conflict("legacy_close_requires_owner_review"));
        }
        self.soft_archive_existing(request, cutex_session_id, None, lifecycle)?;
        if let Some(response) = self.inject_process_loss_after_predecessor_close(request) {
            return Ok(Some(response));
        }
        Ok(None)
    }
    fn validate_current_managed_observation(
        &self,
        agent: &ManagedAgentRecord,
        observation: &AgentRuntimeObservation,
    ) -> Result<(), AgentManagementError> {
        let state = self.store.snapshot()?;
        let mut intents = state.actions.values().filter_map(|action| {
            (action.known_successor_cutex_session.as_ref() == Some(&agent.cutex_session_id))
                .then(|| state.bootstrap_intents.get(&action.action_id))
                .flatten()
        });
        if let Some(intent) = intents.next() {
            if intents.next().is_some() {
                return Err(AgentManagementError::InvalidStore);
            }
            validate_managed_observation_groups(agent, observation, &intent.runtime_groups)
        } else {
            validate_managed_observation_identity(agent, observation)
        }
    }

    fn reject_active_collision(
        &self,
        spec: &ManagedAgentSpec,
        caller: &CutexSessionId,
    ) -> Result<(), AgentManagementError> {
        self.reject_active_collision_except(spec, None, caller)
    }

    /// Active Agent names are unique; the cwd is not (2026-10-11: a Director
    /// and its members share the project workspace, and the owner's agents
    /// share one cwd). Every agent still gets its own native thread, rollout
    /// and durable session, keyed by the native thread ID.
    fn reject_active_collision_except(
        &self,
        spec: &ManagedAgentSpec,
        allowed_predecessor: Option<&CutexSessionId>,
        caller: &CutexSessionId,
    ) -> Result<(), AgentManagementError> {
        let snapshot = self.current_name_snapshot()?;
        let holder = snapshot.agents.values().find(|agent| {
            agent.retired_at.is_none()
                && snapshot.reversible_archive_projection.get(&agent.cutex_session_id) != Some(&true)
                && allowed_predecessor != Some(&agent.cutex_session_id)
                && agent.spec.name == spec.name
        });
        match holder {
            Some(holder) => Err(AgentManagementError::ConflictDetail(
                active_name_taken_detail(&snapshot, holder, caller),
            )),
            None => Ok(()),
        }
    }

    fn terminalize_pre_effect_no_write(
        &self,
        request: &AuthorizedAgentManagementRequest<'_>,
        error: AgentManagementError,
    ) -> AgentManagementResponse {
        let deterministic = matches!(
            &error,
            AgentManagementError::InvalidRequest(_)
                | AgentManagementError::InvalidRequestDetail(_)
                | AgentManagementError::Unauthorized
                | AgentManagementError::NotFound(_)
                | AgentManagementError::Conflict(_)
                | AgentManagementError::ConflictDetail(_)
        );
        let response = error_response(&request.action_id, error);
        let recorded = self.store.with_state(true, |mut state| {
            let action = state
                .actions
                .get_mut(&request.action_id)
                .ok_or(AgentManagementError::InvalidStore)?;
            if deterministic {
                if action.phase != AgentActionPhase::Prepared
                    || action.known_native_session_id.is_some()
                    || action.known_successor_cutex_session.is_some()
                {
                    return Ok((state, (response.clone(), None), false));
                }
                action.response = Some(response.clone());
            }
            let (_, event) = Self::record_phase_event(
                &mut state,
                request,
                if deterministic {
                    AgentActionPhase::NoWrite
                } else {
                    AgentActionPhase::Failure
                },
                deterministic,
            )?;
            Ok((state, (response.clone(), Some(event)), true))
        });
        match recorded {
            Ok((response, event)) => {
                if let Some(event) = event.as_ref() {
                    self.notify_phase(event);
                }
                response
            }
            Err(_) => response,
        }
    }

    fn capture_native_session(
        &self,
        request: &AuthorizedAgentManagementRequest<'_>,
        native_session_id: &str,
    ) -> Result<AgentActionRecord, AgentManagementError> {
        if native_session_id.trim().is_empty() {
            return Err(AgentManagementError::OwnerActionRequired(
                "native bootstrap returned an empty session identity".to_string(),
            ));
        }
        self.update_action(request, AgentActionPhase::NativeSessionCaptured, |action| {
            if let Some(existing) = action.known_native_session_id.as_deref() {
                if existing != native_session_id {
                    return Err(AgentManagementError::OwnerActionRequired(
                        "conflicting native session identities were observed".to_string(),
                    ));
                }
            }
            action.known_native_session_id = Some(native_session_id.to_string());
            Ok(())
        })
    }

    fn mark_native_bootstrap_retryable(
        &self,
        request: &AuthorizedAgentManagementRequest<'_>,
    ) -> Result<(), AgentManagementError> {
        self.store.with_state(true, |mut state| {
            let action = state
                .actions
                .get_mut(&request.action_id)
                .ok_or(AgentManagementError::InvalidStore)?;
            if action.phase != AgentActionPhase::NativeBootstrapPending
                || action.known_native_session_id.is_some()
                || action.known_successor_cutex_session.is_some()
            {
                return Err(AgentManagementError::InvalidStore);
            }
            action.native_bootstrap_retryable = true;
            action.updated_at = now();
            Ok((state, (), true))
        })
    }

    fn consume_native_bootstrap_retry(
        &self,
        request: &AuthorizedAgentManagementRequest<'_>,
    ) -> Result<AgentActionRecord, AgentManagementError> {
        self.update_action(
            request,
            AgentActionPhase::NativeBootstrapPending,
            |action| {
                if !action.native_bootstrap_retryable
                    || action.known_native_session_id.is_some()
                    || action.known_successor_cutex_session.is_some()
                {
                    return Err(AgentManagementError::InvalidStore);
                }
                action.native_bootstrap_retryable = false;
                Ok(())
            },
        )
    }

    fn capture_adopted_agent(
        &self,
        invocation: &AgentManagementInvocation,
        request: &AuthorizedAgentManagementRequest<'_>,
        cutex_session_id: &CutexSessionId,
        native_session_id: &str,
        spec: &ManagedAgentSpec,
    ) -> Result<AgentActionRecord, AgentManagementError> {
        let (action, event) = self.store.with_state(true, |mut state| {
            let action = state
                .actions
                .get_mut(&request.action_id)
                .ok_or(AgentManagementError::InvalidStore)?;
            if action.known_native_session_id.as_deref() != Some(native_session_id) {
                return Err(AgentManagementError::InvalidStore);
            }
            if let Some(existing) = action.known_successor_cutex_session.as_ref() {
                if existing != cutex_session_id {
                    return Err(AgentManagementError::OwnerActionRequired(
                        "native session resolved to conflicting durable Agents".to_string(),
                    ));
                }
            }
            if let Some(existing) = state.agents.get(cutex_session_id) {
                if existing.project_id.as_ref() != Some(&request.project_id)
                    || existing.native_session_id != native_session_id
                    || existing.spec != *spec
                {
                    return Err(AgentManagementError::OwnerActionRequired(
                        "durable Agent has conflicting explicit ownership".to_string(),
                    ));
                }
            } else {
                let primary_director = state
                    .projects
                    .get(&request.project_id)
                    .map(|authority| authority.authorized_director_session.clone())
                    .ok_or(AgentManagementError::InvalidStore)?;
                state.agents.insert(
                    cutex_session_id.clone(),
                    ManagedAgentRecord {
                        project_id: Some(request.project_id.clone()),
                        created_by_director_session: Some(primary_director),
                        created_by_operator_session: (request.role
                            == AgentManagementRole::Operator)
                            .then(|| invocation.caller_cutex_session.clone()),
                        cutex_session_id: cutex_session_id.clone(),
                        native_session_id: native_session_id.to_string(),
                        spec: spec.clone(),
                        created_at: now(),
                        retired_at: None,
                    },
                );
            }
            let action = state
                .actions
                .get_mut(&request.action_id)
                .ok_or(AgentManagementError::InvalidStore)?;
            action.known_successor_cutex_session = Some(cutex_session_id.clone());
            let (action, event) =
                Self::record_phase_event(&mut state, request, AgentActionPhase::Adopted, true)?;
            Ok((state, (action, event), true))
        })?;
        self.notify_phase(&event);
        Ok(action)
    }

    fn set_phase(
        &self,
        request: &AuthorizedAgentManagementRequest<'_>,
        phase: AgentActionPhase,
    ) -> Result<AgentActionRecord, AgentManagementError> {
        self.update_action(request, phase, |_| Ok(()))
    }

    fn update_action(
        &self,
        request: &AuthorizedAgentManagementRequest<'_>,
        phase: AgentActionPhase,
        update: impl FnOnce(&mut AgentActionRecord) -> Result<(), AgentManagementError>,
    ) -> Result<AgentActionRecord, AgentManagementError> {
        let (action, event) = self.store.with_state(true, |mut state| {
            update(
                state
                    .actions
                    .get_mut(&request.action_id)
                    .ok_or(AgentManagementError::InvalidStore)?,
            )?;
            let (action, event) = Self::record_phase_event(&mut state, request, phase, true)?;
            Ok((state, (action, event), true))
        })?;
        self.notify_phase(&event);
        Ok(action)
    }

    fn record_phase_event(
        state: &mut AgentManagementSnapshot,
        request: &AuthorizedAgentManagementRequest<'_>,
        phase: AgentActionPhase,
        update_current_phase: bool,
    ) -> Result<(AgentActionRecord, AgentManagementPhaseEvent), AgentManagementError> {
        let committed_at = now();
        let action = state
            .actions
            .get_mut(&request.action_id)
            .ok_or(AgentManagementError::InvalidStore)?;
        action.phase_sequence = action
            .phase_sequence
            .checked_add(1)
            .filter(|sequence| *sequence <= crate::role_revision::MAX_JSON_SAFE_INTEGER)
            .ok_or(AgentManagementError::Conflict("phase_sequence_overflow"))?;
        if update_current_phase {
            action.phase = phase;
        }
        action.updated_at = committed_at.clone();
        let action = action.clone();

        let (predecessor, replace_policy) = match &request.operation {
            AgentOperation::Replace {
                predecessor_cutex_session_id,
                policy,
                ..
            } => (Some(predecessor_cutex_session_id.clone()), Some(*policy)),
            _ => (None, None),
        };
        let successor = action.known_successor_cutex_session.clone();
        let predecessor_phase = matches!(
            phase,
            AgentActionPhase::PredecessorClosing | AgentActionPhase::PredecessorClosed
        );
        let (subject_cutex_session_id, subject_agent_name) = match &request.operation {
            AgentOperation::Create { spec, .. } => (successor.clone(), Some(spec.name.clone())),
            // r219: a removed `director_rotate` never records phases.
            AgentOperation::QueryManaged | AgentOperation::DirectorRotate { .. } => (None, None),
            AgentOperation::GrantOperator {
                operator_cutex_session_id,
                ..
            }
            | AgentOperation::RevokeOperator {
                operator_cutex_session_id,
                ..
            } => (Some(operator_cutex_session_id.clone()), None),
            AgentOperation::Online { cutex_session_id }
            | AgentOperation::Offline { cutex_session_id }
            | AgentOperation::Restart { cutex_session_id }
            | AgentOperation::Close { cutex_session_id } => (Some(cutex_session_id.clone()), None),
            AgentOperation::Replace {
                predecessor_cutex_session_id,
                successor: successor_spec,
                ..
            } => {
                if predecessor_phase {
                    (Some(predecessor_cutex_session_id.clone()), None)
                } else {
                    (successor.clone(), Some(successor_spec.name.clone()))
                }
            }
        };
        let presentation_owner_cutex_session_id = action.caller_cutex_session.clone();
        let event_id = format!(
            "agent-management:{}:phase:{}",
            request.action_id.as_str(),
            action.phase_sequence
        );
        let event = AgentManagementPhaseEvent {
            event_id: event_id.clone(),
            action_id: request.action_id.clone(),
            project_id: request.project_id.clone(),
            operation: request.operation.kind(),
            phase,
            phase_sequence: action.phase_sequence,
            committed_at,
            presentation_owner_cutex_session_id,
            subject_cutex_session_id,
            subject_agent_name,
            predecessor_cutex_session_id: predecessor,
            successor_cutex_session_id: successor,
            replace_policy,
            rotation_mode: None,
            authority_epoch: None,
        };
        if state.phase_events.insert(event_id, event.clone()).is_some() {
            return Err(AgentManagementError::InvalidStore);
        }
        Ok((action, event))
    }

    fn complete(
        &self,
        request: &AuthorizedAgentManagementRequest<'_>,
        digest: &Sha256,
        result: AgentManagementResult,
    ) -> Result<AgentManagementResponse, AgentManagementError> {
        let archive_kind = self.completed_archive_disposition(request.request)?;
        let receipt = AgentManagementReceipt {
            schema: AgentManagementReceiptSchema::V1,
            action_id: request.action_id.clone(),
            request_sha256: digest.clone(),
            operation: request.operation.kind(),
            project_id: request.project_id.clone(),
            completed_at: now(),
            archive_kind,
            restorable: archive_kind.map(|kind| kind == AgentArchiveKind::Soft),
            result,
        };
        let response = AgentManagementResponse {
            schema: AgentManagementSchema::V1,
            action_id: request.action_id.clone(),
            outcome: AgentManagementOutcome::Complete { receipt },
        };
        let (response, event) = self.store.with_state(true, |mut state| {
            let action = state
                .actions
                .get_mut(&request.action_id)
                .ok_or(AgentManagementError::InvalidStore)?;
            action.response = Some(response.clone());
            let (_, event) =
                Self::record_phase_event(&mut state, request, AgentActionPhase::Complete, true)?;
            Ok((state, (response.clone(), event), true))
        })?;
        self.notify_phase(&event);
        Ok(response)
    }

    fn complete_operator_grant(
        &self,
        invocation: &AgentManagementInvocation,
        request: &AuthorizedAgentManagementRequest<'_>,
        digest: &Sha256,
        operator: &CutexSessionId,
        expected_revision: u64,
    ) -> Result<AgentManagementResponse, AgentManagementError> {
        if request.role != AgentManagementRole::PrimaryDirector {
            return Err(AgentManagementError::NotAuthorizedDirector);
        }
        let (response, event) = self.store.with_state(true, |mut state| {
            let authority = state
                .projects
                .get(&request.project_id)
                .cloned()
                .ok_or(AgentManagementError::ProjectNotAuthorized)?;
            if authority.authorized_director_session != invocation.caller_cutex_session {
                return Err(AgentManagementError::NotAuthorizedDirector);
            }
            if operator == &authority.authorized_director_session {
                return Err(AgentManagementError::Conflict(
                    "primary_director_cannot_be_operator",
                ));
            }
            let agent = state
                .agents
                .get(operator)
                .filter(|agent| {
                    super::projects::current_project_id(&state, agent).as_ref()
                        == Some(&request.project_id)
                        && agent.retired_at.is_none()
                        && !archive_operations::archived_caller(&state, &agent.cutex_session_id)
                })
                .ok_or(AgentManagementError::Conflict(
                    "operator_must_be_active_managed_agent",
                ))?;
            if &agent.cutex_session_id != operator {
                return Err(AgentManagementError::InvalidStore);
            }
            let current_revision = operator_grant_revision(&state, &request.project_id);
            if current_revision != expected_revision {
                return Err(AgentManagementError::Conflict(
                    "operator_grant_revision_conflict",
                ));
            }
            if state
                .operator_grants
                .get(&request.project_id)
                .is_some_and(|grants| grants.contains_key(operator))
            {
                return Err(AgentManagementError::Conflict("operator_already_granted"));
            }
            let revision = next_operator_grant_revision(current_revision)?;
            let committed_at = now();
            let grant = AgentOperatorGrant {
                project_id: request.project_id.clone(),
                operator_cutex_session_id: operator.clone(),
                grant_revision: revision,
                granted_at: committed_at.clone(),
                granted_by_primary_director_session: invocation.caller_cutex_session.clone(),
                performed_by_human_management: false,
            };
            state
                .operator_grants
                .entry(request.project_id.clone())
                .or_default()
                .insert(operator.clone(), grant.clone());
            state
                .operator_grant_revisions
                .insert(request.project_id.clone(), revision);
            insert_operator_audit_event(
                &mut state,
                request,
                operator,
                AgentOperatorAuditKind::Granted,
                current_revision,
                revision,
                &invocation.caller_cutex_session,
                committed_at,
            )?;
            let roster = operator_roster(&state, &request.project_id);
            let response = complete_response(
                request,
                digest,
                AgentManagementResult::OperatorGranted { grant, roster },
            );
            state
                .actions
                .get_mut(&request.action_id)
                .ok_or(AgentManagementError::InvalidStore)?
                .response = Some(response.clone());
            let (_, event) =
                Self::record_phase_event(&mut state, request, AgentActionPhase::Complete, true)?;
            Ok((state, (response, event), true))
        })?;
        self.notify_phase(&event);
        Ok(response)
    }

    fn complete_operator_revoke(
        &self,
        invocation: &AgentManagementInvocation,
        request: &AuthorizedAgentManagementRequest<'_>,
        digest: &Sha256,
        operator: &CutexSessionId,
        expected_revision: u64,
    ) -> Result<AgentManagementResponse, AgentManagementError> {
        if request.role != AgentManagementRole::PrimaryDirector {
            return Err(AgentManagementError::NotAuthorizedDirector);
        }
        let (response, event) = self.store.with_state(true, |mut state| {
            let authority = state
                .projects
                .get(&request.project_id)
                .cloned()
                .ok_or(AgentManagementError::ProjectNotAuthorized)?;
            if authority.authorized_director_session != invocation.caller_cutex_session {
                return Err(AgentManagementError::NotAuthorizedDirector);
            }
            let current_revision = operator_grant_revision(&state, &request.project_id);
            if current_revision != expected_revision {
                return Err(AgentManagementError::Conflict(
                    "operator_grant_revision_conflict",
                ));
            }
            let removed = state
                .operator_grants
                .get_mut(&request.project_id)
                .and_then(|grants| grants.remove(operator))
                .ok_or(AgentManagementError::Conflict("operator_not_granted"))?;
            if removed.project_id != request.project_id
                || removed.operator_cutex_session_id != *operator
            {
                return Err(AgentManagementError::InvalidStore);
            }
            let revision = next_operator_grant_revision(current_revision)?;
            let committed_at = now();
            state
                .operator_grant_revisions
                .insert(request.project_id.clone(), revision);
            insert_operator_audit_event(
                &mut state,
                request,
                operator,
                AgentOperatorAuditKind::Revoked,
                current_revision,
                revision,
                &invocation.caller_cutex_session,
                committed_at,
            )?;
            let roster = operator_roster(&state, &request.project_id);
            let response = complete_response(
                request,
                digest,
                AgentManagementResult::OperatorRevoked {
                    operator_cutex_session_id: operator.clone(),
                    roster,
                },
            );
            state
                .actions
                .get_mut(&request.action_id)
                .ok_or(AgentManagementError::InvalidStore)?
                .response = Some(response.clone());
            let (_, event) =
                Self::record_phase_event(&mut state, request, AgentActionPhase::Complete, true)?;
            Ok((state, (response, event), true))
        })?;
        self.notify_phase(&event);
        Ok(response)
    }

    fn owner_action_required(
        &self,
        request: &AuthorizedAgentManagementRequest<'_>,
        detail: &str,
    ) -> Result<AgentManagementResponse, AgentManagementError> {
        let (response, phase_event) = self.store.with_state(true, |mut state| {
            let action = state
                .actions
                .get_mut(&request.action_id)
                .ok_or(AgentManagementError::InvalidStore)?;
            if let Some(response) = action.response.clone() {
                return Ok((state, (response, None), false));
            }
            let resumable_continuation =
                action.known_native_session_id.is_some() || action.native_bootstrap_retryable;
            let event_id = format!("agent-management:{}:failure", request.action_id.as_str());
            let authority = state.projects.get(&request.project_id).cloned();
            let route_to_director_session = authority
                .as_ref()
                .map(|authority| authority.authorized_director_session.clone());
            let target_cutex_session_id = match &request.operation {
                AgentOperation::Online { cutex_session_id }
                | AgentOperation::Offline { cutex_session_id }
                | AgentOperation::Restart { cutex_session_id }
                | AgentOperation::Close { cutex_session_id } => Some(cutex_session_id.clone()),
                AgentOperation::Create { .. }
                | AgentOperation::QueryManaged
                | AgentOperation::Replace { .. }
                | AgentOperation::GrantOperator { .. }
                | AgentOperation::RevokeOperator { .. }
                | AgentOperation::DirectorRotate { .. } => None,
            };
            let failure = AgentManagementFailureEvent {
                schema: AgentManagementFailureSchema::V1,
                event_id: event_id.clone(),
                action_id: request.action_id.clone(),
                project_id: request.project_id.clone(),
                operation: request.operation.kind(),
                code: "owner_action_required".to_string(),
                detail: detail.to_string(),
                routing_status: if route_to_director_session.is_some() {
                    FailureRoutingStatus::Routable
                } else {
                    FailureRoutingStatus::Unrouted
                },
                route_to_director_session,
                target_cutex_session_id,
                created_at: now(),
            };
            let response = AgentManagementResponse {
                schema: AgentManagementSchema::V1,
                action_id: request.action_id.clone(),
                outcome: AgentManagementOutcome::OwnerActionRequired {
                    failure: failure.clone(),
                },
            };
            state.failure_events.insert(event_id, failure);
            let action = state
                .actions
                .get_mut(&request.action_id)
                .ok_or(AgentManagementError::InvalidStore)?;
            if !resumable_continuation {
                action.response = Some(response.clone());
            }
            let (_, event) = Self::record_phase_event(
                &mut state,
                request,
                AgentActionPhase::OwnerActionRequired,
                !resumable_continuation,
            )?;
            Ok((state, (response, Some(event)), true))
        })?;
        if let Some(event) = phase_event.as_ref() {
            self.notify_phase(event);
        }
        Ok(response)
    }
}

struct AuthorizedAgentManagementRequest<'a> {
    project_id: ProjectId,
    role: AgentManagementRole,
    request: &'a AgentManagementRequest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentManagementRole {
    PrimaryDirector,
    Operator,
    /// Exact terminal receipts remain replayable by their original caller.
    ReceiptReplay,
}

fn captured_creator(caller: &CutexSessionId, role: AgentManagementRole) -> Option<crate::session::model::SessionCreatedBy> {
    use crate::session::model::{SessionCreatedBy, SessionCreatorRole};
    let role = match role {
        AgentManagementRole::PrimaryDirector => SessionCreatorRole::Director,
        AgentManagementRole::Operator => SessionCreatorRole::Member,
        AgentManagementRole::ReceiptReplay => return None,
    };
    Some(SessionCreatedBy::Agent { cutex_session_id: caller.clone(), role: Some(role) })
}

fn authorize_operation(
    snapshot: &AgentManagementSnapshot,
    caller: &CutexSessionId,
    project_id: &ProjectId,
    operation: &AgentOperation,
) -> Result<AgentManagementRole, AgentManagementError> {
    if snapshot
        .project_states
        .get(project_id)
        .is_some_and(|state| state.lifecycle != ProjectLifecycle::Active)
        && !matches!(operation, AgentOperation::QueryManaged)
    {
        return Err(AgentManagementError::Conflict("project_archived"));
    }
    let role = project_management_role(snapshot, caller, project_id)?;
    let authority = snapshot
        .projects
        .get(project_id)
        .ok_or(AgentManagementError::ProjectNotAuthorized)?;
    if role == AgentManagementRole::PrimaryDirector {
        return Ok(role);
    }
    if matches!(
        operation,
        AgentOperation::DirectorRotate { .. }
            | AgentOperation::GrantOperator { .. }
            | AgentOperation::RevokeOperator { .. }
    ) {
        return Err(AgentManagementError::NotAuthorizedDirector);
    }
    if let Some(target) = operation_target(operation) {
        if target == &authority.authorized_director_session {
            return Err(AgentManagementError::Conflict(
                "operator_cannot_target_primary_director",
            ));
        }
        if operation_disables_target(operation)
            && snapshot
                .operator_grants
                .get(project_id)
                .is_some_and(|grants| grants.contains_key(target))
        {
            return Err(AgentManagementError::Conflict(
                "operator_cannot_disable_privileged_operator",
            ));
        }
    }
    Ok(role)
}

/// Project and role authorization of a request whose action is not recorded
/// yet (the second half of `authorize_request`).
fn authorize_new_request(
    snapshot: &AgentManagementSnapshot,
    caller: &CutexSessionId,
    request: &AgentManagementRequest,
) -> Result<(ProjectId, AgentManagementRole), AgentManagementError> {
    let authorized_projects = snapshot
        .projects
        .values()
        .filter(|authority| project_management_role(snapshot, caller, &authority.project_id).is_ok())
        .map(|authority| authority.project_id.clone())
        .collect::<Vec<_>>();
    let project_id = match request.project_id.as_ref() {
        Some(selector) if authorized_projects.contains(selector) => selector.clone(),
        Some(_) if authorized_projects.is_empty() => {
            return Err(AgentManagementError::NotAuthorizedDirector)
        }
        Some(_) => return Err(AgentManagementError::ProjectNotAuthorized),
        None => match authorized_projects.as_slice() {
            [project_id] => project_id.clone(),
            [] => return Err(AgentManagementError::NotAuthorizedDirector),
            _ => return Err(AgentManagementError::ProjectSelectionRequired),
        },
    };
    let role = authorize_operation(snapshot, caller, &project_id, &request.operation)?;
    Ok((project_id, role))
}

/// Before a sparse resolution is bound to its action id: the caller must be
/// allowed to run `request` now (the provider's own project / role checks,
/// and for a reviewed intent its exact request and Director), so a refused
/// caller never takes another caller's action id. Read-only; the caller runs
/// it inside the store transaction that binds the resolution.
pub(crate) fn authorize_sparse_binding(
    snapshot: &AgentManagementSnapshot,
    caller: &CutexSessionId,
    request: &AgentManagementRequest,
) -> Result<(), AgentManagementError> {
    if let Some(intent) = snapshot.bootstrap_intents.get(&request.action_id) {
        if &intent.director != caller || intent.request != *request {
            return Err(AgentManagementError::Conflict("bootstrap_intent_conflict"));
        }
    } else if request.operation.bootstrap_intent().is_some() {
        return Err(AgentManagementError::Conflict("bootstrap_intent_missing"));
    }
    authorize_new_request(snapshot, caller, request).map(|_| ())
}

/// The project a new Agent Management request of `caller` lands in, chosen
/// exactly as `authorize_request` chooses it (the selector when the caller
/// manages it, else the caller's only managed project). Used for defaults of
/// sparse requests; `None` leaves the refusal to the provider.
pub(crate) fn managed_project_for(
    snapshot: &AgentManagementSnapshot,
    caller: &CutexSessionId,
    selector: Option<&ProjectId>,
) -> Option<ProjectId> {
    let authorized = snapshot
        .projects
        .values()
        .filter(|authority| project_management_role(snapshot, caller, &authority.project_id).is_ok())
        .map(|authority| authority.project_id.clone())
        .collect::<Vec<_>>();
    match selector {
        Some(selector) => authorized.contains(selector).then(|| selector.clone()),
        None => match authorized.as_slice() {
            [project_id] => Some(project_id.clone()),
            _ => None,
        },
    }
}

fn project_management_role(
    snapshot: &AgentManagementSnapshot,
    caller: &CutexSessionId,
    project_id: &ProjectId,
) -> Result<AgentManagementRole, AgentManagementError> {
    if archive_operations::archived_caller(snapshot, caller) {
        return Err(AgentManagementError::ProjectNotAuthorized);
    }
    let authority = snapshot
        .projects
        .get(project_id)
        .ok_or(AgentManagementError::ProjectNotAuthorized)?;
    if &authority.authorized_director_session == caller {
        return Ok(AgentManagementRole::PrimaryDirector);
    }
    let grant = snapshot
        .operator_grants
        .get(project_id)
        .and_then(|grants| grants.get(caller))
        .ok_or(AgentManagementError::ProjectNotAuthorized)?;
    if &grant.project_id != project_id || &grant.operator_cutex_session_id != caller {
        return Err(AgentManagementError::InvalidStore);
    }
    let agent = snapshot
        .agents
        .get(caller)
        .filter(|agent| {
            super::projects::current_project_id(snapshot, agent).as_ref() == Some(project_id)
                && agent.retired_at.is_none()
        })
        .ok_or(AgentManagementError::ProjectNotAuthorized)?;
    debug_assert_eq!(&agent.cutex_session_id, caller);
    Ok(AgentManagementRole::Operator)
}

fn operation_target(operation: &AgentOperation) -> Option<&CutexSessionId> {
    match operation {
        AgentOperation::Online { cutex_session_id }
        | AgentOperation::Offline { cutex_session_id }
        | AgentOperation::Restart { cutex_session_id }
        | AgentOperation::Close { cutex_session_id } => Some(cutex_session_id),
        AgentOperation::Replace {
            predecessor_cutex_session_id,
            ..
        } => Some(predecessor_cutex_session_id),
        _ => None,
    }
}

fn operation_disables_target(operation: &AgentOperation) -> bool {
    matches!(
        operation,
        AgentOperation::Offline { .. }
            | AgentOperation::Restart { .. }
            | AgentOperation::Close { .. }
            | AgentOperation::Replace { .. }
    )
}

impl std::ops::Deref for AuthorizedAgentManagementRequest<'_> {
    type Target = AgentManagementRequest;

    fn deref(&self) -> &Self::Target {
        self.request
    }
}

#[derive(Clone)]
#[allow(clippy::large_enum_variant)] // Internal state-machine values avoid allocation on every phase.
enum BeginAction {
    Replay(AgentManagementResponse),
    Execute(AgentActionRecord),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum HistoricalBootstrapContinuation {
    None,
    RetryProvenAbsent,
    CaptureExactSid(String),
    RetryLifecycleAfterOffline(RuntimeOccurrenceFence),
}

fn legacy_offline_revision_conflict_candidate(action: &AgentActionRecord) -> bool {
    matches!(
        action.operation,
        AgentOperationKind::Offline | AgentOperationKind::Restart | AgentOperationKind::Close
    ) && action.phase == AgentActionPhase::OwnerActionRequired
        && matches!(
            action.response.as_ref().map(|response| &response.outcome),
            Some(AgentManagementOutcome::OwnerActionRequired { failure })
                if offline_revision_conflict_detail_matches(&failure.detail)
        )
}

fn offline_scope_timeout_candidate(action: &AgentActionRecord) -> Option<Vec<u32>> {
    if action.operation != AgentOperationKind::Offline
        || action.phase != AgentActionPhase::OwnerActionRequired
    {
        return None;
    }
    let AgentManagementOutcome::OwnerActionRequired { failure } =
        &action.response.as_ref()?.outcome
    else {
        return None;
    };
    let detail = failure
        .detail
        .strip_prefix("owner_action_required: ")
        .unwrap_or(&failure.detail);
    let detail = detail
        .strip_prefix("session_offline_failed: scope_terminate_timeout,")?
        .strip_suffix(" (external outcome unknown)")?;
    let mut pids = Vec::new();
    for item in detail.split(',') {
        let (pid, result) = item.split_once(':')?;
        if !matches!(result, "terminated" | "process_already_exited") {
            return None;
        }
        let pid = pid
            .parse::<u32>()
            .ok()
            .filter(|pid| *pid != 0 && *pid <= i32::MAX as u32)?;
        if pids.contains(&pid) {
            return None;
        }
        pids.push(pid);
    }
    (!pids.is_empty()).then_some(pids)
}

fn offline_revision_conflict_detail_matches(detail: &str) -> bool {
    let detail = detail
        .strip_prefix("owner_action_required: ")
        .unwrap_or(detail);
    let Some(detail) = detail
        .strip_prefix("session_offline_failed: cutex session store revision conflict: expected ")
    else {
        return false;
    };
    let Some(detail) = detail.strip_suffix(" (external outcome unknown)") else {
        return false;
    };
    let Some((expected, current)) = detail.split_once(", current ") else {
        return false;
    };
    let Some(current) = current.strip_suffix("; reload before retrying") else {
        return false;
    };
    expected.parse::<u64>().is_ok() && current.parse::<u64>().is_ok()
}

struct CreatedAgent {
    agent: ManagedAgentRecord,
    observation: AgentRuntimeObservation,
    message_id: Option<String>,
}

fn validate_reservation_reconciliation_request(
    request: &AgentReservationReconciliationRequest,
) -> Result<(), AgentManagementError> {
    if request.action_id == request.target_action_id {
        return Err(AgentManagementError::InvalidRequest(
            "reconciliation_action_must_be_fresh",
        ));
    }
    if request.expected_phase != AgentActionPhase::OwnerActionRequired {
        return Err(AgentManagementError::InvalidRequest(
            "expected_phase_must_be_owner_action_required",
        ));
    }
    if request.expected_authority_epoch == 0
        || request.expected_authority_epoch > crate::role_revision::MAX_JSON_SAFE_INTEGER
    {
        return Err(AgentManagementError::InvalidRequest(
            "invalid_authority_epoch",
        ));
    }
    if request.expected_reserved_agent_name.trim().is_empty()
        || request.expected_reserved_agent_name.trim() != request.expected_reserved_agent_name
    {
        return Err(AgentManagementError::InvalidRequest(
            "invalid_reserved_agent_name",
        ));
    }
    let cwd = std::path::Path::new(&request.expected_reserved_agent_cwd);
    if request.expected_reserved_agent_cwd.trim() != request.expected_reserved_agent_cwd
        || !cwd.is_absolute()
        || cwd.parent().is_none()
    {
        return Err(AgentManagementError::InvalidRequest(
            "invalid_reserved_agent_cwd",
        ));
    }
    Ok(())
}

fn validate_reconciliation_target(
    state: &AgentManagementSnapshot,
    request: &AgentReservationReconciliationRequest,
    target: &AgentActionRecord,
) -> Result<(), AgentManagementError> {
    if target.project_id != request.project_id
        || target.caller_cutex_session != request.expected_authorized_director_session
        || target.request_sha256 != request.expected_target_request_sha256
        || target.phase != request.expected_phase
        || target.reserved_agent_name.as_deref()
            != Some(request.expected_reserved_agent_name.as_str())
        || target.reserved_agent_cwd.as_deref()
            != Some(request.expected_reserved_agent_cwd.as_str())
    {
        return Err(AgentManagementError::Conflict(
            "reservation_reconciliation_cas_mismatch",
        ));
    }
    if !matches!(
        target.operation,
        AgentOperationKind::Create
            | AgentOperationKind::Replace
            | AgentOperationKind::DirectorRotate
    ) {
        return Err(AgentManagementError::Conflict(
            "target_is_not_a_create_like_reservation",
        ));
    }
    if target.known_successor_cutex_session.is_some()
        || target.known_native_session_id.is_some()
        || target.native_bootstrap_retryable
        || target.historical_runtime_occurrence_fence.is_some()
    {
        return Err(AgentManagementError::Conflict(
            "target_has_post_reservation_continuation_evidence",
        ));
    }
    let failure = match target.response.as_ref() {
        Some(AgentManagementResponse {
            schema: AgentManagementSchema::V1,
            action_id,
            outcome: AgentManagementOutcome::OwnerActionRequired { failure },
        }) if action_id == &target.action_id
            && failure.action_id == target.action_id
            && failure.project_id == target.project_id
            && failure.operation == target.operation
            && failure.code == "owner_action_required" =>
        {
            failure
        }
        _ => {
            return Err(AgentManagementError::Conflict(
                "target_has_no_exact_owner_action_required_receipt",
            ))
        }
    };
    if failure.routing_status != FailureRoutingStatus::Routable
        || failure.route_to_director_session.as_ref()
            != Some(&request.expected_authorized_director_session)
        || failure.target_cutex_session_id.is_some()
    {
        return Err(AgentManagementError::Conflict(
            "target_failure_identity_is_not_exact",
        ));
    }
    if !legacy_missing_predecessor_ownership_failure(&failure.detail) {
        return Err(AgentManagementError::Conflict(
            "target_failure_class_not_allowlisted",
        ));
    }
    if state.failure_events.get(&failure.event_id) != Some(failure) {
        return Err(AgentManagementError::Conflict(
            "target_failure_journal_is_missing_or_malformed",
        ));
    }
    let phases = state
        .phase_events
        .values()
        .filter(|event| event.action_id == target.action_id)
        .collect::<Vec<_>>();
    let distinct_sequences = phases
        .iter()
        .map(|event| event.phase_sequence)
        .collect::<std::collections::BTreeSet<_>>();
    let phase_journal_valid = phases.iter().all(|event| {
        event.project_id == target.project_id
            && event.operation == target.operation
            && event.phase_sequence > 0
            && event.phase_sequence <= target.phase_sequence
            && matches!(
                event.phase,
                AgentActionPhase::Prepared | AgentActionPhase::OwnerActionRequired
            )
    }) && distinct_sequences.len() == phases.len()
        && phases.iter().any(|event| {
            event.phase == AgentActionPhase::Prepared
                && event.phase_sequence < target.phase_sequence
        })
        && phases.iter().any(|event| {
            event.phase == AgentActionPhase::OwnerActionRequired
                && event.phase_sequence == target.phase_sequence
        });
    if !phase_journal_valid {
        return Err(AgentManagementError::Conflict(
            "target_phase_journal_is_missing_malformed_or_post_effect",
        ));
    }
    Ok(())
}

fn legacy_missing_predecessor_ownership_failure(detail: &str) -> bool {
    detail
        .strip_prefix("owner_action_required: ")
        .unwrap_or(detail)
        == "Agent has no explicit Agent Management ownership record"
}

/// Identifies the narrow historical receipt that is eligible for authoritative
/// reconciliation. The receipt text does not itself authorize a retry: the
/// lifecycle provider must separately prove native/runtime absence before the
/// cached response is reopened. New attempts persist their retry classification
/// directly.
fn legacy_pre_sid_retry_candidate(action: &AgentActionRecord) -> bool {
    action.operation == AgentOperationKind::Create
        && action.phase == AgentActionPhase::OwnerActionRequired
        && action.known_native_session_id.is_none()
        && action.known_successor_cutex_session.is_none()
        && !action.native_bootstrap_retryable
        && matches!(
            action.response.as_ref().map(|response| &response.outcome),
            Some(AgentManagementOutcome::OwnerActionRequired { failure })
                if legacy_pre_sid_failure_detail_matches(&failure.detail)
        )
}

fn legacy_pre_sid_failure_detail_matches(detail: &str) -> bool {
    // One historical provider path wrapped the lifecycle detail once before
    // persisting it. Normalize only that exact known prefix; the remaining
    // text is still merely a reconciliation trigger, never absence proof.
    let detail = detail
        .strip_prefix("owner_action_required: ")
        .unwrap_or(detail);
    let detail = detail
        .strip_suffix(" (external outcome unknown)")
        .unwrap_or(detail);
    let native_session_unknown = detail
        .strip_prefix("native_session_unknown: native exec exited ")
        .and_then(|detail| detail.split_once(" without one captured SID"))
        .is_some_and(|(status, diagnostic)| {
            !status.is_empty()
                && (diagnostic.is_empty() || diagnostic.starts_with("; diagnostic: "))
        });
    native_session_unknown
        || detail
            == "native_bootstrap_output_malformed: native exec JSONL stdout line 1 is not valid JSON"
}

/// Identifies only the one historical receipt class produced after the native
/// bootstrap had already run but legacy UUID scraping could not select its SID.
/// Receipt text is a trigger for evidence reconciliation, never identity proof.
fn interrupted_bootstrap_candidate(action: &AgentActionRecord) -> bool {
    action.operation == AgentOperationKind::Create
        && action.phase == AgentActionPhase::OwnerActionRequired
        && action.known_native_session_id.is_none()
        && action.known_successor_cutex_session.is_none()
        && !action.native_bootstrap_retryable
        && matches!(action.response.as_ref().map(|r| &r.outcome),
            Some(AgentManagementOutcome::OwnerActionRequired { failure })
            if failure.detail.strip_prefix("owner_action_required: ").unwrap_or(&failure.detail)
                == "native bootstrap outcome is unknown; no second Agent was created")
}

fn legacy_ambiguous_sid_recovery_candidate(action: &AgentActionRecord) -> bool {
    action.operation == AgentOperationKind::Create
        && action.phase == AgentActionPhase::OwnerActionRequired
        && action.known_native_session_id.is_none()
        && action.known_successor_cutex_session.is_none()
        && !action.native_bootstrap_retryable
        && matches!(
            action.response.as_ref().map(|response| &response.outcome),
            Some(AgentManagementOutcome::OwnerActionRequired { failure })
                if legacy_ambiguous_sid_failure_detail_matches(&failure.detail)
        )
}

fn legacy_ambiguous_sid_failure_detail_matches(detail: &str) -> bool {
    let detail = detail
        .strip_prefix("owner_action_required: ")
        .unwrap_or(detail);
    matches!(
        detail,
        "native_session_ambiguous: native exec exposed multiple possible session identities"
            | "native_session_ambiguous: native exec exposed multiple possible session identities (external outcome unknown)"
    )
}

fn latest_native_bootstrap_window(
    snapshot: &AgentManagementSnapshot,
    action: &AgentActionRecord,
) -> Option<(crate::role_revision::Rfc3339, crate::role_revision::Rfc3339)> {
    let started_at = snapshot
        .phase_events
        .values()
        .filter(|event| {
            event.action_id == action.action_id
                && event.phase == AgentActionPhase::NativeBootstrapPending
        })
        .max_by_key(|event| event.phase_sequence)?
        .committed_at
        .clone();
    (started_at.as_str() <= action.updated_at.as_str())
        .then(|| (started_at, action.updated_at.clone()))
}

/// Returns an invocation-only projection. The historical action, response, and
/// failure event remain byte-for-byte unchanged in the durable store.
fn reconciliation_fence_response(
    action: &AgentActionRecord,
    status: &str,
    reason: &str,
) -> AgentManagementResponse {
    let Some(mut response) = action.response.clone() else {
        return no_write(
            &action.action_id,
            "invalid_store",
            "native bootstrap reconciliation did not find the immutable receipt",
        );
    };
    if let AgentManagementOutcome::OwnerActionRequired { failure } = &mut response.outcome {
        if legacy_offline_revision_conflict_candidate(action) {
            failure.event_id = format!(
                "agent-management:{}:runtime-occurrence-reconciliation:{status}",
                action.action_id
            );
            failure.code = format!("runtime_occurrence_reconciliation_{status}");
            failure.detail = format!(
                "historical lifecycle replay remains fenced ({status}): {reason}; immutable original receipt remains unchanged"
            );
            return response;
        }
        failure.event_id = format!(
            "agent-management:{}:native-bootstrap-reconciliation:{status}",
            action.action_id
        );
        failure.code = format!("native_bootstrap_reconciliation_{status}");
        failure.detail = format!(
            "historical create retry remains fenced ({status}): {reason}; immutable original receipt remains unchanged"
        );
    }
    response
}

fn operation_message<'a>(request: &'a AuthorizedAgentManagementRequest<'a>) -> Option<&'a str> {
    match &request.operation {
        AgentOperation::Create { frozen_message, .. }
        | AgentOperation::Replace { frozen_message, .. } => frozen_message.as_deref(),
        _ => None,
    }
}

fn operation_reservation<'a>(
    request: &'a AuthorizedAgentManagementRequest<'a>,
) -> Option<&'a ManagedAgentSpec> {
    match &request.operation {
        AgentOperation::Create { spec, .. } => Some(spec),
        AgentOperation::Replace { successor, .. } => Some(successor),
        AgentOperation::DirectorRotate { .. }
        | AgentOperation::QueryManaged
        | AgentOperation::Online { .. }
        | AgentOperation::Offline { .. }
        | AgentOperation::Restart { .. }
        | AgentOperation::Close { .. }
        | AgentOperation::GrantOperator { .. }
        | AgentOperation::RevokeOperator { .. } => None,
    }
}

fn operation_closes_expected_predecessor(
    request: &AuthorizedAgentManagementRequest<'_>,
    cutex_session_id: &CutexSessionId,
) -> bool {
    match &request.operation {
        AgentOperation::Replace {
            predecessor_cutex_session_id,
            policy: AgentReplacePolicy::CloseBeforeCreate | AgentReplacePolicy::CloseAfterReady,
            ..
        } => predecessor_cutex_session_id == cutex_session_id,
        _ => false,
    }
}

/// Maps a failure of a pure, pre-effect create preflight. A definite failure
/// is a deterministic rejection (terminal NoWrite, reservation released); an
/// uncertain one keeps the owner-action semantics.
fn preflight_lifecycle_error(error: LifecycleFailure) -> AgentManagementError {
    if error.outcome_unknown || error.known_native_session_id.is_some() {
        return lifecycle_error(error);
    }
    AgentManagementError::InvalidRequestDetail(format!(
        "{}: {} (nothing was created; the name is free for a corrected retry with a new action_id)",
        error.code, error.detail
    ))
}

/// Failure codes produced only by the pure create preflight
/// (`AgentLifecycle::effective_create_spec`), which runs before any
/// lifecycle effect. Older releases recorded them as owner_action_required
/// and kept the reservation; such records are provably effect-free.
const PRE_EFFECT_PREFLIGHT_FAILURE_CODES: &[&str] = &[
    "unsupported_claude_launch_permissions",
    "profile_preflight_failed",
];

/// Definite failure codes `AgentLifecycle::bootstrap_native` returns only
/// before it spawns anything (configuration / profile / plan validation).
/// Older releases ran these checks after the private cwd step, so such a
/// Create holds its reservation in a retryable `native_bootstrap_pending`
/// failure although no native thread can exist. The pure checks now run in
/// `effective_create_spec`; existing records are released like the
/// preflight ones above.
const PRE_SPAWN_BOOTSTRAP_FAILURE_CODES: &[&str] = &[
    "native_bootstrap_preflight_failed",
    "bootstrap_plan_failed",
    "invalid_profile",
    "invalid_claude_effort",
    "profile_preflight_failed",
];

/// A legacy Create action that failed in the pure preflight and therefore
/// holds its name reservation without any possible effect. A new Create
/// may release it (terminal NoWrite) instead of failing with
/// `unresolved_agent_reservation`.
fn pre_effect_preflight_reservation(
    state: &AgentManagementSnapshot,
    action: &AgentActionRecord,
) -> bool {
    if action.operation == AgentOperationKind::Replace {
        return pre_successor_replace_reservation(state, action);
    }
    if action.operation != AgentOperationKind::Create
        || action.reserved_agent_name.is_none()
        || action.known_native_session_id.is_some()
        || action.known_successor_cutex_session.is_some()
        || action.historical_runtime_occurrence_fence.is_some()
        || state.bootstrap_intents.contains_key(&action.action_id)
    {
        return false;
    }
    if action.native_bootstrap_retryable {
        return pre_spawn_bootstrap_reservation(state, action);
    }
    if action.phase != AgentActionPhase::OwnerActionRequired {
        return false;
    }
    let Some(AgentManagementResponse {
        outcome: AgentManagementOutcome::OwnerActionRequired { failure },
        ..
    }) = action.response.as_ref()
    else {
        return false;
    };
    let detail = failure
        .detail
        .strip_prefix("owner_action_required: ")
        .unwrap_or(&failure.detail);
    if failure.action_id != action.action_id
        || detail.ends_with("(external outcome unknown)")
        || !PRE_EFFECT_PREFLIGHT_FAILURE_CODES
            .iter()
            .any(|code| detail.starts_with(&format!("{code}: ")))
    {
        return false;
    }
    let phases = state
        .phase_events
        .values()
        .filter(|event| event.action_id == action.action_id)
        .collect::<Vec<_>>();
    !phases.is_empty()
        && phases.iter().all(|event| {
            matches!(
                event.phase,
                AgentActionPhase::Prepared | AgentActionPhase::OwnerActionRequired
            )
        })
}

/// The recorded owner-action failure of an action: its terminal response, or
/// (for a retryable native bootstrap, which keeps no response) its journaled
/// failure event.
fn recorded_failure<'a>(
    state: &'a AgentManagementSnapshot,
    action: &'a AgentActionRecord,
) -> Option<&'a AgentManagementFailureEvent> {
    match action.response.as_ref().map(|response| &response.outcome) {
        Some(AgentManagementOutcome::OwnerActionRequired { failure }) => Some(failure),
        Some(_) => None,
        None => state
            .failure_events
            .get(&format!("agent-management:{}:failure", action.action_id.as_str())),
    }
}

/// A Replace that stopped (owner action required) before its successor
/// existed: it never got past closing its predecessor (r244: the
/// predecessor check failed with `managed Agent observation mismatch: cwd`),
/// so no native thread, durable session or ownership record of the reserved
/// successor name can exist. Its reservation is released like a pre-effect
/// Create; the predecessor stays an ordinary Agent (an active one still holds
/// its name through the roster collision check).
fn pre_successor_replace_reservation(
    state: &AgentManagementSnapshot,
    action: &AgentActionRecord,
) -> bool {
    if action.reserved_agent_name.is_none()
        || action.phase != AgentActionPhase::OwnerActionRequired
        || action.known_native_session_id.is_some()
        || action.known_successor_cutex_session.is_some()
        || action.native_bootstrap_retryable
        || action.historical_runtime_occurrence_fence.is_some()
        || state.bootstrap_intents.contains_key(&action.action_id)
        || recorded_failure(state, action).is_none_or(|failure| failure.action_id != action.action_id)
    {
        return false;
    }
    let phases = state
        .phase_events
        .values()
        .filter(|event| event.action_id == action.action_id)
        .collect::<Vec<_>>();
    !phases.is_empty()
        && phases.iter().all(|event| {
            matches!(
                event.phase,
                AgentActionPhase::Prepared
                    | AgentActionPhase::PredecessorClosing
                    | AgentActionPhase::OwnerActionRequired
            )
        })
}

/// A Create whose native bootstrap failed definitely before anything was
/// spawned (see [`PRE_SPAWN_BOOTSTRAP_FAILURE_CODES`]): parked retryable in
/// `native_bootstrap_pending` without SID or adoption, and only the
/// prepared / private-cwd / bootstrap-pending / owner-action phases journaled.
fn pre_spawn_bootstrap_reservation(
    state: &AgentManagementSnapshot,
    action: &AgentActionRecord,
) -> bool {
    if !matches!(
        action.phase,
        AgentActionPhase::NativeBootstrapPending | AgentActionPhase::OwnerActionRequired
    ) {
        return false;
    }
    let Some(failure) = recorded_failure(state, action) else {
        return false;
    };
    let detail = failure
        .detail
        .strip_prefix("owner_action_required: ")
        .unwrap_or(&failure.detail);
    if failure.action_id != action.action_id
        || detail.ends_with("(external outcome unknown)")
        || !PRE_SPAWN_BOOTSTRAP_FAILURE_CODES
            .iter()
            .any(|code| detail.starts_with(&format!("{code}: ")))
    {
        return false;
    }
    let phases = state
        .phase_events
        .values()
        .filter(|event| event.action_id == action.action_id)
        .collect::<Vec<_>>();
    !phases.is_empty()
        && phases.iter().all(|event| {
            matches!(
                event.phase,
                AgentActionPhase::Prepared
                    | AgentActionPhase::PrivateCwdReady
                    | AgentActionPhase::NativeBootstrapPending
                    | AgentActionPhase::OwnerActionRequired
            )
        })
}

/// Terminalizes one `pre_effect_preflight_reservation` action as NoWrite
/// (keeping its original failure in the detail) and journals the phase.
fn release_pre_effect_reservation(
    state: &mut AgentManagementSnapshot,
    stale: &AgentActionRecord,
    released_by: &AgentActionId,
) -> Result<AgentManagementPhaseEvent, AgentManagementError> {
    let original = recorded_failure(state, stale)
        .map(|failure| failure.detail.clone())
        .ok_or(AgentManagementError::InvalidStore)?;
    let committed_at = now();
    let response = no_write(
        &stale.action_id,
        "reservation_released_pre_effect",
        &format!(
            "released by {}: {} ({original})",
            released_by.as_str(),
            if stale.operation == AgentOperationKind::Replace {
                "this replace stopped before creating its successor"
            } else {
                "the create preflight rejected this action before any native effect"
            }
        ),
    );
    let target = state
        .actions
        .get_mut(&stale.action_id)
        .ok_or(AgentManagementError::InvalidStore)?;
    target.phase_sequence = target
        .phase_sequence
        .checked_add(1)
        .filter(|value| *value <= crate::role_revision::MAX_JSON_SAFE_INTEGER)
        .ok_or(AgentManagementError::Conflict("phase_sequence_overflow"))?;
    target.phase = AgentActionPhase::NoWrite;
    target.response = Some(response);
    target.native_bootstrap_retryable = false;
    target.updated_at = committed_at.clone();
    let target = target.clone();
    let event_id = format!(
        "agent-management:{}:phase:{}",
        target.action_id.as_str(),
        target.phase_sequence
    );
    let event = AgentManagementPhaseEvent {
        event_id: event_id.clone(),
        action_id: target.action_id.clone(),
        project_id: target.project_id.clone(),
        operation: target.operation,
        phase: AgentActionPhase::NoWrite,
        phase_sequence: target.phase_sequence,
        committed_at,
        presentation_owner_cutex_session_id: target.caller_cutex_session.clone(),
        subject_cutex_session_id: None,
        subject_agent_name: target.reserved_agent_name.clone(),
        predecessor_cutex_session_id: None,
        successor_cutex_session_id: None,
        replace_policy: None,
        rotation_mode: None,
        authority_epoch: None,
    };
    if state
        .phase_events
        .insert(event_id, event.clone())
        .is_some()
    {
        return Err(AgentManagementError::InvalidStore);
    }
    Ok(event)
}

/// Finishes one unfinished legacy `director_rotate` action (see
/// [`AgentManagementProvider::finish_removed_director_rotations`]) and
/// journals the terminal phase. Touches only this action's records.
fn finish_removed_director_rotation(
    state: &mut AgentManagementSnapshot,
    action_id: &AgentActionId,
) -> Result<AgentManagementPhaseEvent, AgentManagementError> {
    const DETAIL: &str = "director_rotate was removed in r219: this rotation was not finished \
(no Director seat transfer, no session change). The owner assigns the project Director.";
    let action = state
        .actions
        .get(action_id)
        .cloned()
        .ok_or(AgentManagementError::InvalidStore)?;
    let previous = state
        .phase_events
        .values()
        .filter(|event| &event.action_id == action_id)
        .max_by_key(|event| event.phase_sequence)
        .cloned();
    let effect_free = action.phase == AgentActionPhase::Prepared
        && action.known_successor_cutex_session.is_none()
        && action.known_native_session_id.is_none()
        && !action.native_bootstrap_retryable
        && action.historical_runtime_occurrence_fence.is_none()
        && state
            .phase_events
            .values()
            .filter(|event| &event.action_id == action_id)
            .all(|event| event.phase == AgentActionPhase::Prepared);
    let committed_at = now();
    let (phase, response) = if effect_free {
        (
            AgentActionPhase::NoWrite,
            no_write(action_id, DIRECTOR_ROTATE_REMOVED_CODE, DETAIL),
        )
    } else {
        let event_id = format!(
            "agent-management:{}:failure:{DIRECTOR_ROTATE_REMOVED_CODE}",
            action_id.as_str()
        );
        let route_to_director_session = state
            .projects
            .get(&action.project_id)
            .map(|authority| authority.authorized_director_session.clone());
        let failure = AgentManagementFailureEvent {
            schema: AgentManagementFailureSchema::V1,
            event_id: event_id.clone(),
            action_id: action_id.clone(),
            project_id: action.project_id.clone(),
            operation: AgentOperationKind::DirectorRotate,
            code: DIRECTOR_ROTATE_REMOVED_CODE.to_string(),
            detail: DETAIL.to_string(),
            routing_status: if route_to_director_session.is_some() {
                FailureRoutingStatus::Routable
            } else {
                FailureRoutingStatus::Unrouted
            },
            route_to_director_session,
            target_cutex_session_id: None,
            created_at: committed_at.clone(),
        };
        state.failure_events.insert(event_id, failure.clone());
        (
            AgentActionPhase::Failure,
            AgentManagementResponse {
                schema: AgentManagementSchema::V1,
                action_id: action_id.clone(),
                outcome: AgentManagementOutcome::OwnerActionRequired { failure },
            },
        )
    };
    let target = state
        .actions
        .get_mut(action_id)
        .ok_or(AgentManagementError::InvalidStore)?;
    target.phase_sequence = target
        .phase_sequence
        .checked_add(1)
        .filter(|value| *value <= crate::role_revision::MAX_JSON_SAFE_INTEGER)
        .ok_or(AgentManagementError::Conflict("phase_sequence_overflow"))?;
    target.phase = phase;
    target.response = Some(response);
    target.updated_at = committed_at.clone();
    let target = target.clone();
    let event_id = format!(
        "agent-management:{}:phase:{}",
        action_id.as_str(),
        target.phase_sequence
    );
    // Keep the presentation of the action's last committed phase.
    let event = AgentManagementPhaseEvent {
        event_id: event_id.clone(),
        action_id: action_id.clone(),
        project_id: target.project_id.clone(),
        operation: AgentOperationKind::DirectorRotate,
        phase,
        phase_sequence: target.phase_sequence,
        committed_at,
        presentation_owner_cutex_session_id: previous.as_ref().map_or_else(
            || target.caller_cutex_session.clone(),
            |event| event.presentation_owner_cutex_session_id.clone(),
        ),
        subject_cutex_session_id: previous
            .as_ref()
            .and_then(|event| event.subject_cutex_session_id.clone()),
        subject_agent_name: previous
            .as_ref()
            .and_then(|event| event.subject_agent_name.clone())
            .or_else(|| target.reserved_agent_name.clone()),
        predecessor_cutex_session_id: previous
            .as_ref()
            .and_then(|event| event.predecessor_cutex_session_id.clone()),
        successor_cutex_session_id: target.known_successor_cutex_session.clone(),
        replace_policy: None,
        rotation_mode: previous.as_ref().and_then(|event| event.rotation_mode),
        authority_epoch: previous.as_ref().and_then(|event| event.authority_epoch),
    };
    if state
        .phase_events
        .insert(event_id, event.clone())
        .is_some()
    {
        return Err(AgentManagementError::InvalidStore);
    }
    Ok(event)
}

/// Whether `created_at` lies clearly (more than 2 s) before `reference`;
/// unparsable times never count as earlier (fail closed).
fn record_predates(created_at: &Rfc3339, reference: &Rfc3339) -> bool {
    match (
        chrono::DateTime::parse_from_rfc3339(created_at.as_str()),
        chrono::DateTime::parse_from_rfc3339(reference.as_str()),
    ) {
        (Ok(created), Ok(reference)) => created < reference - chrono::Duration::seconds(2),
        _ => false,
    }
}

/// Refusal detail for a Create / Replace successor whose name an active Agent
/// already holds: who holds it, whether the caller can see that Agent, and a
/// free name to use instead.
fn active_name_taken_detail(
    snapshot: &AgentManagementSnapshot,
    holder: &ManagedAgentRecord,
    caller: &CutexSessionId,
) -> String {
    let name = holder.spec.name.as_str();
    let holder_id = holder.cutex_session_id.as_str();
    let security = crate::config::store::load_codez_config().agent_security;
    let boundary = crate::agent_bus::boundary::AgentBoundary::from_snapshot(&security, snapshot);
    let visible = if caller == &holder.cutex_session_id {
        "it is you"
    } else if boundary.host_wide() {
        let caller_groups = snapshot
            .agents
            .get(caller)
            .map(|agent| agent.spec.groups.as_slice())
            .unwrap_or_default();
        if crate::agent_bus::routing::groups_overlap(caller_groups, &holder.spec.groups) {
            "it is visible to you (agent_list shows it)"
        } else {
            "it is not visible to you (it shares no Agent Bus group with you)"
        }
    } else if boundary.may_see(caller.as_str(), holder_id) {
        "it is visible to you (agent_list shows it)"
    } else {
        "it is not visible to you (it belongs to another project or creator tree)"
    };
    let taken = |candidate: &str| {
        snapshot
            .agents
            .values()
            .any(|agent| agent.retired_at.is_none() && agent.spec.name == candidate)
    };
    let suggestion = (2..1000)
        .map(|n| format!("{name}-{n}"))
        .find(|candidate| !taken(candidate))
        .unwrap_or_else(|| format!("{name}-new"));
    // Owner decision (2026-10-11): name the holder's project even when the
    // caller cannot see the holder; on a single-owner host this is not
    // sensitive, and not knowing it is what made callers dig through config.
    let project = match super::projects::current_project_id(snapshot, holder) {
        Some(project_id) => match snapshot
            .project_presentations
            .get(&project_id)
            .map(|presentation| presentation.display_name.trim())
            .filter(|display| !display.is_empty() && *display != project_id.as_str())
        {
            Some(display) => format!("project {display} ({})", project_id.as_str()),
            None => format!("project {}", project_id.as_str()),
        },
        None => "no project".to_string(),
    };
    format!(
        "active_agent_name_taken: the name '{name}' is held by the active Agent {name} ({}, {project}); {visible}. \
Active Agent names are unique on this host (the cwd may be shared); nothing was created. \
Choose another name such as '{suggestion}', or close or replace that Agent first.",
        crate::short_ids::display(holder_id),
    )
}

fn unresolved_reservation_detail(
    blocking: &AgentActionRecord,
    spec: &ManagedAgentSpec,
) -> String {
    // Only the name is reserved: Agents may share a cwd.
    let field = format!("name '{}'", spec.name);
    let failure = match blocking.response.as_ref().map(|response| &response.outcome) {
        Some(AgentManagementOutcome::OwnerActionRequired { failure }) => {
            format!("; its failure was: {}", failure.detail)
        }
        _ => String::new(),
    };
    let phase = serde_json::to_value(blocking.phase)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{:?}", blocking.phase));
    format!(
        "unresolved_agent_reservation: unfinished action '{}' ({} in project {}, phase {}) still reserves the {}{}. \
Retrying the same action_id with the identical request resumes or replays it. \
Otherwise the owner must resolve that reservation (POST /v2/agent-management/reservation-reconciliation), \
or create the Agent under a different name.",
        blocking.action_id.as_str(),
        serde_json::to_value(blocking.operation)
            .ok()
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_else(|| format!("{:?}", blocking.operation)),
        blocking.project_id.as_str(),
        phase,
        field,
        failure,
    )
}

fn lifecycle_error(error: LifecycleFailure) -> AgentManagementError {
    let ambiguity = if error.outcome_unknown {
        " (external outcome unknown)"
    } else {
        ""
    };
    AgentManagementError::OwnerActionRequired(format!(
        "{}: {}{}",
        error.code, error.detail, ambiguity
    ))
}

fn operator_grant_revision(state: &AgentManagementSnapshot, project_id: &ProjectId) -> u64 {
    state
        .operator_grant_revisions
        .get(project_id)
        .copied()
        .unwrap_or(0)
}

fn next_operator_grant_revision(current: u64) -> Result<u64, AgentManagementError> {
    current
        .checked_add(1)
        .filter(|revision| *revision <= crate::role_revision::MAX_JSON_SAFE_INTEGER)
        .ok_or(AgentManagementError::Conflict(
            "operator_grant_revision_overflow",
        ))
}

fn operator_roster(
    state: &AgentManagementSnapshot,
    project_id: &ProjectId,
) -> AgentOperatorRosterProjection {
    let mut operators = state
        .operator_grants
        .get(project_id)
        .into_iter()
        .flat_map(|grants| grants.values().cloned())
        .collect::<Vec<_>>();
    operators.sort_by(|left, right| {
        left.operator_cutex_session_id
            .cmp(&right.operator_cutex_session_id)
    });
    AgentOperatorRosterProjection {
        grant_revision: operator_grant_revision(state, project_id),
        operators,
    }
}

#[allow(clippy::too_many_arguments)]
fn insert_operator_audit_event(
    state: &mut AgentManagementSnapshot,
    request: &AuthorizedAgentManagementRequest<'_>,
    operator: &CutexSessionId,
    kind: AgentOperatorAuditKind,
    previous_revision: u64,
    revision: u64,
    primary_director: &CutexSessionId,
    committed_at: crate::role_revision::Rfc3339,
) -> Result<(), AgentManagementError> {
    let event_id = format!(
        "agent-management:{}:operator-grant:{}",
        request.action_id.as_str(),
        revision
    );
    let event = AgentOperatorAuditEvent {
        event_id: event_id.clone(),
        action_id: request.action_id.clone(),
        project_id: request.project_id.clone(),
        operator_cutex_session_id: operator.clone(),
        kind,
        previous_grant_revision: previous_revision,
        grant_revision: revision,
        primary_director_cutex_session_id: primary_director.clone(),
        performed_by_human_management: false,
        committed_at,
    };
    if state
        .operator_audit_events
        .insert(event_id, event)
        .is_some()
    {
        return Err(AgentManagementError::InvalidStore);
    }
    Ok(())
}

fn complete_response(
    request: &AuthorizedAgentManagementRequest<'_>,
    digest: &Sha256,
    result: AgentManagementResult,
) -> AgentManagementResponse {
    let receipt = AgentManagementReceipt {
        schema: AgentManagementReceiptSchema::V1,
        action_id: request.action_id.clone(),
        request_sha256: digest.clone(),
        operation: request.operation.kind(),
        project_id: request.project_id.clone(),
        completed_at: now(),
        archive_kind: None,
        restorable: None,
        result,
    };
    AgentManagementResponse {
        schema: AgentManagementSchema::V1,
        action_id: request.action_id.clone(),
        outcome: AgentManagementOutcome::Complete { receipt },
    }
}

fn recover_runtime_for_agent(
    agent: &ManagedAgentRecord,
    lifecycle: &dyn AgentLifecycle,
) -> Result<RuntimeRecoveryOutcome, AgentManagementError> {
    let before = lifecycle
        .observe(&agent.cutex_session_id)
        .map_err(lifecycle_error)?;
    validate_recovery_spec(agent, &before)?;
    let outcome = lifecycle
        .recover_runtime(
            &agent.cutex_session_id,
            &agent.native_session_id,
            &agent.spec,
        )
        .map_err(lifecycle_error)?;
    let after = lifecycle
        .observe(&agent.cutex_session_id)
        .map_err(lifecycle_error)?;
    validate_recovery_spec(agent, &after)?;
    validate_identity_preserved(&before, &after)?;
    if after.runtime_generation != before.runtime_generation {
        return Err(AgentManagementError::OwnerActionRequired(
            "runtime recovery changed the claimed generation".to_string(),
        ));
    }
    match outcome {
        RuntimeRecoveryOutcome::NoClaim if before != after => {
            Err(AgentManagementError::OwnerActionRequired(
                "no-claim recovery mutated runtime state".to_string(),
            ))
        }
        RuntimeRecoveryOutcome::RecoveredExact => {
            if before.runtime_generation == 0 || before.runtime_agent_ids != after.runtime_agent_ids
            {
                return Err(AgentManagementError::OwnerActionRequired(
                    "recovered runtime does not preserve its claimed identity".to_string(),
                ));
            }
            validate_ready(agent, &after)?;
            Ok(outcome)
        }
        RuntimeRecoveryOutcome::ClearedDeadClaim => {
            if after.app_server_runtime
                || !after.runtime_agent_ids.is_empty()
                || !after.agent_bus_endpoint_ids.is_empty()
            {
                return Err(AgentManagementError::OwnerActionRequired(
                    "dead runtime recovery did not clear the ownership claim".to_string(),
                ));
            }
            Ok(outcome)
        }
        RuntimeRecoveryOutcome::NoClaim => Ok(outcome),
    }
}

fn validate_recovery_spec(
    agent: &ManagedAgentRecord,
    observation: &AgentRuntimeObservation,
) -> Result<(), AgentManagementError> {
    let spec = &agent.spec;
    let groups_match = observed_groups_match(
        agent,
        observation,
        &expected_runtime_groups(&spec.cwd, &spec.groups),
    );
    if !observation.active {
        return Err(managed_observation_mismatch("runtime recovery", "active"));
    }
    if let Some(field) = managed_spec_mismatch(agent, observation, groups_match) {
        return Err(managed_observation_mismatch("runtime recovery", field));
    }
    Ok(())
}

fn validate_managed_observation_identity(
    agent: &ManagedAgentRecord,
    observation: &AgentRuntimeObservation,
) -> Result<(), AgentManagementError> {
    validate_managed_observation_groups(
        agent,
        observation,
        &expected_runtime_groups(&agent.spec.cwd, &agent.spec.groups),
    )
}

fn validate_managed_observation_groups(
    agent: &ManagedAgentRecord,
    observation: &AgentRuntimeObservation,
    expected_groups: &[String],
) -> Result<(), AgentManagementError> {
    let groups_match = observed_groups_match(agent, observation, expected_groups);
    if let Some(field) = managed_spec_mismatch(agent, observation, groups_match) {
        return Err(managed_observation_mismatch(
            "managed Agent observation",
            field,
        ));
    }
    if observation.runtime_generation == 0 {
        return Err(managed_observation_mismatch(
            "managed Agent observation",
            "runtime_generation",
        ));
    }
    Ok(())
}

fn validate_ready(
    agent: &ManagedAgentRecord,
    observation: &AgentRuntimeObservation,
) -> Result<(), AgentManagementError> {
    validate_ready_with_groups(
        agent,
        observation,
        &expected_runtime_groups(&agent.spec.cwd, &agent.spec.groups),
    )
}

fn validate_ready_with_groups(
    agent: &ManagedAgentRecord,
    observation: &AgentRuntimeObservation,
    expected_groups: &[String],
) -> Result<(), AgentManagementError> {
    if !observation.active {
        return Err(managed_observation_mismatch("Agent readiness", "active"));
    }
    // Native observations retain desired explicit groups; legacy adapters may
    // include the deterministic cwd group. Both represent the same membership.
    let groups_match = observed_groups_match(agent, observation, expected_groups);
    if let Some(field) = managed_spec_mismatch(agent, observation, groups_match) {
        return Err(managed_observation_mismatch("Agent readiness", field));
    }
    if observation.runtime_generation == 0 {
        return Err(managed_observation_mismatch(
            "Agent readiness",
            "runtime_generation",
        ));
    }
    if !observation.app_server_runtime {
        return Err(managed_observation_mismatch(
            "Agent readiness",
            "app_server_runtime",
        ));
    }
    let [runtime] = observation.runtime_agent_ids.as_slice() else {
        return Err(managed_observation_mismatch(
            "Agent readiness",
            "runtime_agent_ids",
        ));
    };
    let [endpoint] = observation.agent_bus_endpoint_ids.as_slice() else {
        return Err(managed_observation_mismatch(
            "Agent readiness",
            "agent_bus_endpoint_ids",
        ));
    };
    if runtime != endpoint {
        return Err(managed_observation_mismatch(
            "Agent readiness",
            "sole_endpoint_identity",
        ));
    }
    Ok(())
}

/// The observed groups are the desired ones, or those plus the system-owned
/// cwd group. That group is derived from the cwd text, so a runtime started
/// on a Windows verbatim spelling of the spec cwd (`\\?\F:\x`, Claude
/// records before r245) carries the group of that spelling: the same
/// directory, accepted as well.
fn observed_groups_match(
    agent: &ManagedAgentRecord,
    observation: &AgentRuntimeObservation,
    expected_groups: &[String],
) -> bool {
    observation.groups == agent.spec.groups
        || observation.groups == expected_groups
        || (observation.cwd != agent.spec.cwd
            && crate::session::metadata::same_session_cwd(&observation.cwd, &agent.spec.cwd)
            && observation.groups == expected_runtime_groups(&observation.cwd, &agent.spec.groups))
}

fn managed_spec_mismatch(
    agent: &ManagedAgentRecord,
    observation: &AgentRuntimeObservation,
    groups_match: bool,
) -> Option<&'static str> {
    let spec = &agent.spec;
    // `profile` is mutable launch configuration/provenance. It is deliberately
    // excluded from the durable/native identity and stable-spec comparison.
    if observation.cutex_session_id != agent.cutex_session_id {
        Some("cutex_session_id")
    } else if observation.native_session_id != agent.native_session_id {
        Some("native_session_id")
    } else if !crate::session::metadata::same_session_cwd(&observation.cwd, &spec.cwd) {
        // A Windows verbatim spelling (`\\?\F:\x`) of the same directory is
        // the same cwd (r244: Claude records stored canonicalize() output).
        Some("cwd")
    } else if observation.runtime_backend != spec.runtime_backend {
        Some("runtime_backend")
    } else if observation.model != spec.model {
        Some("model")
    } else if observation.reasoning != spec.reasoning {
        Some("reasoning")
    } else if observation.permissions != spec.permissions {
        Some("permissions")
    } else if observation.approval_policy != spec.approval_policy {
        Some("approval_policy")
    } else if observation.sandbox_mode != spec.sandbox_mode {
        Some("sandbox_mode")
    } else if !groups_match {
        Some("groups")
    } else {
        None
    }
}

fn managed_observation_mismatch(context: &str, field: &str) -> AgentManagementError {
    AgentManagementError::OwnerActionRequired(format!("{context} mismatch: {field}"))
}

fn expected_runtime_groups(cwd: &str, groups: &[String]) -> Vec<String> {
    let cwd_hash = crate::agent_bus::identity::fnv1a_hex(cwd);
    crate::agent_bus::groups::normalize_registered_agent_groups(
        groups.to_vec(),
        Some(&cwd_hash[..7]),
        cwd,
    )
}

fn validate_identity_preserved(
    before: &AgentRuntimeObservation,
    after: &AgentRuntimeObservation,
) -> Result<(), AgentManagementError> {
    if before.cutex_session_id != after.cutex_session_id
        || before.native_session_id != after.native_session_id
    {
        Err(AgentManagementError::OwnerActionRequired(
            "lifecycle operation changed durable Agent or native thread identity".to_string(),
        ))
    } else {
        Ok(())
    }
}

fn error_response(
    action_id: &AgentActionId,
    error: AgentManagementError,
) -> AgentManagementResponse {
    let (code, detail) = error.wire();
    no_write(action_id, &code, &detail)
}

fn no_write(action_id: &AgentActionId, code: &str, detail: &str) -> AgentManagementResponse {
    AgentManagementResponse {
        schema: AgentManagementSchema::V1,
        action_id: action_id.clone(),
        outcome: AgentManagementOutcome::NoWrite {
            code: code.to_string(),
            detail: detail.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use super::*;
    include!("provider_archive_tests.rs");

    const REAL_LEGACY_NO_SID_DETAIL: &str =
        "owner_action_required: native_session_unknown: native exec exited exit status: 1 without one captured SID";
    const REAL_CURRENT_NO_SID_DETAIL: &str =
        "owner_action_required: native_session_unknown: native exec exited exit status: 1 without one captured SID (external outcome unknown)";
    const REAL_LEGACY_AMBIGUOUS_SID_DETAIL: &str =
        "owner_action_required: native_session_ambiguous: native exec exposed multiple possible session identities (external outcome unknown)";
    const REAL_MALFORMED_JSONL_DETAIL: &str =
        "native_bootstrap_output_malformed: native exec JSONL stdout line 1 is not valid JSON (external outcome unknown)";
    const REAL_OFFLINE_REVISION_CONFLICT_DETAIL: &str = "owner_action_required: session_offline_failed: cutex session store revision conflict: expected 1163254, current 1163259; reload before retrying (external outcome unknown)";
    const RECOVERED_NATIVE_SID: &str = "018e0000-0000-7000-8000-0000b0009ae4";

    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    enum FakeRecovery {
        #[default]
        NoClaim,
        ExactLive,
        DeadClaim,
        MismatchedGeneration,
        Ambiguous,
    }

    #[derive(Default)]
    struct FakeState {
        archive_path: Option<std::path::PathBuf>,
        resolved_backend: Option<String>,
        creator_calls: Vec<(String, Option<crate::session::model::SessionCreatedBy>)>,
        log: Vec<String>,
        scope_timeout_path: Option<std::path::PathBuf>,
        scope_timeout_child: Option<std::process::Child>,
        scope_absence_unknown: bool,
        scope_fail_after_cleanup: bool,
        next_agent: u64,
        known_sid_bootstrap_failure: Option<String>,
        definite_pre_sid_bootstrap_failures: usize,
        definite_pre_sid_code: Option<&'static str>,
        uncertain_pre_sid_bootstrap_failures: usize,
        preflight_rejections: usize,
        reconciliation_override: Option<NativeBootstrapReconciliation>,
        identity_reconciliation_override: Option<NativeBootstrapIdentityReconciliation>,
        occurrence_reconciliation_override: Option<HistoricalRuntimeOccurrenceReconciliation>,
        advance_before_fenced_restart_check: bool,
        advance_after_fenced_restart_check: bool,
        extra_online_groups: Vec<String>,
        recovery: FakeRecovery,
        legacy_unmigrated: Option<CutexSessionId>,
        launch_count: usize,
        agents: BTreeMap<CutexSessionId, AgentRuntimeObservation>,
        messages: HashMap<String, String>,
    }

    #[derive(Default)]
    struct FakeLifecycle {
        state: Mutex<FakeState>,
    }

    impl FakeLifecycle {
        fn fail_bootstrap_once_with_known_sid(native_session_id: &str) -> Self {
            Self {
                state: Mutex::new(FakeState {
                    known_sid_bootstrap_failure: Some(native_session_id.to_string()),
                    ..FakeState::default()
                }),
            }
        }

        fn fail_bootstrap_once_before_sid() -> Self {
            Self {
                state: Mutex::new(FakeState {
                    definite_pre_sid_bootstrap_failures: 1,
                    ..FakeState::default()
                }),
            }
        }

        fn fail_bootstrap_once_with_uncertain_sid() -> Self {
            Self {
                state: Mutex::new(FakeState {
                    uncertain_pre_sid_bootstrap_failures: 1,
                    ..FakeState::default()
                }),
            }
        }

        fn with_extra_online_group(group: &str) -> Self {
            Self {
                state: Mutex::new(FakeState {
                    extra_online_groups: vec![group.to_string()],
                    ..FakeState::default()
                }),
            }
        }

        fn with_reconciliation(outcome: NativeBootstrapReconciliation) -> Self {
            Self {
                state: Mutex::new(FakeState {
                    reconciliation_override: Some(outcome),
                    ..FakeState::default()
                }),
            }
        }

        fn with_identity_reconciliation(outcome: NativeBootstrapIdentityReconciliation) -> Self {
            Self {
                state: Mutex::new(FakeState {
                    identity_reconciliation_override: Some(outcome),
                    ..FakeState::default()
                }),
            }
        }

        fn log(&self) -> Vec<String> {
            self.state.lock().unwrap().log.clone()
        }

        fn bootstrap_count(&self) -> usize {
            self.log()
                .iter()
                .filter(|entry| entry.as_str() == "bootstrap:Hi.")
                .count()
        }

        fn message_count(&self) -> usize {
            self.log()
                .iter()
                .filter(|entry| entry.starts_with("message:"))
                .count()
        }

        fn retire_count(&self, cutex_session_id: &CutexSessionId) -> usize {
            let expected = format!("retire:{}", cutex_session_id.as_str());
            self.log()
                .iter()
                .filter(|entry| entry.as_str() == expected)
                .count()
        }

        fn corrupt_native_session_id(&self, cutex_session_id: &CutexSessionId) {
            self.state
                .lock()
                .unwrap()
                .agents
                .get_mut(cutex_session_id)
                .unwrap()
                .native_session_id = "native-conflict".to_string();
        }

        fn advance_runtime_occurrence(&self, cutex_session_id: &CutexSessionId) {
            let mut state = self.state.lock().unwrap();
            let agent = state.agents.get_mut(cutex_session_id).unwrap();
            agent.runtime_generation += 1;
            let runtime_id = format!(
                "runtime:{}:{}",
                cutex_session_id.as_str(),
                agent.runtime_generation
            );
            agent.active = true;
            agent.app_server_runtime = true;
            agent.runtime_agent_ids = vec![runtime_id.clone()];
            agent.agent_bus_endpoint_ids = vec![runtime_id];
        }

        fn set_occurrence_reconciliation(
            &self,
            outcome: HistoricalRuntimeOccurrenceReconciliation,
        ) {
            self.state
                .lock()
                .unwrap()
                .occurrence_reconciliation_override = Some(outcome);
        }

        fn advance_before_fenced_restart_check(&self) {
            self.state
                .lock()
                .unwrap()
                .advance_before_fenced_restart_check = true;
        }

        fn advance_after_fenced_restart_check(&self) {
            self.state
                .lock()
                .unwrap()
                .advance_after_fenced_restart_check = true;
        }

        fn offline_count(&self, cutex_session_id: &CutexSessionId) -> usize {
            let expected = format!("offline:{}", cutex_session_id.as_str());
            self.log()
                .iter()
                .filter(|entry| entry.as_str() == expected)
                .count()
        }

        fn online_count(&self, cutex_session_id: &CutexSessionId) -> usize {
            let expected = format!("online:{}", cutex_session_id.as_str());
            self.log()
                .iter()
                .filter(|entry| entry.as_str() == expected)
                .count()
        }

        fn set_groups(&self, cutex_session_id: &CutexSessionId, groups: Vec<String>) {
            let mut state = self.state.lock().unwrap();
            state.extra_online_groups.clear();
            state.agents.get_mut(cutex_session_id).unwrap().groups = groups;
        }

        fn set_profile(&self, cutex_session_id: &CutexSessionId, profile: &str) {
            self.state
                .lock()
                .unwrap()
                .agents
                .get_mut(cutex_session_id)
                .unwrap()
                .profile = profile.to_string();
        }

        fn disconnect_with_claim(&self, cutex_session_id: &CutexSessionId, recovery: FakeRecovery) {
            let mut state = self.state.lock().unwrap();
            state.recovery = recovery;
            let agent = state.agents.get_mut(cutex_session_id).unwrap();
            assert_eq!(agent.runtime_agent_ids.len(), 1);
            agent.app_server_runtime = false;
            agent.agent_bus_endpoint_ids.clear();
        }

        fn launch_count(&self) -> usize {
            self.state.lock().unwrap().launch_count
        }

        fn mark_legacy_unmigrated(&self, cutex_session_id: &CutexSessionId) {
            self.state.lock().unwrap().legacy_unmigrated = Some(cutex_session_id.clone());
        }

        fn recovery_count(&self) -> usize {
            self.log()
                .iter()
                .filter(|entry| entry.starts_with("recover:"))
                .count()
        }

        fn insert_agent(
            &self,
            cutex_session_id: &CutexSessionId,
            native_session_id: &str,
            spec: &ManagedAgentSpec,
        ) {
            let mut observed = observation(cutex_session_id, native_session_id, spec, true, 1);
            let runtime_id = format!("runtime:{}:1", cutex_session_id.as_str());
            observed.runtime_agent_ids = vec![runtime_id.clone()];
            observed.agent_bus_endpoint_ids = vec![runtime_id];
            observed.app_server_runtime = true;
            self.state
                .lock()
                .unwrap()
                .agents
                .insert(cutex_session_id.clone(), observed);
        }
    }

    impl AgentLifecycle for FakeLifecycle {
        fn archive_session_store_path(&self) -> Option<std::path::PathBuf> {
            let mut state = self.state.lock().unwrap();
            let path = state.archive_path.get_or_insert_with(|| root("archive-fixture").join("sessions.json")).clone();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut sessions = crate::session::store::load_cutex_session_store_from_path(&path).unwrap();
            for observation in state.agents.values() {
                if !sessions.sessions.contains_key(observation.cutex_session_id.as_str()) {
                    let mut record = crate::session::model::CutexSessionRecord::new_at(
                        observation.cutex_session_id.as_str().to_string(), Some(observation.native_session_id.clone()),
                        crate::platform::host::current_host_name(), observation.cwd.clone(),
                        Some(observation.profile.clone()), now().as_str().to_string()).unwrap();
                    record.agent_enabled = true;
                    record.registration_class = crate::agent_bus::model::AgentRegistrationClass::Persistent;
                    record.formal_agent_name = Some(observation.cutex_session_id.as_str().to_string());
                    record.runtime_generation = observation.runtime_generation;
                    sessions.sessions.insert(record.cutex_session_id.clone(), record);
                }
            }
            crate::session::store::save_cutex_session_store_to_path(&path, &sessions).unwrap();
            Some(path)
        }

        fn archive_committed(&self, record: &crate::session::model::CutexSessionRecord) {
            let mut state = self.state.lock().unwrap();
            let id = CutexSessionId::new(record.cutex_session_id.clone()).unwrap();
            if state.agents.get(&id).is_some_and(|a| a.active != record.is_active()) {
                state.log.push(format!("archive:{}", id.as_str()));
            }
            state.agents.get_mut(&id).unwrap().active = record.is_active();
        }
        fn migration_required(&self, id: &CutexSessionId) -> Option<String> {
            (self.state.lock().unwrap().legacy_unmigrated.as_ref() == Some(id))
                .then(|| "migration_required: worker uses the legacy cute-alden backend.".to_string())
        }

        fn effective_create_spec(&self, spec: &ManagedAgentSpec) -> Result<ManagedAgentSpec, LifecycleFailure> {
            let mut state = self.state.lock().unwrap();
            if state.preflight_rejections > 0 {
                state.preflight_rejections -= 1;
                return Err(LifecycleFailure::definite(
                    "unsupported_claude_launch_permissions",
                    "Claude agents are created with full access and approval never",
                ));
            }
            let mut spec = spec.clone();
            if let Some(backend) = &state.resolved_backend { spec.runtime_backend = backend.clone(); }
            Ok(spec)
        }

        fn capture_offline_occurrence(
            &self,
            id: &CutexSessionId,
            legacy: Option<(&crate::role_revision::Rfc3339, &[u32])>,
        ) -> Result<Option<RuntimeOccurrenceFence>, LifecycleFailure> {
            let state = self.state.lock().unwrap();
            let Some(path) = &state.scope_timeout_path else {
                return Ok(None);
            };
            let store = crate::session::store::load_cutex_session_store_from_path(path).unwrap();
            let record = &store.sessions[id.as_str()];
            match legacy {
                Some((started, pids)) => {
                    crate::session::offline_reconciliation::legacy_timeout_occurrence(
                        record,
                        started.as_str(),
                        pids,
                    )
                    .map(Some)
                    .map_err(|e| {
                        LifecycleFailure::outcome_unknown("legacy_scope_fence", e.to_string())
                    })
                }
                None => Ok(Some(
                    crate::session::offline_reconciliation::durable_offline_occurrence(record),
                )),
            }
        }

        fn reconcile_offline_scope_timeout(
            &self,
            id: &CutexSessionId,
            expected: &RuntimeOccurrenceFence,
        ) -> Result<RuntimeOccurrenceFence, LifecycleFailure> {
            let mut state = self.state.lock().unwrap();
            let path = state
                .scope_timeout_path
                .as_ref()
                .expect("scope fixture path");
            let result = crate::session::offline_reconciliation::reconcile_offline_occurrence(
                path,
                id.as_str(),
                "private-host",
                expected,
                |_| {
                    anyhow::ensure!(
                        !state.scope_absence_unknown,
                        "scope observation unavailable"
                    );
                    crate::session::offline_reconciliation::prove_processes_and_endpoint_absent(
                        expected,
                    )
                },
            )
            .map_err(|e| {
                LifecycleFailure::outcome_unknown("scope_absence_failed", e.to_string())
            })?;
            let observed = state.agents.get_mut(id).unwrap();
            observed.app_server_runtime = false;
            observed.runtime_agent_ids.clear();
            observed.agent_bus_endpoint_ids.clear();
            if std::mem::take(&mut state.scope_fail_after_cleanup) {
                return Err(LifecycleFailure::outcome_unknown(
                    "injected_process_loss",
                    "after cleanup before action completion",
                ));
            }
            Ok(result)
        }
        fn prepare_private_cwd(&self, spec: &ManagedAgentSpec) -> Result<(), LifecycleFailure> {
            self.state
                .lock()
                .unwrap()
                .log
                .push(format!("prepare:{}", spec.name));
            Ok(())
        }

        fn bootstrap_native_with_creator(&self, spec: &ManagedAgentSpec, creator: Option<&crate::session::model::SessionCreatedBy>) -> Result<String, LifecycleFailure> {
            self.state.lock().unwrap().creator_calls.push(("bootstrap".into(),creator.cloned()));
            self.bootstrap_native(spec)
        }
        fn adopt_native_with_creator(&self, native: &str, spec: &ManagedAgentSpec, creator: Option<&crate::session::model::SessionCreatedBy>) -> Result<CutexSessionId, LifecycleFailure> {
            self.state.lock().unwrap().creator_calls.push(("adopt".into(),creator.cloned()));
            self.adopt_native(native,spec)
        }

        fn bootstrap_native(&self, _spec: &ManagedAgentSpec) -> Result<String, LifecycleFailure> {
            let mut state = self.state.lock().unwrap();
            state.next_agent += 1;
            state.log.push("bootstrap:Hi.".to_string());
            if let Some(native_session_id) = state.known_sid_bootstrap_failure.take() {
                return Err(LifecycleFailure {
                    code: "native_bootstrap_failed".to_string(),
                    detail: "native exec failed after exposing one exact SID".to_string(),
                    outcome_unknown: true,
                    known_native_session_id: Some(native_session_id),
                });
            }
            if state.definite_pre_sid_bootstrap_failures > 0 {
                state.definite_pre_sid_bootstrap_failures -= 1;
                return Err(LifecycleFailure::definite(
                    state.definite_pre_sid_code.unwrap_or("native_bootstrap_pre_sid_failed"),
                    "native wrapper exited before publishing a SID",
                ));
            }
            if state.uncertain_pre_sid_bootstrap_failures > 0 {
                state.uncertain_pre_sid_bootstrap_failures -= 1;
                return Err(LifecycleFailure::outcome_unknown(
                    "native_session_unknown",
                    "native wrapper outcome may include an unpublished SID",
                ));
            }
            Ok(format!("native-{}", state.next_agent))
        }

        fn reconcile_pre_sid_bootstrap(
            &self,
            spec: &ManagedAgentSpec,
            _started_at: &crate::role_revision::Rfc3339,
            _failed_at: &crate::role_revision::Rfc3339,
        ) -> Result<NativeBootstrapReconciliation, LifecycleFailure> {
            let mut state = self.state.lock().unwrap();
            state.log.push("reconcile-pre-sid".to_string());
            if let Some(outcome) = state.reconciliation_override.clone() {
                return Ok(outcome);
            }
            if state.agents.values().any(|agent| agent.cwd == spec.cwd) {
                Ok(NativeBootstrapReconciliation::Present {
                    reason: "fake exact managed runtime is present".to_string(),
                })
            } else {
                Ok(NativeBootstrapReconciliation::ProvenAbsent {
                    reason: "fake provider sources are empty".to_string(),
                })
            }
        }

        fn reconcile_interrupted_native_bootstrap(
            &self,
            spec: &ManagedAgentSpec,
            started: &Rfc3339,
            failed: &Rfc3339,
        ) -> Result<NativeBootstrapIdentityReconciliation, LifecycleFailure> {
            self.reconcile_ambiguous_native_bootstrap(spec, started, failed)
        }
        fn reconcile_ambiguous_native_bootstrap(
            &self,
            _spec: &ManagedAgentSpec,
            _started_at: &crate::role_revision::Rfc3339,
            _failed_at: &crate::role_revision::Rfc3339,
        ) -> Result<NativeBootstrapIdentityReconciliation, LifecycleFailure> {
            let mut state = self.state.lock().unwrap();
            state.log.push("reconcile-ambiguous-sid".to_string());
            Ok(state.identity_reconciliation_override.clone().unwrap_or(
                NativeBootstrapIdentityReconciliation::Absent {
                    reason: "fake selected-profile sources are empty".to_string(),
                },
            ))
        }

        fn reconcile_historical_runtime_occurrence(
            &self,
            cutex_session_id: &CutexSessionId,
        ) -> Result<HistoricalRuntimeOccurrenceReconciliation, LifecycleFailure> {
            let mut state = self.state.lock().unwrap();
            state.log.push(format!(
                "reconcile-occurrence:{}",
                cutex_session_id.as_str()
            ));
            if let Some(outcome) = state.occurrence_reconciliation_override.clone() {
                return Ok(outcome);
            }
            let observed = state
                .agents
                .get(cutex_session_id)
                .cloned()
                .ok_or_else(|| LifecycleFailure::definite("not_found", "missing fake Agent"))?;
            if observed.app_server_runtime
                || !observed.runtime_agent_ids.is_empty()
                || !observed.agent_bus_endpoint_ids.is_empty()
            {
                return Ok(HistoricalRuntimeOccurrenceReconciliation::Present {
                    reason: "fake provider has an active runtime occurrence".to_string(),
                });
            }
            Ok(HistoricalRuntimeOccurrenceReconciliation::ProvenAbsent {
                fence: RuntimeOccurrenceFence {
                    runtime_generation: observed.runtime_generation,
                    current_runtime_agent_id: None,
                    agent_bus_endpoint_ids: Vec::new(),
                    pending_launch_id: None,
                    app_server_launch_claim_id: None,
                    alden_session_name: None,
                    alden_pid: None,
                    runtime_pid: None,
                    app_server_pid: None,
                    app_server_endpoint: None,
                    app_server_connected: false,
                },
                reason: "fake provider sources prove the occurrence absent".to_string(),
            })
        }

        fn adopt_native(
            &self,
            native_session_id: &str,
            spec: &ManagedAgentSpec,
        ) -> Result<CutexSessionId, LifecycleFailure> {
            let mut state = self.state.lock().unwrap();
            state.log.push(format!("adopt:{native_session_id}"));
            if let Some(existing) = state
                .agents
                .values()
                .find(|agent| agent.native_session_id == native_session_id)
            {
                return Ok(existing.cutex_session_id.clone());
            }
            let suffix = native_session_id
                .strip_prefix("native-")
                .unwrap_or(native_session_id);
            let cutex_session_id = CutexSessionId::new(format!("cutex.agent-{suffix}")).unwrap();
            state.agents.insert(
                cutex_session_id.clone(),
                observation(&cutex_session_id, native_session_id, spec, true, 0),
            );
            Ok(cutex_session_id)
        }

        fn configure(
            &self,
            cutex_session_id: &CutexSessionId,
            _native_session_id: &str,
            _spec: &ManagedAgentSpec,
        ) -> Result<(), LifecycleFailure> {
            self.state
                .lock()
                .unwrap()
                .log
                .push(format!("configure:{}", cutex_session_id.as_str()));
            Ok(())
        }

        fn recover_runtime(
            &self,
            cutex_session_id: &CutexSessionId,
            native_session_id: &str,
            spec: &ManagedAgentSpec,
        ) -> Result<RuntimeRecoveryOutcome, LifecycleFailure> {
            let mut state = self.state.lock().unwrap();
            state
                .log
                .push(format!("recover:{}", cutex_session_id.as_str()));
            let recovery = state.recovery;
            let agent = state.agents.get_mut(cutex_session_id).unwrap();
            if agent.native_session_id != native_session_id
                || !crate::session::metadata::same_session_cwd(&agent.cwd, &spec.cwd)
                || agent.runtime_backend != spec.runtime_backend
                || agent.model != spec.model
                || agent.reasoning != spec.reasoning
                || agent.permissions != spec.permissions
                || agent.approval_policy != spec.approval_policy
                || agent.sandbox_mode != spec.sandbox_mode
            {
                return Err(LifecycleFailure::definite(
                    "runtime_recovery_spec_mismatch",
                    "managed runtime recovery identity/spec mismatch",
                ));
            }
            match recovery {
                FakeRecovery::NoClaim => Ok(RuntimeRecoveryOutcome::NoClaim),
                FakeRecovery::ExactLive => {
                    let [runtime_id] = agent.runtime_agent_ids.as_slice() else {
                        return Err(LifecycleFailure::definite(
                            "runtime_recovery_identity_missing",
                            "exact live claim omitted its runtime identity",
                        ));
                    };
                    agent.app_server_runtime = true;
                    agent.agent_bus_endpoint_ids = vec![runtime_id.clone()];
                    state.recovery = FakeRecovery::NoClaim;
                    Ok(RuntimeRecoveryOutcome::RecoveredExact)
                }
                FakeRecovery::DeadClaim => {
                    agent.app_server_runtime = false;
                    agent.runtime_agent_ids.clear();
                    agent.agent_bus_endpoint_ids.clear();
                    state.recovery = FakeRecovery::NoClaim;
                    Ok(RuntimeRecoveryOutcome::ClearedDeadClaim)
                }
                FakeRecovery::MismatchedGeneration => {
                    agent.runtime_generation += 1;
                    let runtime_id = format!(
                        "runtime:{}:{}",
                        cutex_session_id.as_str(),
                        agent.runtime_generation
                    );
                    agent.app_server_runtime = true;
                    agent.runtime_agent_ids = vec![runtime_id.clone()];
                    agent.agent_bus_endpoint_ids = vec![runtime_id];
                    Ok(RuntimeRecoveryOutcome::RecoveredExact)
                }
                FakeRecovery::Ambiguous => Err(LifecycleFailure::outcome_unknown(
                    "runtime_recovery_ambiguous",
                    "runtime ownership cannot be proven",
                )),
            }
        }

        fn online(&self, cutex_session_id: &CutexSessionId) -> Result<(), LifecycleFailure> {
            let mut state = self.state.lock().unwrap();
            state
                .log
                .push(format!("online:{}", cutex_session_id.as_str()));
            let extra_online_groups = state.extra_online_groups.clone();
            let launches = !state
                .agents
                .get(cutex_session_id)
                .unwrap()
                .app_server_runtime;
            if launches {
                state.launch_count += 1;
            }
            let agent = state.agents.get_mut(cutex_session_id).unwrap();
            if !agent.app_server_runtime {
                agent.runtime_generation += 1;
            }
            agent.active = true;
            agent.app_server_runtime = true;
            let runtime = format!(
                "runtime:{}:{}",
                cutex_session_id.as_str(),
                agent.runtime_generation
            );
            agent.runtime_agent_ids = vec![runtime.clone()];
            agent.agent_bus_endpoint_ids = vec![runtime];
            agent.groups = expected_runtime_groups(&agent.cwd, &agent.groups);
            agent.groups.extend(extra_online_groups);
            Ok(())
        }

        fn offline(&self, cutex_session_id: &CutexSessionId) -> Result<(), LifecycleFailure> {
            let mut state = self.state.lock().unwrap();
            if let Some(mut child) = state.scope_timeout_child.take() {
                let pid = child.id();
                child.kill().unwrap();
                child.wait().unwrap();
                return Err(LifecycleFailure::outcome_unknown(
                    "session_offline_failed",
                    format!("scope_terminate_timeout,{pid}:terminated"),
                ));
            }
            state
                .log
                .push(format!("offline:{}", cutex_session_id.as_str()));
            let agent = state.agents.get_mut(cutex_session_id).unwrap();
            agent.active = true;
            agent.app_server_runtime = false;
            agent.runtime_agent_ids.clear();
            agent.agent_bus_endpoint_ids.clear();
            Ok(())
        }

        fn offline_if_occurrence(
            &self,
            cutex_session_id: &CutexSessionId,
            expected: &RuntimeOccurrenceFence,
        ) -> Result<(), LifecycleFailure> {
            let mut state = self.state.lock().unwrap();
            let observed = state.agents.get(cutex_session_id).unwrap();
            if observed.runtime_generation != expected.runtime_generation
                || observed.runtime_agent_ids
                    != expected
                        .current_runtime_agent_id
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                || observed.agent_bus_endpoint_ids != expected.agent_bus_endpoint_ids
                || observed.app_server_runtime != expected.app_server_connected
            {
                state.log.push(format!(
                    "fenced-offline-rejected:{}",
                    cutex_session_id.as_str()
                ));
                return Err(LifecycleFailure::outcome_unknown(
                    "runtime_occurrence_changed",
                    "runtime occurrence changed before the fenced offline effect",
                ));
            }
            drop(state);
            self.offline(cutex_session_id)
        }

        fn restart_if_occurrence(
            &self,
            cutex_session_id: &CutexSessionId,
            expected: &RuntimeOccurrenceFence,
        ) -> Result<(AgentRuntimeObservation, AgentRuntimeObservation), LifecycleFailure> {
            let mut state = self.state.lock().unwrap();
            let advance = |state: &mut FakeState| {
                let agent = state.agents.get_mut(cutex_session_id).unwrap();
                agent.runtime_generation += 1;
                let runtime_id = format!(
                    "runtime:{}:{}",
                    cutex_session_id.as_str(),
                    agent.runtime_generation
                );
                agent.app_server_runtime = true;
                agent.runtime_agent_ids = vec![runtime_id.clone()];
                agent.agent_bus_endpoint_ids = vec![runtime_id];
            };
            if std::mem::take(&mut state.advance_before_fenced_restart_check) {
                advance(&mut state);
            }
            let matches = |observed: &AgentRuntimeObservation| {
                observed.runtime_generation == expected.runtime_generation
                    && observed.runtime_agent_ids
                        == expected
                            .current_runtime_agent_id
                            .iter()
                            .cloned()
                            .collect::<Vec<_>>()
                    && observed.agent_bus_endpoint_ids == expected.agent_bus_endpoint_ids
                    && observed.app_server_runtime == expected.app_server_connected
            };
            if !matches(state.agents.get(cutex_session_id).unwrap()) {
                state.log.push(format!(
                    "fenced-restart-rejected:{}",
                    cutex_session_id.as_str()
                ));
                return Err(LifecycleFailure::outcome_unknown(
                    "runtime_occurrence_changed",
                    "runtime occurrence changed before the fenced restart effect",
                ));
            }
            if std::mem::take(&mut state.advance_after_fenced_restart_check) {
                advance(&mut state);
            }
            if !matches(state.agents.get(cutex_session_id).unwrap()) {
                state.log.push(format!(
                    "fenced-restart-rejected:{}",
                    cutex_session_id.as_str()
                ));
                return Err(LifecycleFailure::outcome_unknown(
                    "runtime_occurrence_changed",
                    "runtime occurrence changed before the fenced restart claim",
                ));
            }
            let before = state.agents.get(cutex_session_id).unwrap().clone();
            state
                .log
                .push(format!("online:{}", cutex_session_id.as_str()));
            state.launch_count += 1;
            let agent = state.agents.get_mut(cutex_session_id).unwrap();
            agent.runtime_generation += 1;
            agent.active = true;
            agent.app_server_runtime = true;
            let runtime = format!(
                "runtime:{}:{}",
                cutex_session_id.as_str(),
                agent.runtime_generation
            );
            agent.runtime_agent_ids = vec![runtime.clone()];
            agent.agent_bus_endpoint_ids = vec![runtime];
            Ok((before, agent.clone()))
        }

        fn retire(&self, cutex_session_id: &CutexSessionId) -> Result<(), LifecycleFailure> {
            let mut state = self.state.lock().unwrap();
            state
                .log
                .push(format!("retire:{}", cutex_session_id.as_str()));
            state.agents.get_mut(cutex_session_id).unwrap().active = false;
            Ok(())
        }

        fn observe(
            &self,
            cutex_session_id: &CutexSessionId,
        ) -> Result<AgentRuntimeObservation, LifecycleFailure> {
            self.state
                .lock()
                .unwrap()
                .agents
                .get(cutex_session_id)
                .cloned()
                .ok_or_else(|| LifecycleFailure::definite("not_found", "missing fake Agent"))
        }

        fn send_message(
            &self,
            _system: &crate::agent_bus::identity::AgentManagementSystemPrincipal,
            metadata: &AgentManagementMessageMetadata,
            _to_agent: &CutexSessionId,
            exact_message: &str,
            external_message_id: &str,
        ) -> Result<String, LifecycleFailure> {
            let mut state = self.state.lock().unwrap();
            state.log.push(format!(
                "message:{external_message_id}:requested_by={}:{exact_message}",
                metadata.requested_by_director.as_str()
            ));
            let next = state.messages.len() + 1;
            Ok(state
                .messages
                .entry(external_message_id.to_string())
                .or_insert_with(|| format!("message-{next}"))
                .clone())
        }
    }

    fn root(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "cutex-am-{label}-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        ))
    }

    fn project() -> ProjectId {
        ProjectId::new("cutex-project").unwrap()
    }

    fn session(value: &str) -> CutexSessionId {
        CutexSessionId::new(value).unwrap()
    }

    fn action(value: &str) -> AgentActionId {
        AgentActionId::new(value).unwrap()
    }

    fn test_agent_cwd(name: &str) -> String {
        std::env::temp_dir()
            .join("cutex-agent-home")
            .join(name)
            .to_string_lossy()
            .into_owned()
    }

    fn spec(name: &str) -> ManagedAgentSpec {
        ManagedAgentSpec {
            name: name.to_string(),
            cwd: test_agent_cwd(name),
            profile: Some("orchid".to_string()),
            runtime_backend: "cute_alden".to_string(),
            model: "gpt-5.6-sol".to_string(),
            reasoning: "high".to_string(),
            permissions: "danger-full-access".to_string(),
            approval_policy: "never".to_string(),
            sandbox_mode: "danger-full-access".to_string(),
            groups: vec!["cutex".to_string(), format!("agent:{name}")],
            expose_to_im: true,
            pin: false,
        }
    }

    fn observation(
        cutex_session_id: &CutexSessionId,
        native_session_id: &str,
        spec: &ManagedAgentSpec,
        active: bool,
        generation: u64,
    ) -> AgentRuntimeObservation {
        AgentRuntimeObservation {
            cutex_session_id: cutex_session_id.clone(),
            native_session_id: native_session_id.to_string(),
            active,
            cwd: spec.cwd.clone(),
            profile: spec.profile.clone().unwrap_or_default(),
            runtime_backend: spec.runtime_backend.clone(),
            model: spec.model.clone(),
            reasoning: spec.reasoning.clone(),
            permissions: spec.permissions.clone(),
            approval_policy: spec.approval_policy.clone(),
            sandbox_mode: spec.sandbox_mode.clone(),
            groups: spec.groups.clone(),
            runtime_generation: generation,
            runtime_version: None,
            runtime_agent_ids: Vec::new(),
            app_server_runtime: false,
            agent_bus_endpoint_ids: Vec::new(),
        }
    }

    fn bind(
        provider: &AgentManagementProvider,
        action_id: &str,
        director: &str,
        expected: Option<(&str, u64)>,
    ) -> ProjectAuthorityReceipt {
        bind_project(provider, action_id, project().as_str(), director, expected)
    }

    fn bind_project(
        provider: &AgentManagementProvider,
        action_id: &str,
        project_id: &str,
        director: &str,
        expected: Option<(&str, u64)>,
    ) -> ProjectAuthorityReceipt {
        // r219: the Agent Management authority is the whole Director record.
        bind_project_only(provider, action_id, project_id, director, expected)
    }

    fn bind_project_only(
        provider: &AgentManagementProvider,
        action_id: &str,
        project_id: &str,
        director: &str,
        expected: Option<(&str, u64)>,
    ) -> ProjectAuthorityReceipt {
        provider
            .bind_project_authority(&ProjectAuthorityRequest {
                schema: AgentManagementSchema::V1,
                action_id: action(action_id),
                project_id: ProjectId::new(project_id).unwrap(),
                authorized_director_session: session(director),
                expected_authorized_director_session: expected.map(|value| session(value.0)),
                expected_authority_epoch: expected.map(|value| value.1),
            })
            .unwrap()
    }

    #[test]
    fn human_authority_correction_moves_only_the_authority_and_replays() {
        let root = root("human-authority-correction");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "init", "old", None);
        let receipt = bind_project_only(
            &provider,
            "correct",
            project().as_str(),
            "new",
            Some(("old", 1)),
        );
        assert_eq!(receipt.authority.authority_epoch, 2);
        assert_eq!(
            provider.store.snapshot().unwrap().projects[&project()].authorized_director_session,
            session("new")
        );
        assert_eq!(
            bind_project_only(
                &provider,
                "correct",
                project().as_str(),
                "new",
                Some(("old", 1))
            ),
            receipt
        );
        // r219: no Task Service seat copy is opened, created or transferred.
        assert!(!root.join("task-service-seat-authority-v1").exists());
    }

    #[test]
    fn human_authority_correction_rejects_a_stale_expectation_without_writes() {
        let provider = AgentManagementProvider::open(root("human-authority-stale")).unwrap();
        bind(&provider, "init", "old", None);
        bind_project_only(&provider, "correct", project().as_str(), "new", Some(("old", 1)));
        let before = provider.store.snapshot().unwrap();
        assert_eq!(
            provider.bind_project_authority(&ProjectAuthorityRequest {
                schema: AgentManagementSchema::V1,
                action_id: action("correct-again"),
                project_id: project(),
                authorized_director_session: session("other"),
                expected_authorized_director_session: Some(session("old")),
                expected_authority_epoch: Some(1),
            }),
            Err(AgentManagementError::Conflict("stale_project_authority"))
        );
        assert_eq!(provider.store.snapshot().unwrap(), before);
    }

    fn invocation(director: &str) -> AgentManagementInvocation {
        AgentManagementInvocation {
            caller_cutex_session: session(director),
            caller_runtime_agent_id: format!("runtime:{director}"),
        }
    }

    fn create_request(
        action_id: &str,
        name: &str,
        start_mode: AgentStartMode,
    ) -> AgentManagementRequest {
        AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action(action_id),
            project_id: None,
            operation: AgentOperation::Create {
                spec: spec(name),
                bootstrap_intent: None,
                start_mode,
                frozen_message: (start_mode == AgentStartMode::CustomMessage)
                    .then(|| "Frozen assignment.".to_string()),
            },
        }
    }

    fn rotation_request(action_id: &str, successor_name: &str) -> AgentManagementRequest {
        AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action(action_id),
            project_id: Some(project()),
            operation: AgentOperation::DirectorRotate {
                bootstrap_intent: None,
                expected_predecessor_cutex_session: session("cutex.director"),
                expected_authority_epoch: 1,
                mode: DirectorRotateMode::RetainPredecessorWithMessage,
                successor: spec(successor_name),
                frozen_message: Some("Frozen rotation assignment.".to_string()),
            },
        }
    }

    fn operator_request(
        action_id: &str,
        operation: &str,
        operator: &CutexSessionId,
        expected_grant_revision: u64,
    ) -> AgentManagementRequest {
        AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action(action_id),
            project_id: Some(project()),
            operation: match operation {
                "grant" => AgentOperation::GrantOperator {
                    operator_cutex_session_id: operator.clone(),
                    expected_grant_revision,
                },
                "revoke" => AgentOperation::RevokeOperator {
                    operator_cutex_session_id: operator.clone(),
                    expected_grant_revision,
                },
                _ => panic!("unknown operator fixture operation"),
            },
        }
    }

    fn seed_missing_ownership_reservation(
        provider: &AgentManagementProvider,
        request: &AgentManagementRequest,
    ) {
        let digest = request_sha256(request).unwrap();
        let timestamp = now();
        let (reserved_name, reserved_cwd) = match &request.operation {
            AgentOperation::Create { spec, .. } => (spec.name.clone(), spec.cwd.clone()),
            AgentOperation::Replace { successor, .. }
            | AgentOperation::DirectorRotate { successor, .. } => {
                (successor.name.clone(), successor.cwd.clone())
            }
            _ => panic!("fixture requires a create-like operation"),
        };
        let failure = AgentManagementFailureEvent {
            schema: AgentManagementFailureSchema::V1,
            event_id: format!("agent-management:{}:failure", request.action_id),
            action_id: request.action_id.clone(),
            project_id: project(),
            operation: request.operation.kind(),
            code: "owner_action_required".to_string(),
            detail:
                "owner_action_required: Agent has no explicit Agent Management ownership record"
                    .to_string(),
            routing_status: FailureRoutingStatus::Routable,
            route_to_director_session: Some(session("cutex.director")),
            target_cutex_session_id: None,
            created_at: timestamp.clone(),
        };
        let response = AgentManagementResponse {
            schema: AgentManagementSchema::V1,
            action_id: request.action_id.clone(),
            outcome: AgentManagementOutcome::OwnerActionRequired {
                failure: failure.clone(),
            },
        };
        provider
            .store()
            .with_state(true, |mut state| {
                state
                    .failure_events
                    .insert(failure.event_id.clone(), failure);
                state.actions.insert(
                    request.action_id.clone(),
                    AgentActionRecord {
                        action_id: request.action_id.clone(),
                        request_sha256: digest,
                        operation: request.operation.kind(),
                        project_id: project(),
                        caller_cutex_session: session("cutex.director"),
                        created_by: None,
                        phase: AgentActionPhase::OwnerActionRequired,
                        phase_sequence: 2,
                        reserved_agent_name: Some(reserved_name.clone()),
                        reserved_agent_cwd: Some(reserved_cwd),
                        known_successor_cutex_session: None,
                        known_native_session_id: None,
                        native_bootstrap_retryable: false,
                        historical_runtime_occurrence_fence: None,
                        external_message_id: Some(format!(
                            "agent-management:{}:start",
                            request.action_id
                        )),
                        response: Some(response),
                        created_at: timestamp.clone(),
                        updated_at: timestamp.clone(),
                    },
                );
                for (sequence, phase) in [
                    (1, AgentActionPhase::Prepared),
                    (2, AgentActionPhase::OwnerActionRequired),
                ] {
                    let event_id =
                        format!("agent-management:{}:phase:{sequence}", request.action_id);
                    state.phase_events.insert(
                        event_id.clone(),
                        AgentManagementPhaseEvent {
                            event_id,
                            action_id: request.action_id.clone(),
                            project_id: project(),
                            operation: request.operation.kind(),
                            phase,
                            phase_sequence: sequence,
                            committed_at: timestamp.clone(),
                            presentation_owner_cutex_session_id: session("cutex.director"),
                            subject_cutex_session_id: None,
                            subject_agent_name: Some(reserved_name.clone()),
                            predecessor_cutex_session_id: None,
                            successor_cutex_session_id: None,
                            replace_policy: None,
                            rotation_mode: None,
                            authority_epoch: Some(1),
                        },
                    );
                }
                Ok((state, (), true))
            })
            .unwrap();
    }

    fn reservation_reconciliation_request(
        admin_action_id: &str,
        target: &AgentManagementRequest,
    ) -> AgentReservationReconciliationRequest {
        let (name, cwd) = match &target.operation {
            AgentOperation::Create { spec, .. } => (&spec.name, &spec.cwd),
            AgentOperation::Replace { successor, .. }
            | AgentOperation::DirectorRotate { successor, .. } => (&successor.name, &successor.cwd),
            _ => panic!("fixture requires a create-like operation"),
        };
        AgentReservationReconciliationRequest {
            schema: AgentReservationReconciliationSchema::V1,
            action_id: action(admin_action_id),
            project_id: project(),
            target_action_id: target.action_id.clone(),
            expected_target_request_sha256: request_sha256(target).unwrap(),
            expected_phase: AgentActionPhase::OwnerActionRequired,
            expected_reserved_agent_name: name.clone(),
            expected_reserved_agent_cwd: cwd.clone(),
            expected_authorized_director_session: session("cutex.director"),
            expected_authority_epoch: 1,
        }
    }

    /// The r210-era receipt left by a Claude create whose permission
    /// preflight rejected `full-access`: owner_action_required with only the
    /// Prepared and OwnerActionRequired phases journaled.
    const REAL_CLAUDE_PREFLIGHT_DETAIL: &str = "owner_action_required: unsupported_claude_launch_permissions: Claude Management creation currently requires danger-full-access / never; use the session permissions UI after creation";

    fn seed_preflight_rejection_receipt(
        provider: &AgentManagementProvider,
        request: &AgentManagementRequest,
        detail: &str,
    ) -> AgentManagementResponse {
        let response = seed_owner_action_receipt(provider, request, detail);
        let AgentOperation::Create { spec, .. } = &request.operation else {
            panic!("preflight fixture requires create")
        };
        provider
            .store()
            .with_state(true, |mut state| {
                let action = state.actions.get_mut(&request.action_id).unwrap();
                action.phase_sequence = 2;
                for (sequence, phase) in [
                    (1, AgentActionPhase::Prepared),
                    (2, AgentActionPhase::OwnerActionRequired),
                ] {
                    let event_id = format!(
                        "agent-management:{}:phase:{sequence}",
                        request.action_id.as_str()
                    );
                    state.phase_events.insert(
                        event_id.clone(),
                        AgentManagementPhaseEvent {
                            event_id,
                            action_id: request.action_id.clone(),
                            project_id: project(),
                            operation: AgentOperationKind::Create,
                            phase,
                            phase_sequence: sequence,
                            committed_at: now(),
                            presentation_owner_cutex_session_id: session("cutex.director"),
                            subject_cutex_session_id: None,
                            subject_agent_name: Some(spec.name.clone()),
                            predecessor_cutex_session_id: None,
                            successor_cutex_session_id: None,
                            replace_policy: None,
                            rotation_mode: None,
                            authority_epoch: None,
                        },
                    );
                }
                Ok((state, (), true))
            })
            .unwrap();
        response
    }

    #[test]
    fn definite_create_preflight_rejection_is_no_write_and_releases_the_reservation() {
        let root = root("preflight-no-write");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle {
            state: Mutex::new(FakeState {
                preflight_rejections: 1,
                ..FakeState::default()
            }),
        };
        let rejected = create_request("create-rejected", "worker", AgentStartMode::BootstrapOnly);
        let response = provider.execute(&invocation("cutex.director"), &rejected, &lifecycle);
        assert!(
            matches!(
                response.outcome,
                AgentManagementOutcome::NoWrite { ref code, ref detail }
                    if code == "unsupported_claude_launch_permissions"
                        && detail.starts_with("Claude agents are created")
                        && detail.contains("nothing was created")
            ),
            "{response:?}"
        );
        let state = provider.store().snapshot().unwrap();
        assert_eq!(
            state.actions[&rejected.action_id].phase,
            AgentActionPhase::NoWrite
        );
        assert_eq!(lifecycle.bootstrap_count(), 0);
        // Exact replay returns the same terminal rejection without a launch.
        assert_eq!(
            provider.execute(&invocation("cutex.director"), &rejected, &lifecycle),
            response
        );
        assert_eq!(lifecycle.bootstrap_count(), 0);

        // A corrected retry under a new action_id is not fenced.
        let retry = create_request("create-retry", "worker", AgentStartMode::BootstrapOnly);
        completed(provider.execute(&invocation("cutex.director"), &retry, &lifecycle));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn legacy_preflight_owner_action_reservation_is_released_by_a_new_create() {
        let root = root("preflight-legacy-release");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let legacy = create_request("create-legacy", "worker", AgentStartMode::BootstrapOnly);
        seed_preflight_rejection_receipt(&provider, &legacy, REAL_CLAUDE_PREFLIGHT_DETAIL);

        let retry = create_request("create-retry", "worker", AgentStartMode::BootstrapOnly);
        completed(provider.execute(&invocation("cutex.director"), &retry, &lifecycle));
        assert_eq!(lifecycle.bootstrap_count(), 1);

        let state = provider.store().snapshot().unwrap();
        let released = &state.actions[&legacy.action_id];
        assert_eq!(released.phase, AgentActionPhase::NoWrite);
        assert_eq!(released.phase_sequence, 3);
        assert!(matches!(
            released.response.as_ref().map(|response| &response.outcome),
            Some(AgentManagementOutcome::NoWrite { code, detail })
                if code == "reservation_released_pre_effect"
                    && detail.contains("create-retry")
                    && detail.contains("unsupported_claude_launch_permissions")
        ));
        assert_eq!(
            state.phase_events["agent-management:create-legacy:phase:3"].phase,
            AgentActionPhase::NoWrite
        );
        // Replaying the released action returns its terminal NoWrite and
        // never launches.
        assert!(matches!(
            provider
                .execute(&invocation("cutex.director"), &legacy, &lifecycle)
                .outcome,
            AgentManagementOutcome::NoWrite { ref code, .. }
                if code == "reservation_released_pre_effect"
        ));
        assert_eq!(lifecycle.bootstrap_count(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn effectful_or_uncertain_reservations_still_block_with_an_actionable_detail() {
        for (label, detail, journal) in [
            ("no-sid", REAL_LEGACY_NO_SID_DETAIL, true),
            (
                "uncertain-preflight",
                "owner_action_required: unsupported_claude_launch_permissions: x (external outcome unknown)",
                true,
            ),
            ("unjournaled-preflight", REAL_CLAUDE_PREFLIGHT_DETAIL, false),
        ] {
            let root = root(&format!("preflight-block-{label}"));
            let provider = AgentManagementProvider::open(&root).unwrap();
            bind(&provider, "bind", "cutex.director", None);
            let lifecycle = FakeLifecycle::default();
            let legacy = create_request("create-legacy", "worker", AgentStartMode::BootstrapOnly);
            if journal {
                seed_preflight_rejection_receipt(&provider, &legacy, detail);
            } else {
                seed_owner_action_receipt(&provider, &legacy, detail);
            }
            let retry = create_request("create-retry", "worker", AgentStartMode::BootstrapOnly);
            let response = provider.execute(&invocation("cutex.director"), &retry, &lifecycle);
            assert!(
                matches!(
                    response.outcome,
                    AgentManagementOutcome::NoWrite { ref code, ref detail }
                        if code == "unresolved_agent_reservation"
                            && detail.starts_with("unfinished action")
                            && detail.contains("'create-legacy'")
                            && detail.contains("phase owner_action_required")
                            && detail.contains("name 'worker'")
                            && detail.contains("reservation-reconciliation")
                ),
                "{label}: {response:?}"
            );
            assert_eq!(lifecycle.bootstrap_count(), 0, "{label}");
            let state = provider.store().snapshot().unwrap();
            assert_eq!(
                state.actions[&legacy.action_id].phase,
                AgentActionPhase::OwnerActionRequired,
                "{label}"
            );
            assert!(!state.actions.contains_key(&retry.action_id), "{label}");
            let _ = std::fs::remove_dir_all(root);
        }
    }

    fn seed_legacy_no_sid_receipt(
        provider: &AgentManagementProvider,
        request: &AgentManagementRequest,
    ) -> AgentManagementResponse {
        seed_owner_action_receipt(provider, request, REAL_LEGACY_NO_SID_DETAIL)
    }

    fn seed_malformed_jsonl_receipt(
        provider: &AgentManagementProvider,
        request: &AgentManagementRequest,
    ) -> AgentManagementResponse {
        seed_owner_action_receipt(provider, request, REAL_MALFORMED_JSONL_DETAIL)
    }

    fn seed_legacy_ambiguous_sid_receipt(
        provider: &AgentManagementProvider,
        request: &AgentManagementRequest,
    ) -> AgentManagementResponse {
        let response =
            seed_owner_action_receipt(provider, request, REAL_LEGACY_AMBIGUOUS_SID_DETAIL);
        let snapshot = provider.store().snapshot().unwrap();
        let action = snapshot.actions[&request.action_id].clone();
        let event_id = format!(
            "agent-management:{}:phase:{}",
            request.action_id.as_str(),
            3
        );
        provider
            .store()
            .with_state(true, |mut state| {
                state.phase_events.insert(
                    event_id.clone(),
                    AgentManagementPhaseEvent {
                        event_id,
                        action_id: request.action_id.clone(),
                        project_id: project(),
                        operation: AgentOperationKind::Create,
                        phase: AgentActionPhase::NativeBootstrapPending,
                        phase_sequence: 3,
                        committed_at: action.created_at,
                        presentation_owner_cutex_session_id: session("cutex.director"),
                        subject_cutex_session_id: None,
                        subject_agent_name: action.reserved_agent_name.clone(),
                        predecessor_cutex_session_id: None,
                        successor_cutex_session_id: None,
                        replace_policy: None,
                        rotation_mode: None,
                        authority_epoch: None,
                    },
                );
                Ok((state, (), true))
            })
            .unwrap();
        response
    }

    fn seed_owner_action_receipt(
        provider: &AgentManagementProvider,
        request: &AgentManagementRequest,
        detail: &str,
    ) -> AgentManagementResponse {
        let digest = request_sha256(request).unwrap();
        let timestamp = now();
        let failure = AgentManagementFailureEvent {
            schema: AgentManagementFailureSchema::V1,
            event_id: format!("agent-management:{}:failure", request.action_id.as_str()),
            action_id: request.action_id.clone(),
            project_id: project(),
            operation: AgentOperationKind::Create,
            code: "owner_action_required".to_string(),
            detail: detail.to_string(),
            routing_status: FailureRoutingStatus::Routable,
            route_to_director_session: Some(session("cutex.director")),
            target_cutex_session_id: None,
            created_at: timestamp.clone(),
        };
        let response = AgentManagementResponse {
            schema: AgentManagementSchema::V1,
            action_id: request.action_id.clone(),
            outcome: AgentManagementOutcome::OwnerActionRequired {
                failure: failure.clone(),
            },
        };
        let AgentOperation::Create { spec, .. } = &request.operation else {
            panic!("legacy no-SID fixture requires create")
        };
        let external_message_id = matches!(
            &request.operation,
            AgentOperation::Create {
                frozen_message: Some(_),
                ..
            }
        )
        .then(|| format!("agent-management:{}:start", request.action_id.as_str()));
        provider
            .store()
            .with_state(true, |mut state| {
                state
                    .failure_events
                    .insert(failure.event_id.clone(), failure);
                state.actions.insert(
                    request.action_id.clone(),
                    AgentActionRecord {
                        action_id: request.action_id.clone(),
                        request_sha256: digest,
                        operation: AgentOperationKind::Create,
                        project_id: project(),
                        caller_cutex_session: session("cutex.director"),
                        created_by: None,
                        phase: AgentActionPhase::OwnerActionRequired,
                        phase_sequence: 4,
                        reserved_agent_name: Some(spec.name.clone()),
                        reserved_agent_cwd: Some(spec.cwd.clone()),
                        known_successor_cutex_session: None,
                        known_native_session_id: None,
                        native_bootstrap_retryable: false,
                        historical_runtime_occurrence_fence: None,
                        external_message_id,
                        response: Some(response.clone()),
                        created_at: timestamp.clone(),
                        updated_at: timestamp,
                    },
                );
                Ok((state, (), true))
            })
            .unwrap();
        response
    }

    fn seed_lifecycle_revision_conflict_receipt(
        provider: &AgentManagementProvider,
        request: &AgentManagementRequest,
        target: &CutexSessionId,
    ) -> AgentManagementResponse {
        let digest = request_sha256(request).unwrap();
        let timestamp = now();
        let failure = AgentManagementFailureEvent {
            schema: AgentManagementFailureSchema::V1,
            event_id: format!("agent-management:{}:failure", request.action_id.as_str()),
            action_id: request.action_id.clone(),
            project_id: project(),
            operation: request.operation.kind(),
            code: "owner_action_required".to_string(),
            detail: REAL_OFFLINE_REVISION_CONFLICT_DETAIL.to_string(),
            routing_status: FailureRoutingStatus::Routable,
            route_to_director_session: Some(session("cutex.director")),
            target_cutex_session_id: Some(target.clone()),
            created_at: timestamp.clone(),
        };
        let response = AgentManagementResponse {
            schema: AgentManagementSchema::V1,
            action_id: request.action_id.clone(),
            outcome: AgentManagementOutcome::OwnerActionRequired {
                failure: failure.clone(),
            },
        };
        provider
            .store()
            .with_state(true, |mut state| {
                state
                    .failure_events
                    .insert(failure.event_id.clone(), failure);
                state.actions.insert(
                    request.action_id.clone(),
                    AgentActionRecord {
                        action_id: request.action_id.clone(),
                        request_sha256: digest,
                        operation: request.operation.kind(),
                        project_id: project(),
                        caller_cutex_session: session("cutex.director"),
                        created_by: None,
                        phase: AgentActionPhase::OwnerActionRequired,
                        phase_sequence: 2,
                        reserved_agent_name: None,
                        reserved_agent_cwd: None,
                        known_successor_cutex_session: None,
                        known_native_session_id: None,
                        native_bootstrap_retryable: false,
                        historical_runtime_occurrence_fence: None,
                        external_message_id: None,
                        response: Some(response.clone()),
                        created_at: timestamp.clone(),
                        updated_at: timestamp,
                    },
                );
                Ok((state, (), true))
            })
            .unwrap();
        response
    }

    fn legacy_import_request(
        action_id: &str,
        project_id: &str,
        director: &str,
        epoch: u64,
    ) -> LegacyDirectorOwnershipImportRequest {
        LegacyDirectorOwnershipImportRequest {
            schema: LegacyDirectorOwnershipImportSchema::V1,
            action_id: action(action_id),
            project_id: ProjectId::new(project_id).unwrap(),
            director_cutex_session_id: session(director),
            expected_authorized_director_session: session(director),
            expected_authority_epoch: epoch,
        }
    }

    fn legacy_import_evidence(
        director: &str,
        native_session_id: &str,
    ) -> LegacyDirectorOwnershipEvidence {
        LegacyDirectorOwnershipEvidence {
            director_cutex_session_id: session(director),
            native_session_id: native_session_id.to_string(),
            durable_session_revision: 7,
            runtime_generation: 3,
            spec: spec("legacy-director"),
        }
    }

    fn completed(response: AgentManagementResponse) -> AgentManagementReceipt {
        match response.outcome {
            AgentManagementOutcome::Complete { receipt } => receipt,
            other => panic!("expected completed response, got {other:?}"),
        }
    }

    fn created_agent(receipt: &AgentManagementReceipt) -> ManagedAgentRecord {
        match &receipt.result {
            AgentManagementResult::Created { agent, .. } => agent.clone(),
            other => panic!("expected created result, got {other:?}"),
        }
    }

    fn ready_observation(agent: &ManagedAgentRecord) -> AgentRuntimeObservation {
        let runtime_id = "runtime:ready:1".to_string();
        let mut observation = observation(
            &agent.cutex_session_id,
            &agent.native_session_id,
            &agent.spec,
            true,
            1,
        );
        observation.groups = expected_runtime_groups(&agent.spec.cwd, &agent.spec.groups);
        observation.runtime_agent_ids = vec![runtime_id.clone()];
        observation.agent_bus_endpoint_ids = vec![runtime_id];
        observation.app_server_runtime = true;
        observation
    }

    #[test]
    fn legacy_director_import_enables_query_and_refuses_removed_rotation() {
        let provider = AgentManagementProvider::open(root("legacy-import-rotate")).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let request = legacy_import_request("import", project().as_str(), "cutex.director", 1);
        let evidence = legacy_import_evidence("cutex.director", "native-director");
        let (receipt, replayed) = provider
            .import_legacy_director_ownership(&request, || Ok(evidence.clone()))
            .unwrap();
        assert!(!replayed);
        assert_eq!(receipt.authority.authority_epoch, 1);
        assert_eq!(receipt.agent.cutex_session_id, session("cutex.director"));
        assert_eq!(
            receipt.agent.created_by_director_session,
            Some(session("cutex.director"))
        );

        let lifecycle = FakeLifecycle::default();
        lifecycle.insert_agent(
            &receipt.agent.cutex_session_id,
            &receipt.agent.native_session_id,
            &receipt.agent.spec,
        );
        let query = completed(provider.execute(
            &invocation("cutex.director"),
            &AgentManagementRequest {
                schema: AgentManagementSchema::V1,
                action_id: action("query-after-import"),
                project_id: Some(project()),
                operation: AgentOperation::QueryManaged,
            },
            &lifecycle,
        ));
        let AgentManagementResult::QueryManaged {
            authority, agents, ..
        } = query.result
        else {
            panic!("expected managed query result")
        };
        assert_eq!(authority.authority_epoch, 1);
        assert_eq!(agents, vec![receipt.agent.clone()]);

        // r219: director_rotate was removed; the request is refused before
        // any action is recorded and project authority is unchanged.
        let before = provider.store().snapshot().unwrap();
        let rotated = provider.execute(
            &invocation("cutex.director"),
            &AgentManagementRequest {
                schema: AgentManagementSchema::V1,
                action_id: action("rotate-after-import"),
                project_id: Some(project()),
                operation: AgentOperation::DirectorRotate {
                    bootstrap_intent: None,
                    expected_predecessor_cutex_session: session("cutex.director"),
                    expected_authority_epoch: 1,
                    mode: DirectorRotateMode::RetainPredecessorBootstrapOnly,
                    successor: spec("successor-director"),
                    frozen_message: None,
                },
            },
            &lifecycle,
        );
        let AgentManagementOutcome::NoWrite { code, .. } = rotated.outcome else {
            panic!("expected tool_removed, got {:?}", rotated.outcome)
        };
        assert_eq!(code, "tool_removed");
        let after = provider.store().snapshot().unwrap();
        assert!(!after.actions.contains_key(&action("rotate-after-import")));
        assert_eq!(after.store_revision, before.store_revision);
        assert_eq!(after.projects[&project()].authority_epoch, 1);
        assert_eq!(
            after.projects[&project()].authorized_director_session,
            session("cutex.director")
        );
    }

    #[test]
    fn legacy_director_import_replay_conflict_and_restart_are_durable() {
        let root = root("legacy-import-replay");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let request = legacy_import_request("import", project().as_str(), "cutex.director", 1);
        let evidence = legacy_import_evidence("cutex.director", "native-director");
        let (first, replayed) = provider
            .import_legacy_director_ownership(&request, || Ok(evidence))
            .unwrap();
        assert!(!replayed);
        drop(provider);

        let reopened = AgentManagementProvider::open(&root).unwrap();
        let (replay, replayed) = reopened
            .import_legacy_director_ownership(&request, || {
                panic!("exact replay must not reload durable session evidence")
            })
            .unwrap();
        assert!(replayed);
        assert_eq!(replay, first);
        assert_eq!(
            reopened.store().snapshot().unwrap().agents.len(),
            1,
            "restart replay must not duplicate ownership"
        );

        let mut changed = request.clone();
        changed.expected_authority_epoch = 2;
        assert_eq!(
            reopened
                .import_legacy_director_ownership(&changed, || {
                    panic!("payload conflict must precede evidence loading")
                })
                .unwrap_err(),
            AgentManagementError::Conflict("action_id_payload_conflict")
        );
    }

    #[test]
    fn legacy_director_import_fails_closed_on_authority_project_and_evidence_mismatch() {
        let provider = AgentManagementProvider::open(root("legacy-import-denials")).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let evidence = legacy_import_evidence("cutex.director", "native-director");

        let stale_epoch =
            legacy_import_request("stale-epoch", project().as_str(), "cutex.director", 2);
        assert_eq!(
            provider
                .import_legacy_director_ownership(&stale_epoch, || Ok(evidence.clone()))
                .unwrap_err(),
            AgentManagementError::Conflict("stale_project_authority")
        );
        let stale_session = legacy_import_request(
            "stale-session",
            project().as_str(),
            "cutex.other-director",
            1,
        );
        assert_eq!(
            provider
                .import_legacy_director_ownership(&stale_session, || Ok(evidence.clone()))
                .unwrap_err(),
            AgentManagementError::Conflict("stale_project_authority")
        );
        let wrong_project =
            legacy_import_request("wrong-project", "other-project", "cutex.director", 1);
        assert_eq!(
            provider
                .import_legacy_director_ownership(&wrong_project, || Ok(evidence.clone()))
                .unwrap_err(),
            AgentManagementError::Conflict("project_authority_not_initialized")
        );
        bind_project(
            &provider,
            "bind-other",
            "other-project",
            "cutex.director",
            None,
        );
        let cross_project =
            legacy_import_request("cross-project", project().as_str(), "cutex.director", 1);
        assert_eq!(
            provider
                .import_legacy_director_ownership(&cross_project, || Ok(evidence.clone()))
                .unwrap_err(),
            AgentManagementError::Conflict("director_authorized_for_multiple_projects")
        );

        let separate = AgentManagementProvider::open(root("legacy-import-evidence")).unwrap();
        bind(&separate, "bind", "cutex.director", None);
        let request = legacy_import_request("mismatch", project().as_str(), "cutex.director", 1);
        let mut mismatched = evidence;
        mismatched.director_cutex_session_id = session("cutex.other-director");
        assert_eq!(
            separate
                .import_legacy_director_ownership(&request, || Ok(mismatched))
                .unwrap_err(),
            AgentManagementError::Conflict("durable_session_identity_mismatch")
        );
        assert_eq!(
            separate
                .import_legacy_director_ownership(&request, || {
                    Err(AgentManagementError::NotFound(
                        "durable_director_session_not_found",
                    ))
                })
                .unwrap_err(),
            AgentManagementError::NotFound("durable_director_session_not_found")
        );
        assert!(separate.store().snapshot().unwrap().agents.is_empty());
    }

    #[test]
    fn legacy_director_import_rejects_existing_and_retired_ownership_records() {
        for (label, owned_project, retired, expected) in [
            (
                "existing",
                "cutex-project",
                false,
                AgentManagementError::Conflict("director_ownership_record_exists"),
            ),
            (
                "retired",
                "cutex-project",
                true,
                AgentManagementError::Conflict("director_ownership_record_retired"),
            ),
            (
                "cross-project-record",
                "other-project",
                false,
                AgentManagementError::Conflict("director_owned_by_another_project"),
            ),
        ] {
            let provider = AgentManagementProvider::open(root(label)).unwrap();
            bind(&provider, "bind", "cutex.director", None);
            let evidence = legacy_import_evidence("cutex.director", "native-director");
            provider
                .store()
                .with_state(true, |mut state| {
                    state.agents.insert(
                        session("cutex.director"),
                        ManagedAgentRecord {
                            project_id: Some(ProjectId::new(owned_project).unwrap()),
                            created_by_director_session: Some(session("cutex.director")),
                            created_by_operator_session: None,
                            cutex_session_id: session("cutex.director"),
                            native_session_id: evidence.native_session_id.clone(),
                            spec: evidence.spec.clone(),
                            created_at: now(),
                            retired_at: retired.then(now),
                        },
                    );
                    Ok((state, (), true))
                })
                .unwrap();
            let request = legacy_import_request("import", project().as_str(), "cutex.director", 1);
            assert_eq!(
                provider
                    .import_legacy_director_ownership(&request, || Ok(evidence))
                    .unwrap_err(),
                expected
            );
        }
    }

    #[test]
    fn readiness_accepts_only_the_cwd_derived_project_group_delta() {
        let agent = ManagedAgentRecord {
            project_id: Some(project()),
            created_by_director_session: Some(session("cutex.director")),
            created_by_operator_session: None,
            cutex_session_id: session("cutex.worker"),
            native_session_id: "native-worker".to_string(),
            spec: spec("worker"),
            created_at: now(),
            retired_at: None,
        };
        let ready = ready_observation(&agent);
        validate_ready(&agent, &ready).expect("cwd-derived project group is system-owned");
        let mut native = ready.clone();
        native.groups = agent.spec.groups.clone();
        validate_ready(&agent, &native).expect("native explicit groups are authoritative");

        let mut extra = ready.clone();
        extra.groups.push("unexpected-extra".to_string());
        assert!(validate_ready(&agent, &extra).is_err());

        let mut missing = ready;
        missing.groups.retain(|group| group != "cutex");
        assert!(validate_ready(&agent, &missing).is_err());
    }

    #[test]
    fn readiness_keeps_identity_defaults_and_sole_endpoint_strict() {
        let agent = ManagedAgentRecord {
            project_id: Some(project()),
            created_by_director_session: Some(session("cutex.director")),
            created_by_operator_session: None,
            cutex_session_id: session("cutex.worker"),
            native_session_id: "native-worker".to_string(),
            spec: spec("worker"),
            created_at: now(),
            retired_at: None,
        };
        let ready = ready_observation(&agent);

        macro_rules! reject_string_change {
            ($field:ident, $value:expr) => {{
                let mut changed = ready.clone();
                changed.$field = $value.to_string();
                let error = validate_ready(&agent, &changed)
                    .expect_err(concat!("must reject ", stringify!($field)));
                assert!(error.to_string().contains(stringify!($field)));
            }};
        }

        reject_string_change!(native_session_id, "other-native");
        reject_string_change!(cwd, test_agent_cwd("other-cwd"));
        reject_string_change!(runtime_backend, "host_foreground");
        reject_string_change!(model, "other-model");
        reject_string_change!(reasoning, "other-reasoning");
        reject_string_change!(permissions, "read-only");
        reject_string_change!(approval_policy, "on-request");
        reject_string_change!(sandbox_mode, "workspace-write");

        let mut other_session = ready.clone();
        other_session.cutex_session_id = session("cutex.other-worker");
        assert!(validate_ready(&agent, &other_session).is_err());

        let mut extra_endpoint = ready;
        extra_endpoint
            .agent_bus_endpoint_ids
            .push("runtime:unexpected:2".to_string());
        assert!(validate_ready(&agent, &extra_endpoint).is_err());
    }

    #[test]
    fn profile_is_not_identity_for_recovery_managed_observation_or_readiness() {
        let agent = ManagedAgentRecord {
            project_id: Some(project()),
            created_by_director_session: Some(session("cutex.director")),
            created_by_operator_session: None,
            cutex_session_id: session("cutex.worker"),
            native_session_id: "native-worker".to_string(),
            spec: spec("worker"),
            created_at: now(),
            retired_at: None,
        };
        assert_eq!(agent.spec.profile.as_deref(), Some("orchid"));

        for observed_profile in ["", "quince"] {
            let mut observed = ready_observation(&agent);
            observed.profile = observed_profile.to_string();
            validate_recovery_spec(&agent, &observed).unwrap_or_else(|error| {
                panic!("recovery rejected observed profile {observed_profile:?}: {error}")
            });
            validate_managed_observation_identity(&agent, &observed).unwrap_or_else(|error| {
                panic!("managed identity rejected observed profile {observed_profile:?}: {error}")
            });
            validate_ready(&agent, &observed).unwrap_or_else(|error| {
                panic!("readiness rejected observed profile {observed_profile:?}: {error}")
            });
        }
    }

    #[test]
    fn readiness_owner_action_replay_queues_custom_message_once_after_group_repair() {
        let root = root("readiness-group-replay");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::with_extra_online_group("unexpected-extra");
        let request = create_request("create", "worker", AgentStartMode::CustomMessage);

        let first = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        assert!(matches!(
            first.outcome,
            AgentManagementOutcome::OwnerActionRequired { .. }
        ));
        let snapshot = provider.store().snapshot().unwrap();
        let action = &snapshot.actions[&request.action_id];
        assert_eq!(action.phase, AgentActionPhase::Online);
        assert_eq!(lifecycle.bootstrap_count(), 1);
        assert_eq!(lifecycle.message_count(), 0);
        let cutex_session_id = action.known_successor_cutex_session.clone().unwrap();
        let agent = snapshot.agents[&cutex_session_id].clone();
        lifecycle.disconnect_with_claim(&cutex_session_id, FakeRecovery::ExactLive);
        lifecycle.set_groups(
            &cutex_session_id,
            expected_runtime_groups(&agent.spec.cwd, &agent.spec.groups),
        );

        let completed_response =
            provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        let replay = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        assert_eq!(completed_response, replay);
        assert!(matches!(
            completed_response.outcome,
            AgentManagementOutcome::Complete { .. }
        ));
        assert_eq!(lifecycle.bootstrap_count(), 1);
        assert_eq!(lifecycle.message_count(), 1);
        assert_eq!(lifecycle.launch_count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reviewed_bootstrap_missing_or_wrong_reference_never_uses_legacy_lifecycle() {
        let root = root("bootstrap-intent-refusal");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let mut request =
            create_request("explicit-create", "worker", AgentStartMode::BootstrapOnly);
        let original = serde_json::to_value(&request).unwrap();
        assert!(
            original.get("bootstrap_intent").is_none(),
            "legacy semantic digest must not change"
        );
        if let AgentOperation::Create {
            bootstrap_intent, ..
        } = &mut request.operation
        {
            *bootstrap_intent = Some(action("explicit-create"));
        }
        let response = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        assert!(
            matches!(response.outcome, AgentManagementOutcome::NoWrite { ref code, .. } if code == "bootstrap_intent_missing")
        );
        assert_eq!(lifecycle.bootstrap_count(), 0);
        assert!(provider.store.snapshot().unwrap().actions.is_empty());
        request.action_id = action("different-action");
        assert!(request.validate().is_err());
        let response = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        assert!(matches!(
            response.outcome,
            AgentManagementOutcome::NoWrite { .. }
        ));
        assert_eq!(lifecycle.bootstrap_count(), 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn creator_is_captured_before_bootstrap_and_exact_replay_survives_director_handover() {
        use crate::session::model::{SessionCreatedBy,SessionCreatorRole};
        let root=root("created-by-director");
        let provider=AgentManagementProvider::open(&root).unwrap();
        bind(&provider,"bind","cutex.director",None);
        let lifecycle=FakeLifecycle::default();
        let request=create_request("create-actor","actor-worker",AgentStartMode::BootstrapOnly);
        let receipt=completed(provider.execute(&invocation("cutex.director"),&request,&lifecycle));
        let expected=Some(SessionCreatedBy::Agent {cutex_session_id:session("cutex.director"),role:Some(SessionCreatorRole::Director)});
        assert_eq!(provider.store().snapshot().unwrap().actions[&request.action_id].created_by,expected);
        assert_eq!(lifecycle.state.lock().unwrap().creator_calls,vec![("bootstrap".into(),expected.clone()),("adopt".into(),expected.clone())]);
        bind(&provider,"handover","cutex.next-director",Some(("cutex.director",1)));
        let replay=completed(provider.execute(&invocation("cutex.director"),&request,&lifecycle));
        assert_eq!(created_agent(&replay).cutex_session_id,created_agent(&receipt).cutex_session_id);
        assert_eq!(lifecycle.state.lock().unwrap().creator_calls.len(),2,"replay never creates or restamps");
        let action=provider.store().snapshot().unwrap().actions[&request.action_id].clone();
        assert_eq!(action.created_by,expected,"captured role does not follow current Director");
        let mut legacy=serde_json::to_value(&action).unwrap();legacy.as_object_mut().unwrap().remove("created_by");
        let legacy:AgentActionRecord=serde_json::from_value(legacy).unwrap();
        assert!(legacy.created_by.is_none(),"old action provenance stays unknown");
        assert!(serde_json::to_value(&legacy).unwrap().get("created_by").is_none(),"old digest bytes gain no null field");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn creator_role_capture_never_substitutes_the_primary_director_for_an_operator() {
        use crate::session::model::{SessionCreatedBy,SessionCreatorRole};
        let caller=session("cutex.actual-operator");
        assert_eq!(captured_creator(&caller,AgentManagementRole::Operator),Some(SessionCreatedBy::Agent {
            cutex_session_id:caller.clone(),role:Some(SessionCreatorRole::Member),
        }));
        assert_eq!(captured_creator(&caller,AgentManagementRole::PrimaryDirector),Some(SessionCreatedBy::Agent {
            cutex_session_id:caller.clone(),role:Some(SessionCreatorRole::Director),
        }));
        assert!(captured_creator(&caller,AgentManagementRole::ReceiptReplay).is_none());
    }

    #[test]
    fn profile_backend_resolution_keeps_exact_request_replay() {
        let root = root("profile-backend");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        lifecycle.state.lock().unwrap().resolved_backend = Some("claude_subscription".into());
        let request = create_request("create-claude", "claude-worker", AgentStartMode::BootstrapOnly);
        let result = completed(provider.execute(&invocation("cutex.director"), &request, &lifecycle));
        assert_eq!(created_agent(&result).spec.runtime_backend, "claude_subscription");
        let replay = completed(provider.execute(&invocation("cutex.director"), &request, &lifecycle));
        assert_eq!(created_agent(&replay).cutex_session_id, created_agent(&result).cutex_session_id);
        assert_eq!(lifecycle.bootstrap_count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn create_follows_normal_order_supports_modes_and_exact_replay() {
        let root = root("create-order");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();

        let bootstrap = create_request(
            "create-bootstrap",
            "worker-a",
            AgentStartMode::BootstrapOnly,
        );
        let bootstrap_receipt =
            completed(provider.execute(&invocation("cutex.director"), &bootstrap, &lifecycle));
        assert!(matches!(
            bootstrap_receipt.result,
            AgentManagementResult::Created {
                message_id: None,
                ..
            }
        ));
        assert_eq!(
            lifecycle.log()[..6],
            [
                "prepare:worker-a",
                "bootstrap:Hi.",
                "adopt:native-1",
                "configure:cutex.agent-1",
                "recover:cutex.agent-1",
                "online:cutex.agent-1"
            ]
        );
        assert_eq!(
            lifecycle.message_count(),
            0,
            "bootstrap-only creation must not construct a custom message"
        );

        let custom = create_request("create-custom", "worker-b", AgentStartMode::CustomMessage);
        let first = provider.execute(&invocation("cutex.director"), &custom, &lifecycle);
        let replay = provider.execute(&invocation("cutex.director"), &custom, &lifecycle);
        assert_eq!(first, replay);
        assert_eq!(lifecycle.bootstrap_count(), 2);
        assert_eq!(lifecycle.message_count(), 1);
        let receipt = completed(first);
        match receipt.result {
            AgentManagementResult::Created {
                message_id: Some(message_id),
                ..
            } => assert_eq!(message_id, "message-1"),
            other => panic!("unexpected result: {other:?}"),
        }
        assert!(lifecycle.log().iter().any(
            |entry| entry
                == "message:agent-management:create-custom:start:requested_by=cutex.director:Frozen assignment."
        ));

        let mut changed = custom.clone();
        if let AgentOperation::Create { spec, .. } = &mut changed.operation {
            spec.model = "gpt-5.6-terra".to_string();
        }
        assert!(matches!(
            provider
                .execute(&invocation("cutex.director"), &changed, &lifecycle)
                .outcome,
            AgentManagementOutcome::NoWrite { ref code, .. } if code == "action_id_payload_conflict"
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unique_project_is_selected_implicitly_and_exact_request_replays() {
        let root = root("implicit-project");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind_project(&provider, "bind-alpha", "alpha", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let request = AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action("implicit-query"),
            project_id: None,
            operation: AgentOperation::QueryManaged,
        };

        let first = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        bind_project(&provider, "bind-beta", "beta", "cutex.director", None);
        let replay = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        assert_eq!(first, replay);
        let receipt = completed(first);
        assert_eq!(receipt.project_id, ProjectId::new("alpha").unwrap());
        assert_eq!(receipt.request_sha256, request_sha256(&request).unwrap());
        assert_eq!(provider.store().snapshot().unwrap().actions.len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn multi_project_selection_and_denials_are_typed_and_no_write() {
        let root = root("multi-project-selection");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind_project(&provider, "bind-alpha", "alpha", "cutex.director", None);
        bind_project(&provider, "bind-beta", "beta", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let mut request = AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action("query"),
            project_id: None,
            operation: AgentOperation::QueryManaged,
        };

        let ambiguous = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        assert!(matches!(
            ambiguous.outcome,
            AgentManagementOutcome::NoWrite { ref code, .. }
                if code == "project_selection_required"
        ));
        assert!(provider.store().snapshot().unwrap().actions.is_empty());

        request.project_id = Some(ProjectId::new("forged").unwrap());
        let forged = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        assert!(matches!(
            forged.outcome,
            AgentManagementOutcome::NoWrite { ref code, .. }
                if code == "project_not_authorized"
        ));
        assert!(provider.store().snapshot().unwrap().actions.is_empty());

        request.project_id = Some(ProjectId::new("alpha").unwrap());
        let worker = provider.execute(&invocation("cutex.worker"), &request, &lifecycle);
        assert!(matches!(
            worker.outcome,
            AgentManagementOutcome::NoWrite { ref code, .. }
                if code == "not_authorized_director"
        ));
        assert!(provider.store().snapshot().unwrap().actions.is_empty());

        let selected =
            completed(provider.execute(&invocation("cutex.director"), &request, &lifecycle));
        assert_eq!(selected.project_id, ProjectId::new("alpha").unwrap());
        assert_eq!(selected.request_sha256, request_sha256(&request).unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_and_automatic_project_chat_groups_do_not_select_authority() {
        let root = root("chat-groups-no-authority");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind_project(
            &provider,
            "bind-authoritative",
            "authoritative",
            "cutex.director",
            None,
        );
        let lifecycle = FakeLifecycle::default();
        let mut request = create_request(
            "create-grouped-worker",
            "grouped-worker",
            AgentStartMode::BootstrapOnly,
        );
        let AgentOperation::Create { spec, .. } = &mut request.operation else {
            unreachable!()
        };
        spec.groups = crate::agent_bus::identity::normalize_agent_groups(vec![
            "cutex".to_string(),
            "agent:grouped-worker".to_string(),
            "project:legacy-chat-label".to_string(),
            "project:automatic-routing-label".to_string(),
            "project:sc-polya-v2".to_string(),
            "project:umi-tools-scpolya-lab".to_string(),
        ]);

        let receipt =
            completed(provider.execute(&invocation("cutex.director"), &request, &lifecycle));
        assert_eq!(receipt.project_id, ProjectId::new("authoritative").unwrap());
        let AgentManagementResult::Created { agent, .. } = receipt.result else {
            panic!("expected created Agent")
        };
        assert!(agent
            .spec
            .groups
            .iter()
            .any(|group| group == "project:legacy-chat-label"));
        assert!(agent
            .spec
            .groups
            .iter()
            .any(|group| group == "project:automatic-routing-label"));
        let snapshot = provider.store().snapshot().unwrap();
        assert!(snapshot
            .projects
            .contains_key(&ProjectId::new("authoritative").unwrap()));
        assert!(!snapshot
            .projects
            .contains_key(&ProjectId::new("sc-polya-v2").unwrap()));
        assert!(!snapshot
            .projects
            .contains_key(&ProjectId::new("umi-tools-scpolya-lab").unwrap()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn authority_and_query_are_project_scoped_and_reject_old_director() {
        let root = root("authority-query");
        let provider = AgentManagementProvider::open(&root).unwrap();
        let first = bind(&provider, "bind-1", "cutex.director-old", None);
        assert_eq!(first.authority.authority_epoch, 1);
        let lifecycle = FakeLifecycle::default();
        let create = create_request("create", "worker", AgentStartMode::BootstrapOnly);
        provider.execute(&invocation("cutex.director-old"), &create, &lifecycle);
        let rebound = bind(
            &provider,
            "bind-2",
            "cutex.director-new",
            Some(("cutex.director-old", 1)),
        );
        assert_eq!(rebound.authority.authority_epoch, 2);
        let query = AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action("query"),
            project_id: Some(project()),
            operation: AgentOperation::QueryManaged,
        };
        assert!(matches!(
            provider
                .execute(&invocation("cutex.director-old"), &query, &lifecycle)
                .outcome,
            AgentManagementOutcome::NoWrite { ref code, .. } if code == "not_authorized_director"
        ));
        let unauthorized_custom = create_request(
            "unauthorized-custom",
            "worker-unauthorized",
            AgentStartMode::CustomMessage,
        );
        assert!(matches!(
            provider
                .execute(
                    &invocation("cutex.director-old"),
                    &unauthorized_custom,
                    &lifecycle
                )
                .outcome,
            AgentManagementOutcome::NoWrite { ref code, .. } if code == "not_authorized_director"
        ));
        assert_eq!(
            lifecycle.message_count(),
            0,
            "a caller that is no longer the project Director cannot reach the system projection"
        );
        let receipt =
            completed(provider.execute(&invocation("cutex.director-new"), &query, &lifecycle));
        match receipt.result {
            AgentManagementResult::QueryManaged {
                authority, agents, ..
            } => {
                assert_eq!(authority.authority_epoch, 2);
                assert_eq!(agents.len(), 1);
                assert_eq!(agents[0].project_id, Some(project()));
            }
            other => panic!("unexpected query: {other:?}"),
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lifecycle_preserves_durable_and_native_identity_and_close_soft_archives() {
        let root = root("lifecycle");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let created = created_agent(&completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("create", "worker", AgentStartMode::BootstrapOnly),
            &lifecycle,
        )));
        let restart = AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action("restart"),
            project_id: Some(project()),
            operation: AgentOperation::Restart {
                cutex_session_id: created.cutex_session_id.clone(),
            },
        };
        let restarted =
            completed(provider.execute(&invocation("cutex.director"), &restart, &lifecycle));
        match restarted.result {
            AgentManagementResult::Lifecycle { agent, observation } => {
                assert_eq!(agent.cutex_session_id, created.cutex_session_id);
                assert_eq!(observation.native_session_id, created.native_session_id);
                assert_eq!(observation.runtime_generation, 2);
            }
            other => panic!("unexpected restart: {other:?}"),
        }
        let close = AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action("close"),
            project_id: Some(project()),
            operation: AgentOperation::Close {
                cutex_session_id: created.cutex_session_id.clone(),
            },
        };
        let closed = completed(provider.execute(&invocation("cutex.director"), &close, &lifecycle));
        match closed.result {
            AgentManagementResult::Lifecycle { agent, observation } => {
                assert!(agent.retired_at.is_none());
                assert!(!observation.active);
                assert!(observation.agent_bus_endpoint_ids.is_empty());
            }
            other => panic!("unexpected close: {other:?}"),
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn durable_import_nullable_provenance_and_profile_support_normal_lifecycle() {
        use crate::management::control_plane::*;
        use crate::session::{model::CutexSessionRecord, store::save_cutex_session_store_to_path};
        let root = root("import-lifecycle");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let path = root.join("sessions.json");
        let id = session("cutex.imported");
        let config = spec("imported");
        let mut record = CutexSessionRecord::new(
            id.as_str().into(),
            Some("native-imported".into()),
            crate::platform::host::current_host_name(),
            config.cwd.clone(),
            None,
        )
        .unwrap();
        crate::session::runtime_defaults::apply_managed_session_defaults(
            &mut record,
            Some(&config.name),
            Some(config.cwd.clone()),
            config.groups.clone(),
            true,
            false,
        );
        record.runtime_backend = crate::session::model::CutexSessionRuntimeBackend::CuteAlden;
        record.model_defaults = Some(config.model.clone());
        record.reasoning_defaults = Some(config.reasoning.clone());
        record.permission_defaults = Some(config.permissions.clone());
        record.approval_policy = Some(config.approval_policy.clone());
        record.sandbox_mode = Some(config.sandbox_mode.clone());
        let mut sessions = crate::session::model::CutexSessionStore::default();
        sessions.sessions.insert(id.as_str().into(), record);
        save_cutex_session_store_to_path(&path, &sessions).unwrap();
        let principal = HumanManagementPrincipal::authenticated();
        let candidate = provider
            .durable_agent_candidates(&principal, &path)
            .unwrap()
            .remove(0);
        let request = DurableImportRequest {
            action_id: action("import"),
            confirmed_formal_name: config.name.clone(),
            candidate,
            detach: None,
            assignment: Some(HumanManagementProjectMutationRequest {
                schema: HumanManagementProjectMutationSchema::V1,
                action_id: action("import-add"),
                project_id: project(),
                expected_authority_epoch: Some(1),
                expected_project_revision: Some(0),
                operation: HumanManagementProjectMutationKind::AddMember {
                    cutex_session_id: id.clone(),
                },
            }),
        };
        let receipt = provider
            .import_durable_agent(&principal, &path, &request)
            .unwrap();
        assert!(receipt.complete, "{:?}", receipt.error);
        let imported = receipt.imported_agent.unwrap();
        assert!(
            imported.project_id.is_none()
                && imported.created_by_director_session.is_none()
                && imported.spec.profile.is_none()
        );
        let lifecycle = FakeLifecycle::default();
        lifecycle.insert_agent(&id, &imported.native_session_id, &imported.spec);
        lifecycle.set_profile(&id, "changed-profile");
        for (action_id, operation) in [
            (
                "restart",
                AgentOperation::Restart {
                    cutex_session_id: id.clone(),
                },
            ),
            (
                "close",
                AgentOperation::Close {
                    cutex_session_id: id.clone(),
                },
            ),
        ] {
            let result = completed(provider.execute(
                &invocation("cutex.director"),
                &AgentManagementRequest {
                    schema: AgentManagementSchema::V1,
                    action_id: action(action_id),
                    project_id: Some(project()),
                    operation,
                },
                &lifecycle,
            ));
            assert!(matches!(
                result.result,
                AgentManagementResult::Lifecycle { .. }
            ));
        }
        assert!(provider.store().snapshot().unwrap().agents[&id]
            .retired_at
            .is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn current_backend_projects_proven_native_migration_without_rewriting_create_spec() {
        use crate::session::{
            model::{CutexSessionRecord, CutexSessionStore},
            store::save_cutex_session_store_to_path,
        };
        let root = root("backend-migration-projection");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("create-worker", "worker", AgentStartMode::BootstrapOnly),
            &lifecycle,
        ));
        let original = provider.store().snapshot().unwrap();
        let agent = original.agents.values().next().unwrap();
        let mut record = CutexSessionRecord::new(
            agent.cutex_session_id.as_str().into(),
            Some(agent.native_session_id.clone()),
            crate::platform::host::current_host_name(),
            agent.spec.cwd.clone(),
            None,
        )
        .unwrap();
        record.runtime_backend = crate::session::model::CutexSessionRuntimeBackend::Host;
        record.explicit_launch = Some(ExplicitLaunchContract {
            version: 4,
            migration_action_id: None,
            native_id: agent.native_session_id.clone(),
            native_home: root.join("native"),
            bundle_manifest: root.join("bundle.json"),
            bundle_sha256: Sha256::new("a".repeat(64)).unwrap(),
        });
        record.managed_cwd = Some(root.join("next-cwd").to_string_lossy().into_owned());
        record.model_defaults = Some("next-model".into());
        record.sandbox_mode = Some("read-only".into());
        record.permission_defaults = Some("read-only".into());
        record.agent_groups = vec!["next-group".into()];
        let mut sessions = CutexSessionStore::default();
        sessions
            .sessions
            .insert(agent.cutex_session_id.as_str().into(), record);
        let path = root.join("sessions.json");
        save_cutex_session_store_to_path(&path, &sessions).unwrap();
        let provider = provider.with_current_names_path(path.clone());
        assert_eq!(
            provider.current_name_snapshot().unwrap().agents[&agent.cutex_session_id]
                .spec
                .runtime_backend,
            "host"
        );
        let current = provider.current_name_snapshot().unwrap().agents[&agent.cutex_session_id]
            .spec
            .clone();
        assert_eq!(current.cwd, root.join("next-cwd").to_string_lossy());
        assert_eq!(current.model, "next-model");
        assert_eq!(current.sandbox_mode, "read-only");
        assert_eq!(current.permissions, "read-only");
        assert_eq!(current.groups, vec!["next-group"]);

        assert_eq!(
            provider.store().snapshot().unwrap().agents[&agent.cutex_session_id]
                .spec
                .runtime_backend,
            "cute_alden"
        );
        sessions
            .sessions
            .get_mut(agent.cutex_session_id.as_str())
            .unwrap()
            .explicit_launch
            .as_mut()
            .unwrap()
            .native_id = "other-native".into();
        save_cutex_session_store_to_path(&path, &sessions).unwrap();
        assert_eq!(
            provider.current_name_snapshot().unwrap().agents[&agent.cutex_session_id]
                .spec
                .runtime_backend,
            "cute_alden"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn current_names_reach_all_fresh_readers_without_relabeling_receipts() {
        use crate::management::control_plane::HumanManagementPrincipal;
        use crate::session::{
            model::{CutexSessionRecord, CutexSessionStore},
            store::save_cutex_session_store_to_path,
        };
        let root = root("current-name-readers");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        for action_id in ["create-one", "create-two"] {
            completed(provider.execute(
                &invocation("cutex.director"),
                &create_request(action_id, action_id, AgentStartMode::BootstrapOnly),
                &lifecycle,
            ));
        }
        let mut sessions = CutexSessionStore::default();
        // This fixture binds authority separately from importing the Director.
        provider
            .store()
            .with_state(true, |mut state| {
                let mut director = state.agents.values().next().unwrap().clone();
                director.cutex_session_id = session("cutex.director");
                director.native_session_id = "native-director".into();
                director.spec.name = "Original Director".into();
                state
                    .agents
                    .insert(director.cutex_session_id.clone(), director);
                Ok((state, (), true))
            })
            .unwrap();
        let snapshot = provider.store().snapshot().unwrap();
        for agent in snapshot.agents.values() {
            let mut record = CutexSessionRecord::new(
                agent.cutex_session_id.as_str().into(),
                Some(agent.native_session_id.clone()),
                crate::platform::host::current_host_name(),
                agent.spec.cwd.clone(),
                None,
            )
            .unwrap();
            record.formal_agent_name = Some("Shared formal name".into());
            record.thread_name = Some("NEVER A FORMAL NAME".into());
            sessions
                .sessions
                .insert(agent.cutex_session_id.as_str().into(), record);
        }
        let path = root.join("sessions.json");
        save_cutex_session_store_to_path(&path, &sessions).unwrap();
        let provider = provider.with_current_names_path(path.clone());
        let query = AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action("query-before"),
            project_id: Some(project()),
            operation: AgentOperation::QueryManaged,
        };
        let original =
            completed(provider.execute(&invocation("cutex.director"), &query, &lifecycle));
        for id in sessions.sessions.keys().cloned().collect::<Vec<_>>() {
            crate::session::service::set_cutex_session_display_name_by_key(
                &mut sessions,
                &id,
                &format!("Renamed {id}"),
            )
            .unwrap();
        }
        save_cutex_session_store_to_path(&path, &sessions).unwrap();
        assert_eq!(
            completed(provider.execute(&invocation("cutex.director"), &query, &lifecycle)),
            original
        );
        let mut fresh = query.clone();
        fresh.action_id = action("query-after");
        let current =
            completed(provider.execute(&invocation("cutex.director"), &fresh, &lifecycle));
        let AgentManagementResult::QueryManaged { agents, .. } = current.result else {
            panic!("query result")
        };
        assert!(agents.len() >= 2);
        for agent in &agents {
            assert_eq!(
                agent.spec.name,
                format!("Renamed {}", agent.cutex_session_id.as_str())
            );
        }
        let principal = HumanManagementPrincipal::authenticated();
        let expected = Some("Renamed cutex.director".to_string());
        assert_eq!(
            provider
                .list_cutex_projects(&invocation("cutex.director"))
                .unwrap()[0]
                .director_name,
            expected
        );
        let collection = provider
            .list_cutex_projects_for_management(&principal)
            .unwrap();
        assert_eq!(collection.projects[0].director_name, expected);
        for choice in collection.available_agents {
            assert_eq!(
                choice.name,
                format!("Renamed {}", choice.cutex_session_id.as_str())
            );
        }
        let observer = |_: &CutexSessionId| Err(AgentManagementError::PersistenceUnavailable);
        for workspace in [
            provider
                .read_cutex_project(&invocation("cutex.director"), &project(), &observer)
                .unwrap(),
            provider
                .read_cutex_project_for_management(&principal, &project(), &observer)
                .unwrap(),
        ] {
            for member in workspace
                .active_agents
                .iter()
                .chain(&workspace.retired_agents)
                .chain(workspace.director.iter().filter_map(|d| d.member.as_ref()))
                .chain(workspace.agent_operators.iter().map(|o| &o.member))
            {
                assert_eq!(
                    member.agent.spec.name,
                    format!("Renamed {}", member.agent.cutex_session_id.as_str())
                );
            }
        }
        // A legacy record without the dedicated field uses only historical spec.name.
        for record in sessions.sessions.values_mut() {
            record.formal_agent_name = None;
        }
        save_cutex_session_store_to_path(&path, &sessions).unwrap();
        assert_eq!(
            provider.current_name_snapshot().unwrap().agents,
            snapshot.agents
        );
        sessions
            .sessions
            .get_mut("cutex.director")
            .unwrap()
            .cutex_session_id = "cutex.wrong".into();
        save_cutex_session_store_to_path(&path, &sessions).unwrap();
        assert!(provider
            .list_cutex_projects_for_management(&principal)
            .is_err());
        assert_eq!(
            completed(provider.execute(&invocation("cutex.director"), &query, &lifecycle)),
            original
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn scope_timeout_dead_child_replay_completes_once_and_preserves_failure() {
        use crate::session::model::{CutexSessionRecord, CutexSessionStore};
        use crate::session::store::save_cutex_session_store_to_path;
        for legacy in [false, true] {
            let root = root("scope-timeout-real-child");
            let provider = AgentManagementProvider::open(&root).unwrap();
            bind(&provider, "bind", "cutex.director", None);
            let lifecycle = FakeLifecycle::default();
            let created = created_agent(&completed(provider.execute(
                &invocation("cutex.director"),
                &create_request("create", "worker", AgentStartMode::BootstrapOnly),
                &lifecycle,
            )));
            let child = std::process::Command::new("sleep")
                .arg("60")
                .spawn()
                .unwrap();
            let pid = child.id();
            let path = root.join("sessions.json");
            let mut record = CutexSessionRecord::new(
                created.cutex_session_id.as_str().into(),
                Some(created.native_session_id.clone()),
                "private-host".into(),
                root.to_string_lossy().into(),
                None,
            )
            .unwrap();
            record.registration_class = crate::agent_bus::model::AgentRegistrationClass::Persistent;
            record.runtime_generation = 1;
            record.runtime_pid = Some(pid);
            record.exposed_to_backend = false;
            record.app_server_runtime = Some(crate::session::model::CutexAppServerRuntimeBinding {
                transport: crate::session::model::CutexAppServerTransport::UnixSocket,
                endpoint: format!("unix://{}/gone.sock", root.display()),
                pid,
                runtime_dir: root.to_string_lossy().into(),
                launched_profile: None,
                launched_profile_id: None,
                launch_profile_source: None,
                auth_token_path: None,
                diagnostic_journal_path: String::new(),
                schema_version: "test".into(),
                schema_sha256: "test".into(),
                started_at: "2026-01-01T00:00:00Z".into(),
            });
            let mut store = CutexSessionStore::default();
            store
                .sessions
                .insert(created.cutex_session_id.as_str().into(), record);
            save_cutex_session_store_to_path(&path, &store).unwrap();
            {
                let mut state = lifecycle.state.lock().unwrap();
                state.scope_timeout_path = Some(path.clone());
                state.scope_timeout_child = Some(child);
            }
            let request = AgentManagementRequest {
                schema: AgentManagementSchema::V1,
                action_id: action("offline-timeout"),
                project_id: Some(project()),
                operation: AgentOperation::Offline {
                    cutex_session_id: created.cutex_session_id.clone(),
                },
            };
            let first = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
            let AgentManagementOutcome::OwnerActionRequired { failure } = first.outcome else {
                panic!("expected scope timeout")
            };
            assert!(failure.detail.contains("scope_terminate_timeout"));
            assert!(!crate::platform::process::process_is_running(pid));
            if legacy {
                provider
                    .store()
                    .with_state(true, |mut state| {
                        state
                            .actions
                            .get_mut(&request.action_id)
                            .unwrap()
                            .historical_runtime_occurrence_fence = None;
                        Ok((state, (), true))
                    })
                    .unwrap();
            }
            let bytes = crate::session::store::session_store_json_bytes(&path).unwrap();
            lifecycle.state.lock().unwrap().scope_absence_unknown = true;
            assert!(!matches!(
                provider
                    .execute(&invocation("cutex.director"), &request, &lifecycle)
                    .outcome,
                AgentManagementOutcome::Complete { .. }
            ));
            assert_eq!(crate::session::store::session_store_json_bytes(&path).unwrap(), bytes);
            lifecycle.state.lock().unwrap().scope_absence_unknown = false;
            let mut changed =
                crate::session::store::load_cutex_session_store_from_path(&path).unwrap();
            changed
                .sessions
                .get_mut(created.cutex_session_id.as_str())
                .unwrap()
                .runtime_generation += 1;
            save_cutex_session_store_to_path(&path, &changed).unwrap();
            let drift_bytes = crate::session::store::session_store_json_bytes(&path).unwrap();
            assert!(!matches!(
                provider
                    .execute(&invocation("cutex.director"), &request, &lifecycle)
                    .outcome,
                AgentManagementOutcome::Complete { .. }
            ));
            assert_eq!(crate::session::store::session_store_json_bytes(&path).unwrap(), drift_bytes);
            changed
                .sessions
                .get_mut(created.cutex_session_id.as_str())
                .unwrap()
                .runtime_generation = 1;
            save_cutex_session_store_to_path(&path, &changed).unwrap();
            let mut altered = request.clone();
            altered.operation = AgentOperation::Restart {
                cutex_session_id: created.cutex_session_id.clone(),
            };
            assert!(!matches!(
                provider
                    .execute(&invocation("cutex.director"), &altered, &lifecycle)
                    .outcome,
                AgentManagementOutcome::Complete { .. }
            ));
            assert!(!matches!(
                provider
                    .execute(&invocation("cutex.other"), &request, &lifecycle)
                    .outcome,
                AgentManagementOutcome::Complete { .. }
            ));
            lifecycle.state.lock().unwrap().scope_fail_after_cleanup = true;
            assert!(!matches!(
                provider
                    .execute(&invocation("cutex.director"), &request, &lifecycle)
                    .outcome,
                AgentManagementOutcome::Complete { .. }
            ));
            let cleared = crate::session::store::session_store_json_bytes(&path).unwrap();
            let done = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
            assert!(
                matches!(done.outcome, AgentManagementOutcome::Complete { .. }),
                "{done:?}"
            );
            let bytes = crate::session::store::session_store_json_bytes(&path).unwrap();
            assert_eq!(bytes, cleared, "crash recovery must not clear twice");
            assert_eq!(
                provider.execute(&invocation("cutex.director"), &request, &lifecycle),
                done
            );
            assert_eq!(crate::session::store::session_store_json_bytes(&path).unwrap(), bytes);
            let snapshot = provider.store().snapshot().unwrap();
            assert_eq!(snapshot.failure_events[&failure.event_id], failure);
            let current = crate::session::store::load_cutex_session_store_from_path(&path).unwrap();
            assert!(!current.sessions[created.cutex_session_id.as_str()].exposed_to_backend);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn exact_restart_replay_reconciles_historical_offline_revision_conflict_once() {
        let root = root("restart-revision-conflict-replay");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = Arc::new(FakeLifecycle::default());
        let created = created_agent(&completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("create", "worker", AgentStartMode::BootstrapOnly),
            lifecycle.as_ref(),
        )));
        let request = AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action("restart-revision-conflict"),
            project_id: Some(project()),
            operation: AgentOperation::Restart {
                cutex_session_id: created.cutex_session_id.clone(),
            },
        };
        lifecycle.offline(&created.cutex_session_id).unwrap();
        seed_lifecycle_revision_conflict_receipt(&provider, &request, &created.cutex_session_id);
        let launches = lifecycle.launch_count();
        let offline_effects = lifecycle.offline_count(&created.cutex_session_id);
        drop(provider);
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let handles = (0..2)
            .map(|_| {
                let root = root.clone();
                let request = request.clone();
                let lifecycle = Arc::clone(&lifecycle);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let provider = AgentManagementProvider::open(root).unwrap();
                    barrier.wait();
                    provider.execute(&invocation("cutex.director"), &request, lifecycle.as_ref())
                })
            })
            .collect::<Vec<_>>();
        let responses = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(responses[0], responses[1]);
        let receipt = completed(responses[0].clone());
        let AgentManagementResult::Lifecycle { observation, .. } = receipt.result else {
            panic!("expected lifecycle receipt")
        };
        assert_eq!(observation.runtime_generation, 2);
        assert_eq!(lifecycle.launch_count(), launches + 1);
        assert_eq!(
            lifecycle.offline_count(&created.cutex_session_id),
            offline_effects,
            "a proven-absent historical occurrence does not need an offline effect"
        );
        let effects = lifecycle.log();
        assert_eq!(
            effects
                .iter()
                .filter(|entry| entry.starts_with("online:"))
                .count(),
            2,
            "create plus one recovered restart launch"
        );

        let reopened = AgentManagementProvider::open(&root).unwrap();
        let replay = reopened.execute(&invocation("cutex.director"), &request, lifecycle.as_ref());
        assert!(matches!(
            replay.outcome,
            AgentManagementOutcome::Complete { .. }
        ));
        assert_eq!(lifecycle.launch_count(), launches + 1);
        let action = reopened
            .store()
            .snapshot()
            .unwrap()
            .actions
            .get(&request.action_id)
            .cloned()
            .unwrap();
        assert!(
            action
                .historical_runtime_occurrence_fence
                .as_ref()
                .is_some_and(RuntimeOccurrenceFence::is_proven_absent),
            "the exact absence occurrence is durable with the reopened action"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn historical_restart_fences_newer_same_identity_occurrence_before_lifecycle_effect() {
        let root = root("restart-revision-conflict-newer-occurrence");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let created = created_agent(&completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("create", "worker", AgentStartMode::BootstrapOnly),
            &lifecycle,
        )));
        let request = AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action("restart-revision-conflict"),
            project_id: Some(project()),
            operation: AgentOperation::Restart {
                cutex_session_id: created.cutex_session_id.clone(),
            },
        };
        seed_lifecycle_revision_conflict_receipt(&provider, &request, &created.cutex_session_id);
        lifecycle.advance_runtime_occurrence(&created.cutex_session_id);
        let offline_effects = lifecycle.offline_count(&created.cutex_session_id);
        let launches = lifecycle.launch_count();

        let fenced = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        let AgentManagementOutcome::OwnerActionRequired { failure } = fenced.outcome else {
            panic!("newer occurrence must remain fenced")
        };
        assert_eq!(failure.code, "runtime_occurrence_reconciliation_present");
        assert_eq!(
            lifecycle.offline_count(&created.cutex_session_id),
            offline_effects,
            "a newer occurrence must not receive an offline call"
        );
        assert_eq!(lifecycle.launch_count(), launches);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn historical_restart_rechecks_committed_fence_at_atomic_restart_entry() {
        let root = root("restart-revision-conflict-occurrence-race");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let created = created_agent(&completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("create", "worker", AgentStartMode::BootstrapOnly),
            &lifecycle,
        )));
        let request = AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action("restart-revision-conflict"),
            project_id: Some(project()),
            operation: AgentOperation::Restart {
                cutex_session_id: created.cutex_session_id.clone(),
            },
        };
        lifecycle.offline(&created.cutex_session_id).unwrap();
        seed_lifecycle_revision_conflict_receipt(&provider, &request, &created.cutex_session_id);
        let offline_effects = lifecycle.offline_count(&created.cutex_session_id);
        lifecycle.advance_before_fenced_restart_check();

        let fenced = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        assert!(matches!(
            fenced.outcome,
            AgentManagementOutcome::OwnerActionRequired { ref failure }
                if failure.detail.contains("runtime occurrence changed before the fenced restart effect")
        ));
        assert_eq!(
            lifecycle.offline_count(&created.cutex_session_id),
            offline_effects,
            "a generation change after fence commit must still prevent the offline call"
        );
        assert_eq!(
            lifecycle
                .log()
                .iter()
                .filter(|entry| entry.starts_with("fenced-restart-rejected:"))
                .count(),
            1
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn historical_restart_rechecks_fence_at_atomic_restart_claim_boundary() {
        let root = root("restart-revision-conflict-claim-race");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let created = created_agent(&completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("create", "worker", AgentStartMode::BootstrapOnly),
            &lifecycle,
        )));
        let request = AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action("restart-revision-conflict"),
            project_id: Some(project()),
            operation: AgentOperation::Restart {
                cutex_session_id: created.cutex_session_id.clone(),
            },
        };
        lifecycle.offline(&created.cutex_session_id).unwrap();
        seed_lifecycle_revision_conflict_receipt(&provider, &request, &created.cutex_session_id);
        let recoveries = lifecycle.recovery_count();
        let offline_effects = lifecycle.offline_count(&created.cutex_session_id);
        let online_effects = lifecycle.online_count(&created.cutex_session_id);
        let launches = lifecycle.launch_count();
        lifecycle.advance_after_fenced_restart_check();

        let fenced = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        assert!(matches!(
            fenced.outcome,
            AgentManagementOutcome::OwnerActionRequired { ref failure }
                if failure.detail.contains("runtime occurrence changed before the fenced restart claim")
        ));
        assert_eq!(lifecycle.recovery_count(), recoveries);
        assert_eq!(
            lifecycle.offline_count(&created.cutex_session_id),
            offline_effects
        );
        assert_eq!(
            lifecycle.online_count(&created.cutex_session_id),
            online_effects
        );
        assert_eq!(lifecycle.launch_count(), launches);
        let newer = lifecycle.observe(&created.cutex_session_id).unwrap();
        assert!(newer.app_server_runtime);
        assert_eq!(newer.runtime_generation, 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn historical_restart_rechecks_fence_before_any_runtime_recovery_effect() {
        let root = root("restart-revision-conflict-pre-recovery-race");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = Arc::new(FakeLifecycle::default());
        let created = created_agent(&completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("create", "worker", AgentStartMode::BootstrapOnly),
            lifecycle.as_ref(),
        )));
        let request = AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action("restart-revision-conflict"),
            project_id: Some(project()),
            operation: AgentOperation::Restart {
                cutex_session_id: created.cutex_session_id.clone(),
            },
        };
        lifecycle.offline(&created.cutex_session_id).unwrap();
        seed_lifecycle_revision_conflict_receipt(&provider, &request, &created.cutex_session_id);
        assert!(provider
            .store()
            .snapshot()
            .unwrap()
            .actions
            .get(&request.action_id)
            .unwrap()
            .historical_runtime_occurrence_fence
            .is_none());
        drop(provider);

        let target = created.cutex_session_id.clone();
        let barrier_action = request.action_id.clone();
        let barrier_lifecycle = Arc::clone(&lifecycle);
        let installed_occurrence = Arc::new(Mutex::new(None));
        let barrier_observation = Arc::clone(&installed_occurrence);
        let provider = AgentManagementProvider::open(&root)
            .unwrap()
            .with_phase_observer(Arc::new(move |event: &AgentManagementPhaseEvent| {
                if event.action_id == barrier_action && event.phase == AgentActionPhase::Prepared {
                    barrier_lifecycle.advance_runtime_occurrence(&target);
                    *barrier_observation.lock().unwrap() =
                        Some(barrier_lifecycle.observe(&target).unwrap());
                }
            }));
        let recoveries = lifecycle.recovery_count();
        let offline_effects = lifecycle.offline_count(&created.cutex_session_id);
        let online_effects = lifecycle.online_count(&created.cutex_session_id);
        let launches = lifecycle.launch_count();

        let fenced = provider.execute(&invocation("cutex.director"), &request, lifecycle.as_ref());
        assert!(matches!(
            fenced.outcome,
            AgentManagementOutcome::OwnerActionRequired { ref failure }
                if failure.detail.contains("runtime occurrence changed before the fenced restart effect")
        ));
        assert_eq!(
            lifecycle.recovery_count(),
            recoveries,
            "recovery must not run"
        );
        assert_eq!(
            lifecycle.offline_count(&created.cutex_session_id),
            offline_effects,
            "the newer occurrence must not receive an offline call"
        );
        assert_eq!(
            lifecycle.online_count(&created.cutex_session_id),
            online_effects,
            "the newer occurrence must not receive an online call"
        );
        assert_eq!(lifecycle.launch_count(), launches, "no child may launch");
        assert_eq!(
            lifecycle.observe(&created.cutex_session_id).unwrap(),
            installed_occurrence.lock().unwrap().clone().unwrap(),
            "the newer occurrence and its endpoint/claim evidence must remain unchanged"
        );

        drop(provider);
        let reopened = AgentManagementProvider::open(&root).unwrap();
        let replay = reopened.execute(&invocation("cutex.director"), &request, lifecycle.as_ref());
        assert_eq!(
            replay, fenced,
            "the owner-action receipt must replay exactly"
        );
        assert_eq!(lifecycle.recovery_count(), recoveries);
        assert_eq!(
            lifecycle.offline_count(&created.cutex_session_id),
            offline_effects
        );
        assert_eq!(
            lifecycle.online_count(&created.cutex_session_id),
            online_effects
        );
        assert_eq!(lifecycle.launch_count(), launches);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn historical_restart_fences_ambiguous_and_unavailable_occurrence_before_effect() {
        let root = root("restart-revision-conflict-ambiguous-occurrence");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let created = created_agent(&completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("create", "worker", AgentStartMode::BootstrapOnly),
            &lifecycle,
        )));
        let request = AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action("restart-revision-conflict"),
            project_id: Some(project()),
            operation: AgentOperation::Restart {
                cutex_session_id: created.cutex_session_id.clone(),
            },
        };
        seed_lifecycle_revision_conflict_receipt(&provider, &request, &created.cutex_session_id);
        let offline_effects = lifecycle.offline_count(&created.cutex_session_id);
        lifecycle.set_occurrence_reconciliation(
            HistoricalRuntimeOccurrenceReconciliation::Ambiguous {
                reason: "fake occurrence evidence conflicts".to_string(),
            },
        );
        let ambiguous = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        assert!(matches!(
            ambiguous.outcome,
            AgentManagementOutcome::OwnerActionRequired { ref failure }
                if failure.code == "runtime_occurrence_reconciliation_ambiguous"
        ));
        lifecycle.set_occurrence_reconciliation(
            HistoricalRuntimeOccurrenceReconciliation::Unavailable {
                reason: "fake process registry unavailable".to_string(),
            },
        );
        let unavailable = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        assert!(matches!(
            unavailable.outcome,
            AgentManagementOutcome::OwnerActionRequired { ref failure }
                if failure.code == "runtime_occurrence_reconciliation_unavailable"
        ));
        assert_eq!(
            lifecycle.offline_count(&created.cutex_session_id),
            offline_effects
        );
        assert_eq!(lifecycle.launch_count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn historical_revision_conflict_replay_fences_authority_and_identity_drift() {
        let root = root("restart-revision-conflict-fences");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let created = created_agent(&completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("create", "worker", AgentStartMode::BootstrapOnly),
            &lifecycle,
        )));
        let request = AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action("restart-revision-conflict"),
            project_id: Some(project()),
            operation: AgentOperation::Restart {
                cutex_session_id: created.cutex_session_id.clone(),
            },
        };
        let original = seed_lifecycle_revision_conflict_receipt(
            &provider,
            &request,
            &created.cutex_session_id,
        );
        lifecycle.corrupt_native_session_id(&created.cutex_session_id);
        let fenced = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        assert!(matches!(
            fenced.outcome,
            AgentManagementOutcome::OwnerActionRequired { .. }
        ));
        assert_eq!(lifecycle.launch_count(), 1);

        bind(
            &provider,
            "rebind",
            "cutex.director-new",
            Some(("cutex.director", 1)),
        );
        let unauthorized = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        assert!(matches!(
            unauthorized.outcome,
            AgentManagementOutcome::NoWrite { ref code, .. }
                if code == "not_authorized_director"
        ));
        assert_ne!(
            fenced, original,
            "typed reconciliation reason is observable"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn online_recovers_exact_live_claim_without_launching_a_child() {
        let root = root("online-recover-live");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let created = created_agent(&completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("create", "worker", AgentStartMode::BootstrapOnly),
            &lifecycle,
        )));
        let generation = lifecycle
            .observe(&created.cutex_session_id)
            .unwrap()
            .runtime_generation;
        let launches = lifecycle.launch_count();
        let recoveries = lifecycle.recovery_count();
        let authority = provider.store().snapshot().unwrap().projects;
        lifecycle.disconnect_with_claim(&created.cutex_session_id, FakeRecovery::ExactLive);
        let request = AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action("online-recover-live"),
            project_id: Some(project()),
            operation: AgentOperation::Online {
                cutex_session_id: created.cutex_session_id.clone(),
            },
        };

        let first = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        let replay = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        assert_eq!(first, replay);
        let receipt = completed(first);
        let AgentManagementResult::Lifecycle { observation, .. } = receipt.result else {
            panic!("expected lifecycle result")
        };
        assert_eq!(observation.runtime_generation, generation);
        assert_eq!(lifecycle.launch_count(), launches);
        assert_eq!(lifecycle.recovery_count(), recoveries + 1);
        assert_eq!(lifecycle.message_count(), 0);
        assert_eq!(provider.store().snapshot().unwrap().projects, authority);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_unmigrated_online_and_restart_refuse_before_any_write_or_recovery() {
        let root = root("online-legacy-unmigrated");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let created = created_agent(&completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("create", "worker", AgentStartMode::BootstrapOnly),
            &lifecycle,
        )));
        // Legacy records lack later spec fields; the old error was a
        // misleading `runtime recovery mismatch: sandbox_mode`.
        lifecycle
            .state
            .lock()
            .unwrap()
            .agents
            .get_mut(&created.cutex_session_id)
            .unwrap()
            .sandbox_mode
            .clear();
        lifecycle.mark_legacy_unmigrated(&created.cutex_session_id);
        let recoveries = lifecycle.recovery_count();
        let before = provider.store().snapshot().unwrap();
        for (name, operation) in [
            (
                "online-legacy",
                AgentOperation::Online {
                    cutex_session_id: created.cutex_session_id.clone(),
                },
            ),
            (
                "restart-legacy",
                AgentOperation::Restart {
                    cutex_session_id: created.cutex_session_id.clone(),
                },
            ),
        ] {
            let response = provider.execute(
                &invocation("cutex.director"),
                &AgentManagementRequest {
                    schema: AgentManagementSchema::V1,
                    action_id: action(name),
                    project_id: Some(project()),
                    operation,
                },
                &lifecycle,
            );
            match response.outcome {
                AgentManagementOutcome::NoWrite { ref code, ref detail } => {
                    assert_eq!(code, "migration_required");
                    assert!(detail.contains("owner must migrate or archive"), "{detail}");
                }
                other => panic!("expected migration_required, got {other:?}"),
            }
            assert!(!provider
                .store()
                .snapshot()
                .unwrap()
                .actions
                .contains_key(&action(name)));
        }
        assert_eq!(lifecycle.recovery_count(), recoveries);
        assert_eq!(provider.store().snapshot().unwrap().actions, before.actions);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn online_recovery_accepts_empty_or_changed_observed_profile() {
        for (label, observed_profile) in [("empty", ""), ("changed", "quince")] {
            let root = root(&format!("online-recover-profile-{label}"));
            let provider = AgentManagementProvider::open(&root).unwrap();
            bind(&provider, "bind", "cutex.director", None);
            let lifecycle = FakeLifecycle::default();
            let created = created_agent(&completed(provider.execute(
                &invocation("cutex.director"),
                &create_request("create", "worker", AgentStartMode::BootstrapOnly),
                &lifecycle,
            )));
            assert_eq!(created.spec.profile.as_deref(), Some("orchid"));
            let generation = lifecycle
                .observe(&created.cutex_session_id)
                .unwrap()
                .runtime_generation;
            let launches = lifecycle.launch_count();
            let recoveries = lifecycle.recovery_count();
            lifecycle.set_profile(&created.cutex_session_id, observed_profile);
            lifecycle.disconnect_with_claim(&created.cutex_session_id, FakeRecovery::ExactLive);
            let request = AgentManagementRequest {
                schema: AgentManagementSchema::V1,
                action_id: action(&format!("online-recover-profile-{label}")),
                project_id: Some(project()),
                operation: AgentOperation::Online {
                    cutex_session_id: created.cutex_session_id.clone(),
                },
            };

            let receipt =
                completed(provider.execute(&invocation("cutex.director"), &request, &lifecycle));
            let AgentManagementResult::Lifecycle { observation, .. } = receipt.result else {
                panic!("expected lifecycle result")
            };
            assert_eq!(observation.profile, observed_profile);
            assert_eq!(observation.runtime_generation, generation);
            assert_eq!(lifecycle.launch_count(), launches);
            assert_eq!(lifecycle.recovery_count(), recoveries + 1);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn online_fences_dead_claim_before_one_replacement_launch() {
        let root = root("online-recover-dead");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let created = created_agent(&completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("create", "worker", AgentStartMode::BootstrapOnly),
            &lifecycle,
        )));
        let generation = lifecycle
            .observe(&created.cutex_session_id)
            .unwrap()
            .runtime_generation;
        let launches = lifecycle.launch_count();
        let recoveries = lifecycle.recovery_count();
        lifecycle.disconnect_with_claim(&created.cutex_session_id, FakeRecovery::DeadClaim);
        let request = AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action("online-recover-dead"),
            project_id: Some(project()),
            operation: AgentOperation::Online {
                cutex_session_id: created.cutex_session_id.clone(),
            },
        };

        let first = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        let replay = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        assert_eq!(first, replay);
        let receipt = completed(first);
        let AgentManagementResult::Lifecycle { observation, .. } = receipt.result else {
            panic!("expected lifecycle result")
        };
        assert_eq!(observation.runtime_generation, generation + 1);
        assert_eq!(lifecycle.launch_count(), launches + 1);
        assert_eq!(lifecycle.recovery_count(), recoveries + 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ambiguous_recovery_fails_closed_and_exact_replay_is_immutable() {
        let root = root("online-recover-ambiguous");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let created = created_agent(&completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("create", "worker", AgentStartMode::BootstrapOnly),
            &lifecycle,
        )));
        let launches = lifecycle.launch_count();
        let recoveries = lifecycle.recovery_count();
        lifecycle.disconnect_with_claim(&created.cutex_session_id, FakeRecovery::Ambiguous);
        let request = AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action("online-recover-ambiguous"),
            project_id: Some(project()),
            operation: AgentOperation::Online {
                cutex_session_id: created.cutex_session_id.clone(),
            },
        };

        let first = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        let replay = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        assert_eq!(first, replay);
        assert!(matches!(
            first.outcome,
            AgentManagementOutcome::OwnerActionRequired { .. }
        ));
        assert_eq!(lifecycle.recovery_count(), recoveries + 1);
        assert_eq!(lifecycle.launch_count(), launches);
        assert_eq!(lifecycle.message_count(), 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recovered_runtime_with_a_different_generation_fails_closed() {
        let root = root("online-recover-generation-mismatch");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let created = created_agent(&completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("create", "worker", AgentStartMode::BootstrapOnly),
            &lifecycle,
        )));
        let launches = lifecycle.launch_count();
        let recoveries = lifecycle.recovery_count();
        lifecycle.disconnect_with_claim(
            &created.cutex_session_id,
            FakeRecovery::MismatchedGeneration,
        );
        let request = AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action("online-recover-generation-mismatch"),
            project_id: Some(project()),
            operation: AgentOperation::Online {
                cutex_session_id: created.cutex_session_id.clone(),
            },
        };

        let first = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        let replay = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        assert_eq!(first, replay);
        assert!(matches!(
            first.outcome,
            AgentManagementOutcome::OwnerActionRequired { .. }
        ));
        assert_eq!(lifecycle.recovery_count(), recoveries + 1);
        assert_eq!(lifecycle.launch_count(), launches);
        assert_eq!(lifecycle.message_count(), 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn restart_adopts_exact_claim_before_launching_one_new_generation() {
        let root = root("restart-recover-live");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let created = created_agent(&completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("create", "worker", AgentStartMode::BootstrapOnly),
            &lifecycle,
        )));
        let generation = lifecycle
            .observe(&created.cutex_session_id)
            .unwrap()
            .runtime_generation;
        let launches = lifecycle.launch_count();
        let recoveries = lifecycle.recovery_count();
        lifecycle.disconnect_with_claim(&created.cutex_session_id, FakeRecovery::ExactLive);
        let request = AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action("restart-recover-live"),
            project_id: Some(project()),
            operation: AgentOperation::Restart {
                cutex_session_id: created.cutex_session_id.clone(),
            },
        };

        let first = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        let replay = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        assert_eq!(first, replay);
        let receipt = completed(first);
        let AgentManagementResult::Lifecycle { observation, .. } = receipt.result else {
            panic!("expected lifecycle result")
        };
        assert_eq!(observation.runtime_generation, generation + 1);
        assert_eq!(lifecycle.launch_count(), launches + 1);
        assert_eq!(lifecycle.recovery_count(), recoveries + 1);
        assert_eq!(lifecycle.message_count(), 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replace_is_policy_ordered_composition_without_seat_state() {
        let root = root("replace");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let predecessor = created_agent(&completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("create", "worker-old", AgentStartMode::BootstrapOnly),
            &lifecycle,
        )));
        let before = lifecycle.log().len();
        let replace = AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action("replace"),
            project_id: Some(project()),
            operation: AgentOperation::Replace {
                bootstrap_intent: None,
                predecessor_cutex_session_id: predecessor.cutex_session_id.clone(),
                policy: AgentReplacePolicy::CloseBeforeCreate,
                successor: spec("worker-new"),
                start_mode: AgentStartMode::BootstrapOnly,
                frozen_message: None,
            },
        };
        completed(provider.execute(&invocation("cutex.director"), &replace, &lifecycle));
        let log = lifecycle.log();
        let steps = &log[before..];
        assert!(steps[0].starts_with("offline:"));
        assert!(steps[1].starts_with("archive:"));
        assert_eq!(steps[2], "prepare:worker-new");
        let snapshot = provider.store().snapshot().unwrap();
        assert_eq!(
            snapshot.projects.get(&project()).unwrap().authority_epoch,
            1
        );
        let replace_phases = snapshot
            .phase_events
            .values()
            .filter(|event| event.action_id == replace.action_id)
            .collect::<Vec<_>>();
        assert!(replace_phases
            .iter()
            .any(|event| event.phase == AgentActionPhase::PredecessorClosed));
        assert!(replace_phases.iter().all(|event| {
            event.predecessor_cutex_session_id.as_ref() == Some(&predecessor.cutex_session_id)
                && event.replace_policy == Some(AgentReplacePolicy::CloseBeforeCreate)
                && event.rotation_mode.is_none()
        }));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replace_close_policies_recover_after_process_loss_without_duplicate_successors_or_messages()
    {
        for (label, policy) in [
            (
                "replace-close-before-crash",
                AgentReplacePolicy::CloseBeforeCreate,
            ),
            (
                "replace-close-after-crash",
                AgentReplacePolicy::CloseAfterReady,
            ),
        ] {
            let root = root(label);
            let provider = AgentManagementProvider::open(&root)
                .unwrap()
                .with_fail_after_predecessor_close_once();
            bind(&provider, "bind", "cutex.director", None);
            let lifecycle = Arc::new(FakeLifecycle::default());
            let predecessor = created_agent(&completed(provider.execute(
                &invocation("cutex.director"),
                &create_request("create-old", "worker-old", AgentStartMode::BootstrapOnly),
                lifecycle.as_ref(),
            )));
            let request = AgentManagementRequest {
                schema: AgentManagementSchema::V1,
                action_id: action("replace-crash"),
                project_id: Some(project()),
                operation: AgentOperation::Replace {
                    bootstrap_intent: None,
                    predecessor_cutex_session_id: predecessor.cutex_session_id.clone(),
                    policy,
                    successor: spec("worker-new"),
                    start_mode: AgentStartMode::CustomMessage,
                    frozen_message: Some("Exact replacement handoff.".to_string()),
                },
            };

            let interrupted =
                provider.execute(&invocation("cutex.director"), &request, lifecycle.as_ref());
            assert!(matches!(
                interrupted.outcome,
                AgentManagementOutcome::NoWrite { ref code, .. }
                    if code == "injected_process_loss"
            ));
            let interrupted_state = provider.store().snapshot().unwrap();
            assert_eq!(
                interrupted_state.actions[&request.action_id].phase,
                AgentActionPhase::PredecessorClosing
            );
            assert!(interrupted_state.agents[&predecessor.cutex_session_id]
                .retired_at
                .is_none());
            let externally_retired = lifecycle.observe(&predecessor.cutex_session_id).unwrap();
            assert!(!externally_retired.active);
            assert!(externally_retired.agent_bus_endpoint_ids.is_empty());
            assert_eq!(lifecycle.retire_count(&predecessor.cutex_session_id), 0);

            let bypass =
                create_request("fresh-bypass", "worker-new", AgentStartMode::BootstrapOnly);
            assert!(matches!(
                provider
                    .execute(&invocation("cutex.director"), &bypass, lifecycle.as_ref())
                    .outcome,
                AgentManagementOutcome::NoWrite { ref code, ref detail }
                    if code == "unresolved_agent_reservation" && detail.contains("unfinished action")
            ));

            drop(provider);
            let reopened = Arc::new(AgentManagementProvider::open(&root).unwrap());
            let mut threads = Vec::new();
            for _ in 0..2 {
                let provider = Arc::clone(&reopened);
                let lifecycle = Arc::clone(&lifecycle);
                let request = request.clone();
                threads.push(std::thread::spawn(move || {
                    provider.execute(&invocation("cutex.director"), &request, lifecycle.as_ref())
                }));
            }
            let first = threads.remove(0).join().unwrap();
            let second = threads.remove(0).join().unwrap();
            assert_eq!(first, second);
            let receipt = completed(first);
            let AgentManagementResult::Replaced { successor, .. } = receipt.result else {
                panic!("expected replacement receipt")
            };
            assert_eq!(successor.spec, spec("worker-new"));
            assert_eq!(lifecycle.bootstrap_count(), 2);
            assert_eq!(lifecycle.message_count(), 1);
            assert_eq!(lifecycle.retire_count(&predecessor.cutex_session_id), 0);
            let final_state = reopened.store().snapshot().unwrap();
            assert_eq!(
                final_state.actions[&request.action_id].phase,
                AgentActionPhase::Complete
            );
            assert!(final_state.agents[&predecessor.cutex_session_id]
                .retired_at
                .is_none());
            assert_eq!(
                final_state
                    .agents
                    .values()
                    .filter(|agent| agent.retired_at.is_none())
                    .count(),
                2
            );
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn predecessor_close_reconciliation_fences_identity_changes() {
        let mismatch_root = root("replace-close-mismatch");
        let mismatch_provider = AgentManagementProvider::open(&mismatch_root)
            .unwrap()
            .with_fail_after_predecessor_close_once();
        bind(&mismatch_provider, "bind", "cutex.director", None);
        let mismatch_lifecycle = FakeLifecycle::default();
        let predecessor = created_agent(&completed(mismatch_provider.execute(
            &invocation("cutex.director"),
            &create_request("create-old", "worker-old", AgentStartMode::BootstrapOnly),
            &mismatch_lifecycle,
        )));
        let replace = AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action("replace-mismatch"),
            project_id: Some(project()),
            operation: AgentOperation::Replace {
                bootstrap_intent: None,
                predecessor_cutex_session_id: predecessor.cutex_session_id.clone(),
                policy: AgentReplacePolicy::CloseBeforeCreate,
                successor: spec("worker-new"),
                start_mode: AgentStartMode::BootstrapOnly,
                frozen_message: None,
            },
        };
        mismatch_provider.execute(&invocation("cutex.director"), &replace, &mismatch_lifecycle);
        mismatch_lifecycle.corrupt_native_session_id(&predecessor.cutex_session_id);
        assert!(matches!(
            AgentManagementProvider::open(&mismatch_root)
                .unwrap()
                .execute(&invocation("cutex.director"), &replace, &mismatch_lifecycle)
                .outcome,
            AgentManagementOutcome::OwnerActionRequired { .. }
        ));
        assert_eq!(mismatch_lifecycle.bootstrap_count(), 1);
        std::fs::remove_dir_all(mismatch_root).unwrap();
    }

    #[test]
    fn deterministic_pre_effect_no_write_releases_only_the_cross_action_reservation() {
        let root = root("pre-effect-no-write");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let predecessor = created_agent(&completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("create-old", "worker-old", AgentStartMode::BootstrapOnly),
            &lifecycle,
        )));
        let taken = created_agent(&completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("create-taken", "worker-taken", AgentStartMode::BootstrapOnly),
            &lifecycle,
        )));
        let replace = |action_id: &str, successor: &str| AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action(action_id),
            project_id: Some(project()),
            operation: AgentOperation::Replace {
                bootstrap_intent: None,
                predecessor_cutex_session_id: predecessor.cutex_session_id.clone(),
                policy: AgentReplacePolicy::KeepOld,
                successor: spec(successor),
                start_mode: AgentStartMode::BootstrapOnly,
                frozen_message: None,
            },
        };

        // The successor name collides with an active member: a deterministic
        // pre-effect rejection.
        let stale = replace("stale-replace", "worker-taken");
        let first = provider.execute(&invocation("cutex.director"), &stale, &lifecycle);
        assert!(matches!(
            first.outcome,
            AgentManagementOutcome::NoWrite { ref code, .. } if code == "active_agent_name_taken"
        ));
        let snapshot = provider.store().snapshot().unwrap();
        let stale_action = &snapshot.actions[&stale.action_id];
        assert_eq!(stale_action.phase, AgentActionPhase::NoWrite);
        assert!(stale_action.response.is_some());
        assert_eq!(
            provider.execute(&invocation("cutex.director"), &stale, &lifecycle),
            first,
            "exact replay returns the original terminal NoWrite"
        );

        let changed_same_action = replace("stale-replace", "worker-other");
        assert!(matches!(
            provider
                .execute(&invocation("cutex.director"), &changed_same_action, &lifecycle)
                .outcome,
            AgentManagementOutcome::NoWrite { ref code, .. } if code == "action_id_payload_conflict"
        ));

        // Once the name is free, a fresh action may reserve it: the terminal
        // NoWrite released only its own cross-action reservation.
        completed(provider.execute(
            &invocation("cutex.director"),
            &AgentManagementRequest {
                schema: AgentManagementSchema::V1,
                action_id: action("close-taken"),
                project_id: Some(project()),
                operation: AgentOperation::Close {
                    cutex_session_id: taken.cutex_session_id,
                },
            },
            &lifecycle,
        ));
        let corrected_receipt = completed(provider.execute(
            &invocation("cutex.director"),
            &replace("corrected-replace", "worker-taken"),
            &lifecycle,
        ));
        assert!(matches!(
            corrected_receipt.result,
            AgentManagementResult::Replaced { .. }
        ));
        assert_eq!(lifecycle.bootstrap_count(), 3);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn definite_active_collision_releases_reservation_for_a_corrected_fresh_action() {
        let root = root("active-collision-no-write");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let taken = created_agent(&completed(provider.execute(
            &invocation("cutex.director"),
            &create_request(
                "create-taken",
                "worker-taken",
                AgentStartMode::BootstrapOnly,
            ),
            &lifecycle,
        )));

        let collision = create_request(
            "create-collision",
            "worker-taken",
            AgentStartMode::BootstrapOnly,
        );
        assert!(matches!(
            provider
                .execute(&invocation("cutex.director"), &collision, &lifecycle)
                .outcome,
            AgentManagementOutcome::NoWrite { ref code, .. } if code == "active_agent_name_taken"
        ));
        assert_eq!(
            provider.store().snapshot().unwrap().actions[&collision.action_id].phase,
            AgentActionPhase::NoWrite
        );
        let close = AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action("close-taken"),
            project_id: Some(project()),
            operation: AgentOperation::Close {
                cutex_session_id: taken.cutex_session_id,
            },
        };
        completed(provider.execute(&invocation("cutex.director"), &close, &lifecycle));
        completed(provider.execute(
            &invocation("cutex.director"),
            &create_request(
                "create-corrected",
                "worker-taken",
                AgentStartMode::BootstrapOnly,
            ),
            &lifecycle,
        ));
        assert_eq!(lifecycle.bootstrap_count(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn no_write_detail(response: &AgentManagementResponse) -> (String, String) {
        match &response.outcome {
            AgentManagementOutcome::NoWrite { code, detail } => (code.clone(), detail.clone()),
            other => panic!("expected NoWrite, got {other:?}"),
        }
    }

    /// 2026-10-11: a Director and its members share the project workspace;
    /// only the active name is unique.
    #[test]
    fn agents_may_share_a_cwd_and_keep_separate_native_identities() {
        let root = root("shared-cwd");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let workspace = test_agent_cwd("project-workspace");
        let create = |action_id: &str, name: &str| {
            let mut request = create_request(action_id, name, AgentStartMode::BootstrapOnly);
            if let AgentOperation::Create { spec, .. } = &mut request.operation {
                spec.cwd = workspace.clone();
            }
            provider.execute(&invocation("cutex.director"), &request, &lifecycle)
        };
        let first = created_agent(&completed(create("create-a", "worker-a")));
        let second = created_agent(&completed(create("create-b", "worker-b")));
        assert_eq!(first.spec.cwd, second.spec.cwd);
        assert_ne!(first.cutex_session_id, second.cutex_session_id);
        assert_ne!(first.native_session_id, second.native_session_id);
        assert_eq!(lifecycle.bootstrap_count(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn active_name_collision_names_the_holder_and_suggests_a_free_name() {
        let root = root("name-taken-detail");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind_project(&provider, "bind-alpha", "alpha", "cutex.director-a", None);
        bind_project(&provider, "bind-beta", "beta", "cutex.director-b", None);
        let lifecycle = FakeLifecycle::default();
        let holder = created_agent(&completed(provider.execute(
            &invocation("cutex.director-a"),
            &create_request("create-worker", "worker", AgentStartMode::BootstrapOnly),
            &lifecycle,
        )));
        completed(provider.execute(
            &invocation("cutex.director-a"),
            &create_request("create-worker-2", "worker-2", AgentStartMode::BootstrapOnly),
            &lifecycle,
        ));

        // Same project: the holder is visible; `worker-2` is taken too.
        let mut same = create_request("same-name", "worker", AgentStartMode::BootstrapOnly);
        if let AgentOperation::Create { spec, .. } = &mut same.operation {
            spec.cwd = test_agent_cwd("somewhere-else");
        }
        let (code, detail) = no_write_detail(&provider.execute(
            &invocation("cutex.director-a"),
            &same,
            &lifecycle,
        ));
        assert_eq!(code, "active_agent_name_taken");
        assert!(detail.starts_with("the name 'worker' is held by"), "{detail}");
        assert!(!detail.contains("conflict:") && !detail.contains("active_agent_name_taken"), "{detail}");
        assert!(detail.contains("'worker'"), "{detail}");
        assert!(
            detail.contains(&crate::short_ids::display(holder.cutex_session_id.as_str())),
            "{detail}"
        );
        assert!(detail.contains("it is visible to you"), "{detail}");
        assert!(detail.contains("such as 'worker-3'"), "{detail}");

        // Another project: the holder is named but reported as not visible.
        let (code, detail) = no_write_detail(&provider.execute(
            &invocation("cutex.director-b"),
            &create_request("other-project", "worker", AgentStartMode::BootstrapOnly),
            &lifecycle,
        ));
        assert_eq!(code, "active_agent_name_taken");
        assert!(detail.contains("it is not visible to you"), "{detail}");
        // The holder's project is named even though it is not visible.
        assert!(detail.contains(&format!("project {}", holder.project_id.as_ref().unwrap().as_str())), "{detail}");
        assert_eq!(lifecycle.bootstrap_count(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replace_may_reuse_the_predecessor_name_only_when_closing_it_first() {
        let root = root("replace-same-name");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let predecessor = created_agent(&completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("create-old", "worker", AgentStartMode::BootstrapOnly),
            &lifecycle,
        )));
        let replace = |action_id: &str, policy: AgentReplacePolicy| AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action(action_id),
            project_id: Some(project()),
            operation: AgentOperation::Replace {
                bootstrap_intent: None,
                predecessor_cutex_session_id: predecessor.cutex_session_id.clone(),
                policy,
                successor: spec("worker"),
                start_mode: AgentStartMode::BootstrapOnly,
                frozen_message: None,
            },
        };
        let (code, detail) = no_write_detail(&provider.execute(
            &invocation("cutex.director"),
            &replace("keep-old", AgentReplacePolicy::KeepOld),
            &lifecycle,
        ));
        assert_eq!(code, "active_agent_name_taken");
        assert!(detail.contains("such as 'worker-2'"), "{detail}");
        let receipt = completed(provider.execute(
            &invocation("cutex.director"),
            &replace("close-first", AgentReplacePolicy::CloseBeforeCreate),
            &lifecycle,
        ));
        assert!(matches!(receipt.result, AgentManagementResult::Replaced { .. }));
        std::fs::remove_dir_all(root).unwrap();
    }

    /// On Windows a Claude member's durable cwd was the Windows
    /// verbatim spelling `\\?\F:\cutex-test\r244` of its spec cwd
    /// `F:\cutex-test\r244`; `member_runtime restart` failed with `runtime
    /// recovery mismatch: cwd` and `member_replace` with `managed Agent
    /// observation mismatch: cwd`.
    #[test]
    fn restart_and_replace_accept_a_verbatim_spelling_of_the_spec_cwd() {
        let root = root("verbatim-cwd");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let created = created_agent(&completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("create", "worker", AgentStartMode::BootstrapOnly),
            &lifecycle,
        )));
        let verbatim = format!(r"\\?\{}", created.spec.cwd);
        {
            // The durable record (configured with the spec groups) carries
            // the verbatim cwd; the runtime adds the cwd group of that text.
            let mut state = lifecycle.state.lock().unwrap();
            let observed = state.agents.get_mut(&created.cutex_session_id).unwrap();
            observed.cwd = verbatim.clone();
            observed.groups = created.spec.groups.clone();
        }
        let restart = AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action("restart"),
            project_id: Some(project()),
            operation: AgentOperation::Restart {
                cutex_session_id: created.cutex_session_id.clone(),
            },
        };
        completed(provider.execute(&invocation("cutex.director"), &restart, &lifecycle));
        let replace = AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action("replace"),
            project_id: Some(project()),
            operation: AgentOperation::Replace {
                bootstrap_intent: None,
                predecessor_cutex_session_id: created.cutex_session_id.clone(),
                policy: AgentReplacePolicy::CloseBeforeCreate,
                successor: spec("worker"),
                start_mode: AgentStartMode::BootstrapOnly,
                frozen_message: None,
            },
        };
        let receipt = completed(provider.execute(&invocation("cutex.director"), &replace, &lifecycle));
        assert!(matches!(receipt.result, AgentManagementResult::Replaced { .. }));
        // A different directory is still a mismatch.
        let mut other = ready_observation(&created);
        other.cwd = format!(r"\\?\{}", test_agent_cwd("elsewhere"));
        assert!(validate_ready(&created, &other)
            .unwrap_err()
            .to_string()
            .contains("mismatch: cwd"));
        std::fs::remove_dir_all(root).unwrap();
    }

    /// r244: a `member_replace` stopped while checking its predecessor kept
    /// the successor name reserved, so the next `agent_create` of that name
    /// failed with `unresolved_agent_reservation` and the owner had no
    /// allowlisted way to clear it. Nothing of the successor exists yet: a
    /// new request for the name releases the reservation.
    #[test]
    fn replace_stopped_before_its_successor_releases_the_name() {
        let root = root("replace-release");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let predecessor = created_agent(&completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("create-old", "worker", AgentStartMode::BootstrapOnly),
            &lifecycle,
        )));
        // The predecessor no longer matches its ownership record.
        lifecycle
            .state
            .lock()
            .unwrap()
            .agents
            .get_mut(&predecessor.cutex_session_id)
            .unwrap()
            .model = "drifted-model".to_string();
        let replace = AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action("replace-stuck"),
            project_id: Some(project()),
            operation: AgentOperation::Replace {
                bootstrap_intent: None,
                predecessor_cutex_session_id: predecessor.cutex_session_id.clone(),
                policy: AgentReplacePolicy::CloseBeforeCreate,
                successor: spec("worker-next"),
                start_mode: AgentStartMode::BootstrapOnly,
                frozen_message: None,
            },
        };
        match provider.execute(&invocation("cutex.director"), &replace, &lifecycle).outcome {
            AgentManagementOutcome::OwnerActionRequired { failure } => {
                assert!(failure.detail.contains("mismatch: model"), "{}", failure.detail)
            }
            other => panic!("unexpected replace outcome: {other:?}"),
        }
        let stuck = provider.store().snapshot().unwrap().actions[&replace.action_id].clone();
        assert_eq!(stuck.phase, AgentActionPhase::OwnerActionRequired);
        assert_eq!(stuck.reserved_agent_name.as_deref(), Some("worker-next"));

        completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("create-next", "worker-next", AgentStartMode::BootstrapOnly),
            &lifecycle,
        ));
        let released = provider.store().snapshot().unwrap().actions[&replace.action_id].clone();
        assert_eq!(released.phase, AgentActionPhase::NoWrite);
        assert!(matches!(
            released.response.map(|response| response.outcome),
            Some(AgentManagementOutcome::NoWrite { ref code, ref detail })
                if code == "reservation_released_pre_effect"
                    && detail.contains("before creating its successor")
        ));
        // The predecessor itself is untouched.
        assert!(provider.store().snapshot().unwrap().agents[&predecessor.cutex_session_id]
            .retired_at
            .is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Clients render `code: detail`; the detail must not repeat the code.
    #[test]
    fn no_write_detail_does_not_repeat_the_code() {
        let root = root("no-doubled-code");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let mut request = create_request("bad-groups", "worker", AgentStartMode::BootstrapOnly);
        if let AgentOperation::Create { spec, .. } = &mut request.operation {
            spec.groups = Vec::new();
        }
        let (code, detail) = no_write_detail(&provider.execute(
            &invocation("cutex.director"),
            &request,
            &lifecycle,
        ));
        assert_eq!((code.as_str(), detail.as_str()), ("invalid_agent_groups", "invalid_request"));
        assert_eq!(
            AgentManagementError::Conflict("stale_project_authority").wire(),
            ("stale_project_authority".to_string(), "conflict".to_string())
        );
        assert_eq!(
            AgentManagementError::ConflictDetail("x: y".into()).to_string(),
            "conflict: x: y"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A Create parked retryable after a definite pre-spawn bootstrap
    /// failure (older releases validated configuration there) holds no native
    /// effect: a new Create of the same name releases it.
    #[test]
    fn pre_spawn_bootstrap_failure_reservation_is_released_by_a_new_create() {
        let root = root("pre-spawn-release");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle {
            state: Mutex::new(FakeState {
                definite_pre_sid_bootstrap_failures: 1,
                definite_pre_sid_code: Some("native_bootstrap_preflight_failed"),
                ..FakeState::default()
            }),
        };
        let stuck = create_request("probe-create", "worker", AgentStartMode::BootstrapOnly);
        assert!(matches!(
            provider.execute(&invocation("cutex.director"), &stuck, &lifecycle).outcome,
            AgentManagementOutcome::OwnerActionRequired { .. }
        ));
        let parked = provider.store().snapshot().unwrap().actions[&stuck.action_id].clone();
        assert_eq!(parked.phase, AgentActionPhase::NativeBootstrapPending);
        assert!(parked.native_bootstrap_retryable);

        completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("fresh-create", "worker", AgentStartMode::BootstrapOnly),
            &lifecycle,
        ));
        let released = provider.store().snapshot().unwrap().actions[&stuck.action_id].clone();
        assert_eq!(released.phase, AgentActionPhase::NoWrite);
        assert!(!released.native_bootstrap_retryable);
        assert!(matches!(
            released.response.map(|response| response.outcome),
            Some(AgentManagementOutcome::NoWrite { ref code, ref detail })
                if code == "reservation_released_pre_effect"
                    && detail.contains("native_bootstrap_preflight_failed")
        ));

        // An uncertain or other bootstrap failure still blocks the name.
        let other = FakeLifecycle::fail_bootstrap_once_before_sid();
        let blocked = create_request("other-create", "worker-x", AgentStartMode::BootstrapOnly);
        provider.execute(&invocation("cutex.director"), &blocked, &other);
        let (code, _) = no_write_detail(&provider.execute(
            &invocation("cutex.director"),
            &create_request("other-fresh", "worker-x", AgentStartMode::BootstrapOnly),
            &other,
        ));
        assert_eq!(code, "unresolved_agent_reservation");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn definite_pre_sid_failure_retries_exact_create_after_external_repair() {
        let root = root("pre-sid-retry");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::fail_bootstrap_once_before_sid();
        let request = create_request("create", "worker", AgentStartMode::CustomMessage);

        let failed = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        assert!(matches!(
            failed.outcome,
            AgentManagementOutcome::OwnerActionRequired { .. }
        ));
        let snapshot = provider.store().snapshot().unwrap();
        let pending = &snapshot.actions[&request.action_id];
        assert_eq!(pending.phase, AgentActionPhase::NativeBootstrapPending);
        assert!(pending.native_bootstrap_retryable);
        assert!(pending.response.is_none());
        assert!(pending.known_native_session_id.is_none());
        assert!(pending.known_successor_cutex_session.is_none());

        let receipt =
            completed(provider.execute(&invocation("cutex.director"), &request, &lifecycle));
        let agent = created_agent(&receipt);
        assert_eq!(agent.spec, spec("worker"));
        assert_eq!(lifecycle.bootstrap_count(), 2);
        assert_eq!(lifecycle.message_count(), 1);
        assert_eq!(provider.store().snapshot().unwrap().agents.len(), 1);
        let complete = provider.store().snapshot().unwrap().actions[&request.action_id].clone();
        assert_eq!(complete.phase, AgentActionPhase::Complete);
        assert!(!complete.native_bootstrap_retryable);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pre_sid_retry_survives_service_restart() {
        let root = root("pre-sid-restart");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::fail_bootstrap_once_before_sid();
        let request = create_request("create", "worker", AgentStartMode::BootstrapOnly);
        assert!(matches!(
            provider
                .execute(&invocation("cutex.director"), &request, &lifecycle)
                .outcome,
            AgentManagementOutcome::OwnerActionRequired { .. }
        ));
        drop(provider);

        let reopened = AgentManagementProvider::open(&root).unwrap();
        let receipt =
            completed(reopened.execute(&invocation("cutex.director"), &request, &lifecycle));
        assert_eq!(created_agent(&receipt).spec, spec("worker"));
        assert_eq!(lifecycle.bootstrap_count(), 2);
        assert_eq!(reopened.store().snapshot().unwrap().agents.len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_exact_pre_sid_retries_launch_only_one_replacement_attempt() {
        let root = root("pre-sid-concurrent");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = Arc::new(FakeLifecycle::fail_bootstrap_once_before_sid());
        let request = create_request("create", "worker", AgentStartMode::CustomMessage);
        assert!(matches!(
            provider
                .execute(&invocation("cutex.director"), &request, lifecycle.as_ref())
                .outcome,
            AgentManagementOutcome::OwnerActionRequired { .. }
        ));
        drop(provider);

        let barrier = Arc::new(std::sync::Barrier::new(3));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let root = root.clone();
            let request = request.clone();
            let lifecycle = lifecycle.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                let provider = AgentManagementProvider::open(root).unwrap();
                barrier.wait();
                provider.execute(&invocation("cutex.director"), &request, lifecycle.as_ref())
            }));
        }
        barrier.wait();
        let responses = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(responses[0], responses[1]);
        assert!(responses
            .iter()
            .all(|response| matches!(response.outcome, AgentManagementOutcome::Complete { .. })));
        assert_eq!(lifecycle.bootstrap_count(), 2);
        assert_eq!(lifecycle.message_count(), 1);
        assert_eq!(
            AgentManagementProvider::open(&root)
                .unwrap()
                .store()
                .snapshot()
                .unwrap()
                .agents
                .len(),
            1
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn uncertain_pre_sid_outcome_remains_fenced_and_never_relaunches() {
        let root = root("pre-sid-uncertain");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::fail_bootstrap_once_with_uncertain_sid();
        let request = create_request("create", "worker", AgentStartMode::BootstrapOnly);

        let first = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        assert!(matches!(
            first.outcome,
            AgentManagementOutcome::OwnerActionRequired { .. }
        ));
        let fenced = provider.store().snapshot().unwrap().actions[&request.action_id].clone();
        assert_eq!(fenced.phase, AgentActionPhase::OwnerActionRequired);
        assert!(!fenced.native_bootstrap_retryable);
        assert!(fenced.response.is_some());
        let mut accepted_snapshot =
            serde_json::to_value(provider.store().snapshot().unwrap()).unwrap();
        accepted_snapshot["actions"][request.action_id.as_str()]
            .as_object_mut()
            .unwrap()
            .remove("native_bootstrap_retryable");
        let decoded: AgentManagementSnapshot = serde_json::from_value(accepted_snapshot)
            .expect("accepted action without the R23 retry field remains readable");
        assert!(!decoded.actions[&request.action_id].native_bootstrap_retryable);
        assert_eq!(
            provider.execute(&invocation("cutex.director"), &request, &lifecycle),
            first
        );
        assert_eq!(lifecycle.bootstrap_count(), 1);

        let bypass = create_request("bypass", "worker", AgentStartMode::BootstrapOnly);
        assert!(matches!(
            provider
                .execute(&invocation("cutex.director"), &bypass, &lifecycle)
                .outcome,
            AgentManagementOutcome::NoWrite { ref code, .. } if code == "unresolved_agent_reservation"
        ));
        assert_eq!(lifecycle.bootstrap_count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exact_legacy_native_session_unknown_receipt_upgrades_to_current_authority_retry() {
        let root = root("legacy-pre-sid-retry");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let request = create_request("create", "worker", AgentStartMode::BootstrapOnly);
        seed_legacy_no_sid_receipt(&provider, &request);

        bind(
            &provider,
            "rotate-away",
            "cutex.other-director",
            Some(("cutex.director", 1)),
        );
        assert!(matches!(
            provider
                .execute(&invocation("cutex.director"), &request, &lifecycle)
                .outcome,
            AgentManagementOutcome::NoWrite { ref code, .. } if code == "not_authorized_director"
        ));
        assert_eq!(lifecycle.bootstrap_count(), 0);
        assert!(
            provider.store().snapshot().unwrap().actions[&request.action_id]
                .response
                .is_some()
        );
        bind(
            &provider,
            "rotate-back",
            "cutex.director",
            Some(("cutex.other-director", 2)),
        );

        let receipt =
            completed(provider.execute(&invocation("cutex.director"), &request, &lifecycle));
        assert_eq!(created_agent(&receipt).spec, spec("worker"));
        assert_eq!(
            lifecycle
                .log()
                .iter()
                .filter(|entry| entry.as_str() == "reconcile-pre-sid")
                .count(),
            1,
            "the byte-exact live stored detail must enter reconciliation"
        );
        assert_eq!(lifecycle.bootstrap_count(), 1);
        assert_eq!(provider.store().snapshot().unwrap().agents.len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exact_current_native_session_unknown_receipt_reconciles_and_retries_once() {
        let root = root("current-pre-sid-retry");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let request = create_request("create", "worker", AgentStartMode::CustomMessage);
        seed_owner_action_receipt(&provider, &request, REAL_CURRENT_NO_SID_DETAIL);

        let receipt =
            completed(provider.execute(&invocation("cutex.director"), &request, &lifecycle));

        assert_eq!(created_agent(&receipt).spec, spec("worker"));
        assert_eq!(lifecycle.bootstrap_count(), 1);
        assert_eq!(lifecycle.message_count(), 1);
        assert_eq!(
            lifecycle
                .log()
                .iter()
                .filter(|entry| entry.as_str() == "reconcile-pre-sid")
                .count(),
            1
        );
        assert_eq!(
            provider.store().snapshot().unwrap().actions[&request.action_id].phase,
            AgentActionPhase::Complete
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exact_live_malformed_jsonl_receipt_reconciles_proven_absence_once_after_restart() {
        let root = root("malformed-jsonl-proven-absent-retry");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let request = create_request("create", "worker", AgentStartMode::CustomMessage);
        seed_malformed_jsonl_receipt(&provider, &request);
        drop(provider);

        let lifecycle = Arc::new(FakeLifecycle::default());
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let handles = (0..2)
            .map(|_| {
                let root = root.clone();
                let request = request.clone();
                let lifecycle = Arc::clone(&lifecycle);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let provider = AgentManagementProvider::open(root).unwrap();
                    barrier.wait();
                    provider.execute(&invocation("cutex.director"), &request, lifecycle.as_ref())
                })
            })
            .collect::<Vec<_>>();
        let receipts = handles
            .into_iter()
            .map(|handle| completed(handle.join().unwrap()))
            .collect::<Vec<_>>();

        assert_eq!(receipts[0], receipts[1]);
        assert_eq!(lifecycle.bootstrap_count(), 1);
        assert_eq!(lifecycle.message_count(), 1);
        let reconciliation_count = lifecycle
            .log()
            .iter()
            .filter(|entry| entry.as_str() == "reconcile-pre-sid")
            .count();
        assert!((1..=2).contains(&reconciliation_count));
        let snapshot = AgentManagementProvider::open(&root)
            .unwrap()
            .store()
            .snapshot()
            .unwrap();
        assert_eq!(snapshot.agents.len(), 1);
        assert_eq!(
            snapshot.actions[&request.action_id].phase,
            AgentActionPhase::Complete
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn malformed_jsonl_receipt_present_ambiguous_or_unavailable_never_relaunches() {
        let cases = [
            (
                "present",
                NativeBootstrapReconciliation::Present {
                    reason: "exact managed runtime is present".to_string(),
                },
                "native_bootstrap_reconciliation_present",
            ),
            (
                "ambiguous",
                NativeBootstrapReconciliation::Ambiguous {
                    reason: "native evidence cannot be correlated exactly".to_string(),
                },
                "native_bootstrap_reconciliation_ambiguous",
            ),
            (
                "unavailable",
                NativeBootstrapReconciliation::Unavailable {
                    reason: "selected profile registry is unavailable".to_string(),
                },
                "native_bootstrap_reconciliation_unavailable",
            ),
        ];

        for (label, reconciliation, expected_code) in cases {
            let root = root(&format!("malformed-jsonl-{label}"));
            let provider = AgentManagementProvider::open(&root).unwrap();
            bind(&provider, "bind", "cutex.director", None);
            let lifecycle = FakeLifecycle::with_reconciliation(reconciliation);
            let request = create_request("create", "worker", AgentStartMode::BootstrapOnly);
            let original = seed_malformed_jsonl_receipt(&provider, &request);

            let response = provider.execute(&invocation("cutex.director"), &request, &lifecycle);

            assert!(matches!(
                &response.outcome,
                AgentManagementOutcome::OwnerActionRequired { failure }
                    if failure.code == expected_code
            ));
            assert_eq!(lifecycle.bootstrap_count(), 0);
            let snapshot = provider.store().snapshot().unwrap();
            assert_eq!(
                snapshot.actions[&request.action_id].response.as_ref(),
                Some(&original)
            );
            assert!(snapshot.agents.is_empty());
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn unrelated_owner_action_detail_replays_immutably_without_reconciliation() {
        assert!(legacy_pre_sid_failure_detail_matches(
            REAL_LEGACY_NO_SID_DETAIL
        ));
        assert!(legacy_pre_sid_failure_detail_matches(
            "native_session_unknown: native exec exited exit status: 1 without one captured SID"
        ));
        assert!(legacy_pre_sid_failure_detail_matches(
            "owner_action_required: native_session_unknown: native exec exited exit status: 1 without one captured SID (external outcome unknown)"
        ));
        assert!(legacy_pre_sid_failure_detail_matches(
            "owner_action_required: native_session_unknown: native exec exited exit status: 1 without one captured SID; diagnostic: stderr: provider unavailable (external outcome unknown)"
        ));
        assert!(legacy_pre_sid_failure_detail_matches(
            REAL_MALFORMED_JSONL_DETAIL
        ));
        assert!(legacy_pre_sid_failure_detail_matches(&format!(
            "owner_action_required: {REAL_MALFORMED_JSONL_DETAIL}"
        )));
        assert!(!legacy_pre_sid_failure_detail_matches(
            "native_bootstrap_output_malformed: native exec JSONL stdout line 2 is not valid JSON (external outcome unknown)"
        ));
        assert!(!legacy_pre_sid_failure_detail_matches(
            "owner_action_required: owner_action_required: native_session_unknown: native exec exited exit status: 1 without one captured SID"
        ));
        assert!(!legacy_pre_sid_failure_detail_matches(
            "owner_action_required: native_session_unknown: unrelated failure without one captured SID (external outcome unknown)"
        ));
        assert!(!legacy_pre_sid_failure_detail_matches(
            "owner_action_required: unrelated startup failure"
        ));
        assert!(legacy_ambiguous_sid_failure_detail_matches(
            REAL_LEGACY_AMBIGUOUS_SID_DETAIL
        ));
        assert!(legacy_ambiguous_sid_failure_detail_matches(
            "native_session_ambiguous: native exec exposed multiple possible session identities"
        ));
        assert!(!legacy_ambiguous_sid_failure_detail_matches(
            "owner_action_required: native_session_ambiguous: unrelated ambiguity"
        ));
        assert!(offline_revision_conflict_detail_matches(
            REAL_OFFLINE_REVISION_CONFLICT_DETAIL
        ));
        assert!(!offline_revision_conflict_detail_matches(
            "owner_action_required: session_offline_failed: unrelated persistence failure (external outcome unknown)"
        ));
        assert!(!offline_revision_conflict_detail_matches(
            "owner_action_required: session_online_failed: cutex session store revision conflict: expected 1, current 2; reload before retrying (external outcome unknown)"
        ));
        let root = root("legacy-pre-sid-unrelated-detail");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let request = create_request("create", "worker", AgentStartMode::BootstrapOnly);
        let original = seed_owner_action_receipt(
            &provider,
            &request,
            "owner_action_required: unrelated startup failure",
        );

        let replay = provider.execute(&invocation("cutex.director"), &request, &lifecycle);

        assert_eq!(replay, original);
        assert!(!lifecycle
            .log()
            .iter()
            .any(|entry| entry == "reconcile-pre-sid"));
        assert_eq!(lifecycle.bootstrap_count(), 0);
        assert_eq!(
            provider.store().snapshot().unwrap().actions[&request.action_id]
                .response
                .as_ref(),
            Some(&original)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_no_output_receipt_with_existing_runtime_stays_fenced_without_bootstrap() {
        let root = root("legacy-pre-sid-existing-runtime");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let request = create_request("create", "worker", AgentStartMode::BootstrapOnly);
        let original = seed_legacy_no_sid_receipt(&provider, &request);
        lifecycle.insert_agent(
            &session("cutex.existing-native-runtime"),
            "native-existing",
            &spec("worker"),
        );

        let replay = provider.execute(&invocation("cutex.director"), &request, &lifecycle);

        assert!(matches!(
            &replay.outcome,
            AgentManagementOutcome::OwnerActionRequired { failure }
                if failure.code == "native_bootstrap_reconciliation_present"
                    && failure.detail.contains("exact managed runtime is present")
                    && failure.detail.contains("immutable original receipt remains unchanged")
        ));
        assert_ne!(replay, original);
        assert_eq!(lifecycle.bootstrap_count(), 0);
        assert_eq!(
            lifecycle
                .log()
                .iter()
                .filter(|entry| entry.as_str() == "reconcile-pre-sid")
                .count(),
            1
        );
        let action = &provider.store().snapshot().unwrap().actions[&request.action_id];
        assert_eq!(action.phase, AgentActionPhase::OwnerActionRequired);
        assert!(!action.native_bootstrap_retryable);
        assert_eq!(action.response.as_ref(), Some(&original));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_no_output_receipt_projects_unavailable_reason_without_mutating_receipt() {
        let root = root("legacy-pre-sid-unavailable");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle =
            FakeLifecycle::with_reconciliation(NativeBootstrapReconciliation::Unavailable {
                reason: "native rollout evidence is malformed".to_string(),
            });
        let request = create_request("create", "worker", AgentStartMode::BootstrapOnly);
        let original = seed_legacy_no_sid_receipt(&provider, &request);

        let replay = provider.execute(&invocation("cutex.director"), &request, &lifecycle);

        assert!(matches!(
            &replay.outcome,
            AgentManagementOutcome::OwnerActionRequired { failure }
                if failure.code == "native_bootstrap_reconciliation_unavailable"
                    && failure.detail.contains("native rollout evidence is malformed")
        ));
        assert_eq!(lifecycle.bootstrap_count(), 0);
        let action = &provider.store().snapshot().unwrap().actions[&request.action_id];
        assert_eq!(action.response.as_ref(), Some(&original));
        assert_eq!(action.phase, AgentActionPhase::OwnerActionRequired);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_no_output_receipt_projects_ambiguous_reason_without_bootstrap() {
        let root = root("legacy-pre-sid-ambiguous");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle =
            FakeLifecycle::with_reconciliation(NativeBootstrapReconciliation::Ambiguous {
                reason: "native session lacks a managed-cwd marker".to_string(),
            });
        let request = create_request("create", "worker", AgentStartMode::BootstrapOnly);
        let original = seed_legacy_no_sid_receipt(&provider, &request);

        let replay = provider.execute(&invocation("cutex.director"), &request, &lifecycle);

        assert!(matches!(
            &replay.outcome,
            AgentManagementOutcome::OwnerActionRequired { failure }
                if failure.code == "native_bootstrap_reconciliation_ambiguous"
                    && failure.detail.contains("lacks a managed-cwd marker")
        ));
        assert_eq!(lifecycle.bootstrap_count(), 0);
        assert_eq!(
            provider.store().snapshot().unwrap().actions[&request.action_id]
                .response
                .as_ref(),
            Some(&original)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn interrupted_creation_reconciles_absent_exact_and_ambiguous_without_duplicates() {
        for (label, evidence, expected_bootstraps, complete) in [
            (
                "absent",
                NativeBootstrapIdentityReconciliation::Absent {
                    reason: "creator stopped and sources absent".into(),
                },
                1,
                true,
            ),
            (
                "exact",
                NativeBootstrapIdentityReconciliation::Exact {
                    native_session_id: RECOVERED_NATIVE_SID.into(),
                    reason: "exact saved thread".into(),
                },
                0,
                true,
            ),
            (
                "ambiguous",
                NativeBootstrapIdentityReconciliation::Ambiguous {
                    reason: "multiple candidates".into(),
                },
                0,
                false,
            ),
            (
                "unavailable",
                NativeBootstrapIdentityReconciliation::Unavailable {
                    reason: "creator still stopping".into(),
                },
                0,
                false,
            ),
        ] {
            let root = root(&format!("interrupted-{label}"));
            let provider = AgentManagementProvider::open(&root).unwrap();
            bind(&provider, "bind", "cutex.director", None);
            let request = create_request("create", "worker", AgentStartMode::CustomMessage);
            seed_legacy_ambiguous_sid_receipt(&provider, &request);
            provider.store.with_state(true, |mut state| {
                let action = state.actions.get_mut(&request.action_id).unwrap();
                if let Some(AgentManagementResponse { outcome: AgentManagementOutcome::OwnerActionRequired { failure }, .. }) = &mut action.response {
                    failure.detail = "owner_action_required: native bootstrap outcome is unknown; no second Agent was created".into();
                }
                Ok((state, (), true))
            }).unwrap();
            let lifecycle = FakeLifecycle::with_identity_reconciliation(evidence);
            let response = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
            if complete {
                let receipt = completed(response);
                assert_eq!(
                    completed(provider.execute(
                        &invocation("cutex.director"),
                        &request,
                        &lifecycle
                    )),
                    receipt
                );
                assert_eq!(lifecycle.message_count(), 1);
            } else {
                assert!(matches!(
                    response.outcome,
                    AgentManagementOutcome::OwnerActionRequired { .. }
                ));
                assert_eq!(lifecycle.message_count(), 0);
            }
            assert_eq!(lifecycle.bootstrap_count(), expected_bootstraps);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn exact_ambiguous_attempt_recovery_survives_restart_and_concurrent_replay_without_bootstrap() {
        let root = root("ambiguous-sid-exact-recovery");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let request = create_request("create", "worker", AgentStartMode::CustomMessage);
        seed_legacy_ambiguous_sid_receipt(&provider, &request);
        drop(provider);

        let lifecycle = Arc::new(FakeLifecycle::with_identity_reconciliation(
            NativeBootstrapIdentityReconciliation::Exact {
                native_session_id: RECOVERED_NATIVE_SID.to_string(),
                reason: "one exact selected-profile rollout matches the attempt".to_string(),
            },
        ));
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let handles = (0..2)
            .map(|_| {
                let root = root.clone();
                let request = request.clone();
                let lifecycle = Arc::clone(&lifecycle);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let provider = AgentManagementProvider::open(root).unwrap();
                    barrier.wait();
                    provider.execute(&invocation("cutex.director"), &request, lifecycle.as_ref())
                })
            })
            .collect::<Vec<_>>();
        let responses = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();

        let receipts = responses.into_iter().map(completed).collect::<Vec<_>>();
        assert_eq!(receipts[0], receipts[1]);
        assert_eq!(
            created_agent(&receipts[0]).native_session_id,
            RECOVERED_NATIVE_SID
        );
        assert_eq!(lifecycle.bootstrap_count(), 0);
        assert_eq!(lifecycle.message_count(), 1);
        let snapshot = AgentManagementProvider::open(&root)
            .unwrap()
            .store()
            .snapshot()
            .unwrap();
        assert_eq!(snapshot.agents.len(), 1);
        assert_eq!(
            snapshot.failure_events
                [&format!("agent-management:{}:failure", request.action_id.as_str())]
                .detail,
            REAL_LEGACY_AMBIGUOUS_SID_DETAIL
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ambiguous_attempt_absent_multiple_and_unavailable_evidence_never_relaunch() {
        let cases = [
            (
                "absent",
                NativeBootstrapIdentityReconciliation::Absent {
                    reason: "no selected-profile thread matches".to_string(),
                },
                "native_bootstrap_reconciliation_absent",
            ),
            (
                "multiple",
                NativeBootstrapIdentityReconciliation::Ambiguous {
                    reason: "two exact-cwd native rollouts match".to_string(),
                },
                "native_bootstrap_reconciliation_ambiguous",
            ),
            (
                "unavailable",
                NativeBootstrapIdentityReconciliation::Unavailable {
                    reason: "selected-profile session index is malformed".to_string(),
                },
                "native_bootstrap_reconciliation_unavailable",
            ),
        ];
        for (label, reconciliation, expected_code) in cases {
            let root = root(&format!("ambiguous-sid-{label}"));
            let provider = AgentManagementProvider::open(&root).unwrap();
            bind(&provider, "bind", "cutex.director", None);
            let lifecycle = FakeLifecycle::with_identity_reconciliation(reconciliation);
            let request = create_request("create", "worker", AgentStartMode::BootstrapOnly);
            let original = seed_legacy_ambiguous_sid_receipt(&provider, &request);

            let response = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
            assert!(matches!(
                &response.outcome,
                AgentManagementOutcome::OwnerActionRequired { failure }
                    if failure.code == expected_code
            ));
            assert_eq!(lifecycle.bootstrap_count(), 0);
            let stored = provider.store().snapshot().unwrap();
            assert_eq!(
                stored.actions[&request.action_id].response.as_ref(),
                Some(&original)
            );
            assert!(stored.agents.is_empty());
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn ambiguous_attempt_recovery_requires_current_director_authority() {
        let root = root("ambiguous-sid-authority");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let request = create_request("create", "worker", AgentStartMode::BootstrapOnly);
        seed_legacy_ambiguous_sid_receipt(&provider, &request);
        bind(
            &provider,
            "rotate-authority",
            "cutex.new-director",
            Some(("cutex.director", 1)),
        );
        let lifecycle = FakeLifecycle::with_identity_reconciliation(
            NativeBootstrapIdentityReconciliation::Exact {
                native_session_id: RECOVERED_NATIVE_SID.to_string(),
                reason: "exact evidence".to_string(),
            },
        );

        let response = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        assert!(matches!(
            response.outcome,
            AgentManagementOutcome::NoWrite { ref code, .. } if code == "not_authorized_director"
        ));
        assert_eq!(lifecycle.bootstrap_count(), 0);
        assert!(provider.store().snapshot().unwrap().agents.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn real_known_sid_failure_is_resumable_and_reservations_block_bypass() {
        let root = root("known-sid-resume");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::fail_bootstrap_once_with_known_sid("native-7");
        let request = create_request("captured", "worker", AgentStartMode::BootstrapOnly);

        let failed = provider.execute(&invocation("cutex.director"), &request, &lifecycle);
        assert!(matches!(
            failed.outcome,
            AgentManagementOutcome::OwnerActionRequired { .. }
        ));
        let snapshot = provider.store().snapshot().unwrap();
        let action = &snapshot.actions[&request.action_id];
        assert_eq!(
            action.phase,
            AgentActionPhase::NativeSessionCaptured,
            "known SID failure must retain its exact continuation phase"
        );
        assert_eq!(action.known_native_session_id.as_deref(), Some("native-7"));
        assert!(action.response.is_none(), "known SID failure is resumable");

        let mut changed_same_action = request.clone();
        if let AgentOperation::Create {
            spec: candidate_spec,
            ..
        } = &mut changed_same_action.operation
        {
            candidate_spec.name = "changed-name".to_string();
        }
        assert!(matches!(
            provider
                .execute(
                    &invocation("cutex.director"),
                    &changed_same_action,
                    &lifecycle
                )
                .outcome,
            AgentManagementOutcome::NoWrite { ref code, .. } if code == "action_id_payload_conflict"
        ));

        let mut changed_name = create_request(
            "fresh-same-cwd",
            "changed-name",
            AgentStartMode::BootstrapOnly,
        );
        if let AgentOperation::Create {
            spec: candidate_spec,
            ..
        } = &mut changed_name.operation
        {
            candidate_spec.cwd = spec("worker").cwd;
        }
        // 2026-10-11: only the name is reserved; another Agent may use the
        // same cwd while the unfinished action keeps its name.
        let changed_name_agent = created_agent(&completed(provider.execute(
            &invocation("cutex.director"),
            &changed_name,
            &lifecycle,
        )));
        assert_eq!(changed_name_agent.spec.cwd, spec("worker").cwd);

        let mut changed_cwd =
            create_request("fresh-same-name", "worker", AgentStartMode::BootstrapOnly);
        if let AgentOperation::Create {
            spec: candidate_spec,
            ..
        } = &mut changed_cwd.operation
        {
            candidate_spec.cwd = test_agent_cwd("changed-cwd");
        }
        let changed_cwd_response =
            provider.execute(&invocation("cutex.director"), &changed_cwd, &lifecycle);
        assert!(matches!(
            changed_cwd_response.outcome,
            AgentManagementOutcome::NoWrite { ref code, .. } if code == "unresolved_agent_reservation"
        ));
        assert_eq!(lifecycle.bootstrap_count(), 2);

        let receipt =
            completed(provider.execute(&invocation("cutex.director"), &request, &lifecycle));
        assert_eq!(created_agent(&receipt).native_session_id, "native-7");
        assert_eq!(lifecycle.bootstrap_count(), 2, "the resumed action does not bootstrap again");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn captured_sid_continues_exact_agent_and_unknown_bootstrap_never_creates_second() {
        let root = root("captured-sid");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let captured = create_request("captured", "worker", AgentStartMode::BootstrapOnly);
        let digest = request_sha256(&captured).unwrap();
        let timestamp = now();
        provider
            .store()
            .with_state(true, |mut state| {
                state.actions.insert(
                    captured.action_id.clone(),
                    AgentActionRecord {
                        action_id: captured.action_id.clone(),
                        request_sha256: digest,
                        operation: AgentOperationKind::Create,
                        project_id: project(),
                        caller_cutex_session: session("cutex.director"),
                        created_by: None,
                        phase: AgentActionPhase::NativeSessionCaptured,
                        phase_sequence: 0,
                        reserved_agent_name: Some("worker".to_string()),
                        reserved_agent_cwd: Some(spec("worker").cwd),
                        known_successor_cutex_session: None,
                        known_native_session_id: Some("native-7".to_string()),
                        native_bootstrap_retryable: false,
                        historical_runtime_occurrence_fence: None,
                        external_message_id: None,
                        response: None,
                        created_at: timestamp.clone(),
                        updated_at: timestamp,
                    },
                );
                Ok((state, (), true))
            })
            .unwrap();
        let receipt =
            completed(provider.execute(&invocation("cutex.director"), &captured, &lifecycle));
        assert_eq!(created_agent(&receipt).native_session_id, "native-7");
        assert_eq!(lifecycle.bootstrap_count(), 0);

        let unknown = create_request("unknown", "worker-unknown", AgentStartMode::BootstrapOnly);
        let digest = request_sha256(&unknown).unwrap();
        let timestamp = now();
        provider
            .store()
            .with_state(true, |mut state| {
                state.actions.insert(
                    unknown.action_id.clone(),
                    AgentActionRecord {
                        action_id: unknown.action_id.clone(),
                        request_sha256: digest,
                        operation: AgentOperationKind::Create,
                        project_id: project(),
                        caller_cutex_session: session("cutex.director"),
                        created_by: None,
                        phase: AgentActionPhase::NativeBootstrapPending,
                        phase_sequence: 0,
                        reserved_agent_name: Some("worker-unknown".to_string()),
                        reserved_agent_cwd: Some(spec("worker-unknown").cwd),
                        known_successor_cutex_session: None,
                        known_native_session_id: None,
                        native_bootstrap_retryable: false,
                        historical_runtime_occurrence_fence: None,
                        external_message_id: None,
                        response: None,
                        created_at: timestamp.clone(),
                        updated_at: timestamp,
                    },
                );
                Ok((state, (), true))
            })
            .unwrap();
        let response = provider.execute(&invocation("cutex.director"), &unknown, &lifecycle);
        assert!(matches!(
            response.outcome,
            AgentManagementOutcome::OwnerActionRequired { .. }
        ));
        assert_eq!(
            provider.store().snapshot().unwrap().actions[&unknown.action_id].phase,
            AgentActionPhase::OwnerActionRequired,
            "an outcome-unknown pre-SID action must retain its fence"
        );
        assert_eq!(lifecycle.bootstrap_count(), 0);
        assert_eq!(provider.store().snapshot().unwrap().agents.len(), 1);

        let mut bypass = create_request(
            "unknown-bypass",
            "changed-unknown-name",
            AgentStartMode::BootstrapOnly,
        );
        if let AgentOperation::Create {
            spec: candidate_spec,
            ..
        } = &mut bypass.operation
        {
            candidate_spec.cwd = spec("worker-unknown").cwd;
        }
        // Another name may share the cwd; the fenced action keeps its name.
        completed(provider.execute(&invocation("cutex.director"), &bypass, &lifecycle));
        assert_eq!(lifecycle.bootstrap_count(), 1);
        let mut same_name = create_request(
            "unknown-same-name",
            "worker-unknown",
            AgentStartMode::BootstrapOnly,
        );
        if let AgentOperation::Create {
            spec: candidate_spec,
            ..
        } = &mut same_name.operation
        {
            candidate_spec.cwd = test_agent_cwd("elsewhere");
        }
        assert!(matches!(
            provider
                .execute(&invocation("cutex.director"), &same_name, &lifecycle)
                .outcome,
            AgentManagementOutcome::NoWrite { ref code, .. } if code == "unresolved_agent_reservation"
        ));
        assert_eq!(lifecycle.bootstrap_count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn root_reconciliation_terminalizes_exact_live_legacy_shape_and_releases_reservation() {
        let root = root("reservation-reconcile-live-shape");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let target = rotation_request("legacy-rotation", "example-director-v2");
        seed_missing_ownership_reservation(&provider, &target);
        let request = reservation_reconciliation_request("root-reconcile", &target);
        let before = provider.store().snapshot().unwrap();
        let before_authority = before.projects.clone();
        let before_agents = before.agents.clone();
        let before_failures = before.failure_events.clone();

        let (receipt, replayed) = provider
            .reconcile_agent_reservation(&request, |name, cwd, _, _| {
                assert_eq!(name, "example-director-v2");
                assert_eq!(cwd, spec("example-director-v2").cwd);
                AgentReservationReconciliationEvidence::ProvenAbsent {
                    reason: "every authoritative fake source is absent".to_string(),
                }
            })
            .unwrap();
        assert!(!replayed);
        assert!(matches!(
            receipt.target_response.outcome,
            AgentManagementOutcome::NoWrite { ref code, .. }
                if code == "reservation_reconciled_no_write"
        ));
        let after = provider.store().snapshot().unwrap();
        assert_eq!(after.projects, before_authority);
        assert_eq!(after.agents, before_agents);
        assert_eq!(after.failure_events, before_failures);
        assert_eq!(
            after.actions[&target.action_id].external_message_id,
            before.actions[&target.action_id].external_message_id,
            "reconciliation must not enqueue or replace a message identity"
        );
        assert_eq!(
            after.actions[&target.action_id].phase,
            AgentActionPhase::NoWrite
        );
        assert_eq!(after.reservation_reconciliation_receipts.len(), 1);
        assert_eq!(after.reservation_reconciliation_events.len(), 1);
        assert!(after.phase_events.values().any(|event| {
            event.action_id == target.action_id && event.phase == AgentActionPhase::NoWrite
        }));

        let (replayed_receipt, replayed) = provider
            .reconcile_agent_reservation(&request, |_, _, _, _| {
                panic!("exact admin replay must not re-read external evidence")
            })
            .unwrap();
        assert!(replayed);
        assert_eq!(replayed_receipt, receipt);
        let mut changed = request.clone();
        changed.expected_reserved_agent_name.push_str("-changed");
        assert!(matches!(
            provider.reconcile_agent_reservation(&changed, |_, _, _, _| unreachable!()),
            Err(AgentManagementError::Conflict("action_id_payload_conflict"))
        ));

        // r219: director_rotate was removed, so even an exact replay of the
        // legacy request is refused before the recorded action is read.
        let lifecycle = FakeLifecycle::default();
        let original_replay = provider.execute(&invocation("cutex.director"), &target, &lifecycle);
        assert!(matches!(
            original_replay.outcome,
            AgentManagementOutcome::NoWrite { ref code, .. } if code == "tool_removed"
        ));
        assert!(lifecycle.log().is_empty());

        let fresh = create_request(
            "corrected-fresh-action",
            "example-director-v2",
            AgentStartMode::BootstrapOnly,
        );
        assert!(matches!(
            provider
                .execute(&invocation("cutex.director"), &fresh, &lifecycle)
                .outcome,
            AgentManagementOutcome::Complete { .. }
        ));
        assert_eq!(lifecycle.message_count(), 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reconciliation_evidence_present_ambiguous_or_unavailable_fails_closed() {
        let cases = [
            AgentReservationReconciliationEvidence::Present {
                reason: "matching durable successor".to_string(),
            },
            AgentReservationReconciliationEvidence::Ambiguous {
                reason: "native index is ambiguous".to_string(),
            },
            AgentReservationReconciliationEvidence::Unavailable {
                reason: "Agent Bus registry is unreadable".to_string(),
            },
        ];
        for (index, evidence) in cases.into_iter().enumerate() {
            let root = root(&format!("reservation-evidence-{index}"));
            let provider = AgentManagementProvider::open(&root).unwrap();
            bind(&provider, "bind", "cutex.director", None);
            let target = rotation_request("legacy-rotation", "example-director-v2");
            seed_missing_ownership_reservation(&provider, &target);
            let request = reservation_reconciliation_request("root-reconcile", &target);
            let before = provider.store().snapshot().unwrap();
            let result = provider.reconcile_agent_reservation(&request, |_, _, _, _| evidence);
            assert!(matches!(
                result,
                Err(AgentManagementError::ReconciliationPresent(_))
                    | Err(AgentManagementError::ReconciliationAmbiguous(_))
                    | Err(AgentManagementError::ReconciliationUnavailable(_))
            ));
            assert_eq!(provider.store().snapshot().unwrap(), before);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn reconciliation_cas_failure_class_and_post_effect_evidence_fail_closed() {
        for case in [
            "digest",
            "name",
            "authority",
            "phase",
            "failure",
            "post-effect",
            "retry",
            "native",
            "successor",
            "fence",
            "managed",
            "operation",
        ] {
            let root = root(&format!("reservation-invalid-{case}"));
            let provider = AgentManagementProvider::open(&root).unwrap();
            bind(&provider, "bind", "cutex.director", None);
            let target = rotation_request("legacy-rotation", "example-director-v2");
            seed_missing_ownership_reservation(&provider, &target);
            let mut request = reservation_reconciliation_request("root-reconcile", &target);
            match case {
                "digest" => {
                    request.expected_target_request_sha256 = Sha256::new("0".repeat(64)).unwrap()
                }
                "name" => request.expected_reserved_agent_name.push_str("-wrong"),
                "authority" => request.expected_authority_epoch = 2,
                "phase" => request.expected_phase = AgentActionPhase::Prepared,
                "failure" => {
                    provider
                        .store()
                        .with_state(true, |mut state| {
                            let action = state.actions.get_mut(&target.action_id).unwrap();
                            let AgentManagementOutcome::OwnerActionRequired { failure } =
                                &mut action.response.as_mut().unwrap().outcome
                            else {
                                unreachable!()
                            };
                            failure.detail = "owner_action_required: unrelated failure".to_string();
                            Ok((state, (), true))
                        })
                        .unwrap();
                }
                "post-effect" => {
                    provider
                        .store()
                        .with_state(true, |mut state| {
                            let mut event = state
                                .phase_events
                                .values()
                                .find(|event| event.action_id == target.action_id)
                                .unwrap()
                                .clone();
                            event.event_id =
                                format!("agent-management:{}:phase:99", target.action_id);
                            event.phase = AgentActionPhase::NativeBootstrapPending;
                            event.phase_sequence = 99;
                            state.phase_events.insert(event.event_id.clone(), event);
                            Ok((state, (), true))
                        })
                        .unwrap();
                }
                "retry" => {
                    provider
                        .store()
                        .with_state(true, |mut state| {
                            state
                                .actions
                                .get_mut(&target.action_id)
                                .unwrap()
                                .native_bootstrap_retryable = true;
                            Ok((state, (), true))
                        })
                        .unwrap();
                }
                "native" => {
                    provider
                        .store()
                        .with_state(true, |mut state| {
                            state
                                .actions
                                .get_mut(&target.action_id)
                                .unwrap()
                                .known_native_session_id = Some("native-existing".to_string());
                            Ok((state, (), true))
                        })
                        .unwrap();
                }
                "successor" => {
                    provider
                        .store()
                        .with_state(true, |mut state| {
                            state
                                .actions
                                .get_mut(&target.action_id)
                                .unwrap()
                                .known_successor_cutex_session = Some(session("cutex.existing-r5"));
                            Ok((state, (), true))
                        })
                        .unwrap();
                }
                "fence" => {
                    provider
                        .store()
                        .with_state(true, |mut state| {
                            state
                                .actions
                                .get_mut(&target.action_id)
                                .unwrap()
                                .historical_runtime_occurrence_fence =
                                Some(RuntimeOccurrenceFence {
                                    runtime_generation: 0,
                                    current_runtime_agent_id: None,
                                    agent_bus_endpoint_ids: Vec::new(),
                                    pending_launch_id: None,
                                    app_server_launch_claim_id: None,
                                    alden_session_name: None,
                                    alden_pid: None,
                                    runtime_pid: None,
                                    app_server_pid: None,
                                    app_server_endpoint: None,
                                    app_server_connected: false,
                                });
                            Ok((state, (), true))
                        })
                        .unwrap();
                }
                "managed" => {
                    provider
                        .store()
                        .with_state(true, |mut state| {
                            let cutex_session_id = session("cutex.existing-r5");
                            state.agents.insert(
                                cutex_session_id.clone(),
                                ManagedAgentRecord {
                                    project_id: Some(project()),
                                    created_by_director_session: Some(session("cutex.director")),
                                    created_by_operator_session: None,
                                    cutex_session_id,
                                    native_session_id: "native-existing-r5".to_string(),
                                    spec: spec("example-director-v2"),
                                    created_at: now(),
                                    retired_at: None,
                                },
                            );
                            Ok((state, (), true))
                        })
                        .unwrap();
                }
                "operation" => {
                    provider
                        .store()
                        .with_state(true, |mut state| {
                            state.actions.get_mut(&target.action_id).unwrap().operation =
                                AgentOperationKind::Online;
                            for event in state
                                .phase_events
                                .values_mut()
                                .filter(|event| event.action_id == target.action_id)
                            {
                                event.operation = AgentOperationKind::Online;
                            }
                            let failure = state
                                .failure_events
                                .values_mut()
                                .find(|failure| failure.action_id == target.action_id)
                                .unwrap();
                            failure.operation = AgentOperationKind::Online;
                            let AgentManagementOutcome::OwnerActionRequired { failure } =
                                &mut state
                                    .actions
                                    .get_mut(&target.action_id)
                                    .unwrap()
                                    .response
                                    .as_mut()
                                    .unwrap()
                                    .outcome
                            else {
                                unreachable!()
                            };
                            failure.operation = AgentOperationKind::Online;
                            Ok((state, (), true))
                        })
                        .unwrap();
                }
                _ => unreachable!(),
            }
            let before = provider.store().snapshot().unwrap();
            assert!(provider
                .reconcile_agent_reservation(&request, |_, _, _, _| {
                    AgentReservationReconciliationEvidence::ProvenAbsent {
                        reason: "absent".to_string(),
                    }
                })
                .is_err());
            assert_eq!(provider.store().snapshot().unwrap(), before);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn new_director_rotate_is_refused_without_recording_an_action_or_reservation() {
        let root = root("new-rotation-removed");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let before = provider.store().snapshot().unwrap();
        let rotation = rotation_request("new-rotation", "example-director-v2");
        let response = provider.execute(&invocation("cutex.director"), &rotation, &lifecycle);
        assert!(matches!(
            response.outcome,
            AgentManagementOutcome::NoWrite { ref code, ref detail }
                if code == "tool_removed" && detail.contains("owner assigns the project Director")
        ));
        let after = provider.store().snapshot().unwrap();
        assert!(!after.actions.contains_key(&rotation.action_id));
        assert_eq!(after.store_revision, before.store_revision);
        assert!(lifecycle.log().is_empty());

        let create = create_request(
            "fresh-after-missing-ownership",
            "example-director-v2",
            AgentStartMode::BootstrapOnly,
        );
        assert!(matches!(
            provider
                .execute(&invocation("cutex.director"), &create, &lifecycle)
                .outcome,
            AgentManagementOutcome::Complete { .. }
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A `director_rotate` action an older release left without a terminal
    /// response, with the given phase history.
    fn seed_pending_rotation(
        provider: &AgentManagementProvider,
        request: &AgentManagementRequest,
        phases: &[AgentActionPhase],
        known_successor: Option<CutexSessionId>,
    ) {
        let successor = request.operation.bootstrap_spec().unwrap().clone();
        let digest = request_sha256(request).unwrap();
        let timestamp = now();
        provider
            .store()
            .with_state(true, |mut state| {
                state.actions.insert(
                    request.action_id.clone(),
                    AgentActionRecord {
                        action_id: request.action_id.clone(),
                        request_sha256: digest,
                        operation: AgentOperationKind::DirectorRotate,
                        project_id: project(),
                        caller_cutex_session: session("cutex.director"),
                        created_by: None,
                        phase: *phases.last().unwrap(),
                        phase_sequence: phases.len() as u64,
                        reserved_agent_name: Some(successor.name.clone()),
                        reserved_agent_cwd: Some(successor.cwd.clone()),
                        known_successor_cutex_session: known_successor.clone(),
                        known_native_session_id: None,
                        native_bootstrap_retryable: false,
                        historical_runtime_occurrence_fence: None,
                        external_message_id: Some(format!(
                            "agent-management:{}:start",
                            request.action_id
                        )),
                        response: None,
                        created_at: timestamp.clone(),
                        updated_at: timestamp.clone(),
                    },
                );
                for (index, phase) in phases.iter().enumerate() {
                    let sequence = index as u64 + 1;
                    let event_id =
                        format!("agent-management:{}:phase:{sequence}", request.action_id);
                    state.phase_events.insert(
                        event_id.clone(),
                        AgentManagementPhaseEvent {
                            event_id,
                            action_id: request.action_id.clone(),
                            project_id: project(),
                            operation: AgentOperationKind::DirectorRotate,
                            phase: *phase,
                            phase_sequence: sequence,
                            committed_at: timestamp.clone(),
                            presentation_owner_cutex_session_id: session("cutex.director"),
                            subject_cutex_session_id: None,
                            subject_agent_name: Some(successor.name.clone()),
                            predecessor_cutex_session_id: Some(session("cutex.director")),
                            successor_cutex_session_id: known_successor.clone(),
                            replace_policy: None,
                            rotation_mode: Some(DirectorRotateMode::RetainPredecessorWithMessage),
                            authority_epoch: Some(1),
                        },
                    );
                }
                Ok((state, (), true))
            })
            .unwrap();
    }

    /// r219: a rotation that never left `prepared` is closed as NoWrite
    /// (`director_rotate_removed`) and its reservation is released; a second
    /// pass changes nothing.
    #[test]
    fn startup_finishes_an_effect_free_legacy_rotation_as_no_write() {
        let root = root("removed-rotation-no-write");
        let observed = Arc::new(std::sync::Mutex::new(Vec::<AgentManagementPhaseEvent>::new()));
        let sink = observed.clone();
        let provider = AgentManagementProvider::open(&root)
            .unwrap()
            .with_phase_observer(Arc::new(move |event: &AgentManagementPhaseEvent| {
                sink.lock().unwrap().push(event.clone());
            }));
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let rotation = rotation_request("legacy-prepared-rotation", "director-r2");
        seed_pending_rotation(&provider, &rotation, &[AgentActionPhase::Prepared], None);
        // The pending rotation holds the successor name.
        assert!(matches!(
            provider
                .execute(
                    &invocation("cutex.director"),
                    &create_request("blocked-create", "director-r2", AgentStartMode::BootstrapOnly),
                    &lifecycle,
                )
                .outcome,
            AgentManagementOutcome::NoWrite { .. }
        ));
        let failures_before = provider.store().snapshot().unwrap().failure_events;

        assert_eq!(provider.finish_removed_director_rotations().unwrap(), 1);
        let snapshot = provider.store().snapshot().unwrap();
        let action = &snapshot.actions[&rotation.action_id];
        assert_eq!(action.phase, AgentActionPhase::NoWrite);
        assert_eq!(action.phase_sequence, 2);
        assert!(matches!(
            action.response.as_ref().unwrap().outcome,
            AgentManagementOutcome::NoWrite { ref code, .. } if code == DIRECTOR_ROTATE_REMOVED_CODE
        ));
        assert_eq!(snapshot.failure_events, failures_before);
        let event = &snapshot.phase_events
            [&format!("agent-management:{}:phase:2", rotation.action_id)];
        assert_eq!(event.phase, AgentActionPhase::NoWrite);
        assert_eq!(event.operation, AgentOperationKind::DirectorRotate);
        assert_eq!(observed.lock().unwrap().as_slice(), std::slice::from_ref(event));

        let revision = snapshot.store_revision;
        assert_eq!(provider.finish_removed_director_rotations().unwrap(), 0);
        assert_eq!(provider.store().snapshot().unwrap().store_revision, revision);
        assert_eq!(observed.lock().unwrap().len(), 1);
        assert!(lifecycle.log().is_empty());

        // The name is free again for a fresh action.
        completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("fresh-create", "director-r2", AgentStartMode::BootstrapOnly),
            &lifecycle,
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    /// r219: a rotation that may have had effects ends in `failure` with an
    /// owner-visible `director_rotate_removed` record; nothing else changes
    /// (no session, authority or seat write) and a second pass is a no-op.
    #[test]
    fn startup_fails_a_legacy_rotation_that_may_have_had_effects() {
        let root = root("removed-rotation-failure");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let rotation = rotation_request("legacy-ready-rotation", "director-r2");
        seed_pending_rotation(
            &provider,
            &rotation,
            &[
                AgentActionPhase::Prepared,
                AgentActionPhase::NativeBootstrapPending,
                AgentActionPhase::SuccessorReady,
            ],
            Some(session("cutex.director-r2")),
        );
        let before = provider.store().snapshot().unwrap();

        assert_eq!(provider.finish_removed_director_rotations().unwrap(), 1);
        let after = provider.store().snapshot().unwrap();
        let action = &after.actions[&rotation.action_id];
        assert_eq!(action.phase, AgentActionPhase::Failure);
        assert_eq!(action.phase_sequence, 4);
        let AgentManagementOutcome::OwnerActionRequired { failure } =
            &action.response.as_ref().unwrap().outcome
        else {
            panic!("expected an owner-visible failure")
        };
        assert_eq!(failure.code, DIRECTOR_ROTATE_REMOVED_CODE);
        assert_eq!(failure.operation, AgentOperationKind::DirectorRotate);
        assert_eq!(failure.routing_status, FailureRoutingStatus::Routable);
        assert_eq!(failure.route_to_director_session, Some(session("cutex.director")));
        assert_eq!(after.failure_events.get(&failure.event_id), Some(failure));
        let event = &after.phase_events
            [&format!("agent-management:{}:phase:4", rotation.action_id)];
        assert_eq!(event.phase, AgentActionPhase::Failure);
        assert_eq!(event.successor_cutex_session_id, Some(session("cutex.director-r2")));
        assert_eq!(
            event.rotation_mode,
            Some(DirectorRotateMode::RetainPredecessorWithMessage)
        );
        assert_eq!(after.projects, before.projects);
        assert_eq!(after.agents, before.agents);
        assert_eq!(after.operator_grants, before.operator_grants);

        // Reopen from disk: the finished history still loads and is final.
        let reopened = AgentManagementProvider::open(&root).unwrap();
        assert_eq!(reopened.store().snapshot().unwrap(), after);
        assert_eq!(reopened.finish_removed_director_rotations().unwrap(), 0);
        assert_eq!(reopened.store().snapshot().unwrap(), after);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exact_project_operator_can_query_and_run_each_ordinary_lifecycle_operation() {
        let root = root("operator-positive-lifecycle");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let operator = created_agent(&completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("create-operator", "operator", AgentStartMode::BootstrapOnly),
            &lifecycle,
        )));
        completed(provider.execute(
            &invocation("cutex.director"),
            &operator_request("grant-operator", "grant", &operator.cutex_session_id, 0),
            &lifecycle,
        ));

        let query = AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action("operator-query"),
            project_id: Some(project()),
            operation: AgentOperation::QueryManaged,
        };
        let receipt = completed(provider.execute(
            &invocation(operator.cutex_session_id.as_str()),
            &query,
            &lifecycle,
        ));
        let AgentManagementResult::QueryManaged { operators, .. } = receipt.result else {
            panic!("expected query result")
        };
        assert_eq!(operators.grant_revision, 1);
        assert_eq!(
            operators.operators[0].operator_cutex_session_id,
            operator.cutex_session_id
        );

        let managed = created_agent(&completed(provider.execute(
            &invocation(operator.cutex_session_id.as_str()),
            &create_request("operator-create", "managed", AgentStartMode::BootstrapOnly),
            &lifecycle,
        )));
        assert_eq!(
            managed.created_by_director_session,
            Some(session("cutex.director"))
        );
        assert_eq!(
            managed.created_by_operator_session,
            Some(operator.cutex_session_id.clone())
        );
        let creator=Some(crate::session::model::SessionCreatedBy::Agent {
            cutex_session_id:operator.cutex_session_id.clone(),role:Some(crate::session::model::SessionCreatorRole::Member),
        });
        assert_eq!(provider.store().snapshot().unwrap().actions[&action("operator-create")].created_by,creator);
        assert!(lifecycle.state.lock().unwrap().creator_calls.iter().any(|(stage,saved)|stage=="adopt"&&saved==&creator));
        for (action_id, operation) in [
            (
                "operator-offline",
                AgentOperation::Offline {
                    cutex_session_id: managed.cutex_session_id.clone(),
                },
            ),
            (
                "operator-online",
                AgentOperation::Online {
                    cutex_session_id: managed.cutex_session_id.clone(),
                },
            ),
            (
                "operator-restart",
                AgentOperation::Restart {
                    cutex_session_id: managed.cutex_session_id.clone(),
                },
            ),
        ] {
            completed(provider.execute(
                &invocation(operator.cutex_session_id.as_str()),
                &AgentManagementRequest {
                    schema: AgentManagementSchema::V1,
                    action_id: action(action_id),
                    project_id: Some(project()),
                    operation,
                },
                &lifecycle,
            ));
        }

        let close_target = created_agent(&completed(provider.execute(
            &invocation(operator.cutex_session_id.as_str()),
            &create_request(
                "operator-create-close",
                "close-target",
                AgentStartMode::BootstrapOnly,
            ),
            &lifecycle,
        )));
        completed(provider.execute(
            &invocation(operator.cutex_session_id.as_str()),
            &AgentManagementRequest {
                schema: AgentManagementSchema::V1,
                action_id: action("operator-close"),
                project_id: Some(project()),
                operation: AgentOperation::Close {
                    cutex_session_id: close_target.cutex_session_id,
                },
            },
            &lifecycle,
        ));

        let replace_target = created_agent(&completed(provider.execute(
            &invocation(operator.cutex_session_id.as_str()),
            &create_request(
                "operator-create-replace",
                "replace-target",
                AgentStartMode::BootstrapOnly,
            ),
            &lifecycle,
        )));
        completed(provider.execute(
            &invocation(operator.cutex_session_id.as_str()),
            &AgentManagementRequest {
                schema: AgentManagementSchema::V1,
                action_id: action("operator-replace"),
                project_id: Some(project()),
                operation: AgentOperation::Replace {
                    bootstrap_intent: None,
                    predecessor_cutex_session_id: replace_target.cutex_session_id,
                    policy: AgentReplacePolicy::CloseBeforeCreate,
                    successor: spec("replacement"),
                    start_mode: AgentStartMode::BootstrapOnly,
                    frozen_message: None,
                },
            },
            &lifecycle,
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn operator_authority_denies_impersonation_privileged_targets_and_control_planes() {
        let root = root("operator-denials");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let first = created_agent(&completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("create-op-1", "operator-one", AgentStartMode::BootstrapOnly),
            &lifecycle,
        )));
        let second = created_agent(&completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("create-op-2", "operator-two", AgentStartMode::BootstrapOnly),
            &lifecycle,
        )));
        completed(provider.execute(
            &invocation("cutex.director"),
            &operator_request("grant-op-1", "grant", &first.cutex_session_id, 0),
            &lifecycle,
        ));
        completed(provider.execute(
            &invocation("cutex.director"),
            &operator_request("grant-op-2", "grant", &second.cutex_session_id, 1),
            &lifecycle,
        ));

        for (id, operation) in [
            (
                "operator-target-primary",
                AgentOperation::Close {
                    cutex_session_id: session("cutex.director"),
                },
            ),
            (
                "operator-target-operator",
                AgentOperation::Offline {
                    cutex_session_id: second.cutex_session_id.clone(),
                },
            ),
            (
                "operator-rotate",
                AgentOperation::DirectorRotate {
                    bootstrap_intent: None,
                    expected_predecessor_cutex_session: session("cutex.director"),
                    expected_authority_epoch: 1,
                    mode: DirectorRotateMode::RetainPredecessorBootstrapOnly,
                    successor: spec("forbidden-director"),
                    frozen_message: None,
                },
            ),
            (
                "operator-grant",
                AgentOperation::GrantOperator {
                    operator_cutex_session_id: second.cutex_session_id.clone(),
                    expected_grant_revision: 2,
                },
            ),
            (
                "operator-revoke",
                AgentOperation::RevokeOperator {
                    operator_cutex_session_id: second.cutex_session_id.clone(),
                    expected_grant_revision: 2,
                },
            ),
        ] {
            assert!(matches!(
                provider
                    .execute(
                        &invocation(first.cutex_session_id.as_str()),
                        &AgentManagementRequest {
                            schema: AgentManagementSchema::V1,
                            action_id: action(id),
                            project_id: Some(project()),
                            operation,
                        },
                        &lifecycle,
                    )
                    .outcome,
                AgentManagementOutcome::NoWrite { .. }
            ));
        }

        bind_project_only(
            &provider,
            "bind-other",
            "other-project",
            "cutex.other-director",
            None,
        );
        let cross_project = AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action("operator-cross-project"),
            project_id: Some(ProjectId::new("other-project").unwrap()),
            operation: AgentOperation::QueryManaged,
        };
        assert!(matches!(
            provider
                .execute(
                    &invocation(first.cutex_session_id.as_str()),
                    &cross_project,
                    &lifecycle
                )
                .outcome,
            AgentManagementOutcome::NoWrite { ref code, .. } if code == "project_not_authorized"
        ));
        let impersonator = created_agent(&completed(provider.execute(
            &invocation("cutex.director"),
            &create_request(
                "create-impersonator",
                "operator-lookalike",
                AgentStartMode::BootstrapOnly,
            ),
            &lifecycle,
        )));
        assert!(matches!(
            provider
                .execute(
                    &invocation(impersonator.cutex_session_id.as_str()),
                    &AgentManagementRequest {
                        schema: AgentManagementSchema::V1,
                        action_id: action("lookalike-query"),
                        project_id: Some(project()),
                        operation: AgentOperation::QueryManaged,
                    },
                    &lifecycle,
                )
                .outcome,
            AgentManagementOutcome::NoWrite { .. }
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn revoke_is_cas_audited_and_blocks_fresh_effects_but_replays_completed_receipts() {
        let root = root("operator-revoke-replay");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let operator = created_agent(&completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("create-op", "operator", AgentStartMode::BootstrapOnly),
            &lifecycle,
        )));
        let grant = operator_request("grant", "grant", &operator.cutex_session_id, 0);
        let grant_receipt =
            completed(provider.execute(&invocation("cutex.director"), &grant, &lifecycle));
        assert!(matches!(
            grant_receipt.result,
            AgentManagementResult::OperatorGranted { .. }
        ));
        // An Operator grant never makes the Operator the project Director.
        assert_eq!(
            provider.store().snapshot().unwrap().projects[&project()].authorized_director_session,
            session("cutex.director")
        );
        assert_eq!(
            completed(provider.execute(&invocation("cutex.director"), &grant, &lifecycle)),
            grant_receipt
        );
        let query = AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action("completed-before-revoke"),
            project_id: Some(project()),
            operation: AgentOperation::QueryManaged,
        };
        let completed_query = provider.execute(
            &invocation(operator.cutex_session_id.as_str()),
            &query,
            &lifecycle,
        );
        completed(completed_query.clone());
        let pending = create_request(
            "unfinished-before-revoke",
            "never-started",
            AgentStartMode::BootstrapOnly,
        );
        let pending_digest = request_sha256(&pending).unwrap();
        let pending_timestamp = now();
        provider
            .store()
            .with_state(true, |mut state| {
                state.actions.insert(
                    pending.action_id.clone(),
                    AgentActionRecord {
                        action_id: pending.action_id.clone(),
                        request_sha256: pending_digest,
                        operation: AgentOperationKind::Create,
                        project_id: project(),
                        caller_cutex_session: operator.cutex_session_id.clone(),
                        created_by: None,
                        phase: AgentActionPhase::Prepared,
                        phase_sequence: 0,
                        reserved_agent_name: Some("never-started".to_string()),
                        reserved_agent_cwd: Some(spec("never-started").cwd),
                        known_successor_cutex_session: None,
                        known_native_session_id: None,
                        native_bootstrap_retryable: false,
                        historical_runtime_occurrence_fence: None,
                        external_message_id: None,
                        response: None,
                        created_at: pending_timestamp.clone(),
                        updated_at: pending_timestamp,
                    },
                );
                Ok((state, (), true))
            })
            .unwrap();
        completed(provider.execute(
            &invocation("cutex.director"),
            &operator_request("revoke", "revoke", &operator.cutex_session_id, 1),
            &lifecycle,
        ));
        assert_eq!(
            provider.execute(
                &invocation(operator.cutex_session_id.as_str()),
                &query,
                &lifecycle,
            ),
            completed_query
        );
        let bootstraps_before = lifecycle.bootstrap_count();
        assert!(matches!(
            provider
                .execute(
                    &invocation(operator.cutex_session_id.as_str()),
                    &pending,
                    &lifecycle,
                )
                .outcome,
            AgentManagementOutcome::NoWrite { .. }
        ));
        assert_eq!(lifecycle.bootstrap_count(), bootstraps_before);
        let fresh = AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: action("fresh-after-revoke"),
            project_id: Some(project()),
            operation: AgentOperation::QueryManaged,
        };
        assert!(matches!(
            provider
                .execute(
                    &invocation(operator.cutex_session_id.as_str()),
                    &fresh,
                    &lifecycle,
                )
                .outcome,
            AgentManagementOutcome::NoWrite { .. }
        ));
        let snapshot = provider.store().snapshot().unwrap();
        assert_eq!(snapshot.operator_grant_revisions[&project()], 2);
        assert_eq!(snapshot.operator_audit_events.len(), 2);
        assert!(!snapshot.actions.contains_key(&fresh.action_id));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn grant_cas_rejects_primary_unmanaged_and_retired_grantees() {
        let root = root("operator-grant-cas-denials");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let candidate = created_agent(&completed(provider.execute(
            &invocation("cutex.director"),
            &create_request(
                "create-candidate",
                "candidate",
                AgentStartMode::BootstrapOnly,
            ),
            &lifecycle,
        )));
        for request in [
            operator_request("stale-grant", "grant", &candidate.cutex_session_id, 1),
            operator_request("primary-grant", "grant", &session("cutex.director"), 0),
            operator_request("unmanaged-grant", "grant", &session("cutex.unmanaged"), 0),
        ] {
            assert!(matches!(
                provider.execute(&invocation("cutex.director"), &request, &lifecycle).outcome,
                AgentManagementOutcome::NoWrite { ref code, .. } if matches!(code.as_str(), "operator_grant_revision_conflict" | "operator_must_be_active_managed_agent" | "primary_director_cannot_be_operator")
            ));
        }
        completed(provider.execute(
            &invocation("cutex.director"),
            &operator_request("grant-candidate", "grant", &candidate.cutex_session_id, 0),
            &lifecycle,
        ));
        completed(provider.execute(
            &invocation("cutex.director"),
            &AgentManagementRequest {
                schema: AgentManagementSchema::V1,
                action_id: action("primary-close-operator"),
                project_id: Some(project()),
                operation: AgentOperation::Close {
                    cutex_session_id: candidate.cutex_session_id.clone(),
                },
            },
            &lifecycle,
        ));
        assert!(matches!(
            provider
                .execute(
                    &invocation("cutex.director"),
                    &operator_request(
                        "retired-grant",
                        "grant",
                        &candidate.cutex_session_id,
                        2,
                    ),
                    &lifecycle,
                )
                .outcome,
            AgentManagementOutcome::NoWrite { ref code, .. } if code == "operator_must_be_active_managed_agent"
        ));
        let snapshot = provider.store().snapshot().unwrap();
        // Soft archive preserves grants for restoration, but removes current authority.
        assert_eq!(snapshot.operator_audit_events.len(), 1);
        assert!(snapshot.operator_grants[&project()].contains_key(&candidate.cutex_session_id));
        assert!(project_management_role(&snapshot, &candidate.cutex_session_id, &project()).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_revoke_and_operator_action_have_a_linearizable_outcome() {
        let root = root("operator-concurrent-revoke");
        let provider = AgentManagementProvider::open(&root).unwrap();
        bind(&provider, "bind", "cutex.director", None);
        let lifecycle = FakeLifecycle::default();
        let operator = created_agent(&completed(provider.execute(
            &invocation("cutex.director"),
            &create_request("create-op", "operator", AgentStartMode::BootstrapOnly),
            &lifecycle,
        )));
        completed(provider.execute(
            &invocation("cutex.director"),
            &operator_request("grant", "grant", &operator.cutex_session_id, 0),
            &lifecycle,
        ));
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let revoke_root = root.clone();
        let revoke_barrier = Arc::clone(&barrier);
        let revoke_operator = operator.cutex_session_id.clone();
        let revoke = std::thread::spawn(move || {
            let provider = AgentManagementProvider::open(revoke_root).unwrap();
            revoke_barrier.wait();
            provider.execute(
                &invocation("cutex.director"),
                &operator_request("race-revoke", "revoke", &revoke_operator, 1),
                &FakeLifecycle::default(),
            )
        });
        let action_root = root.clone();
        let action_barrier = Arc::clone(&barrier);
        let action_operator = operator.cutex_session_id.clone();
        let action_thread = std::thread::spawn(move || {
            let provider = AgentManagementProvider::open(action_root).unwrap();
            action_barrier.wait();
            provider.execute(
                &invocation(action_operator.as_str()),
                &AgentManagementRequest {
                    schema: AgentManagementSchema::V1,
                    action_id: action("race-query"),
                    project_id: Some(project()),
                    operation: AgentOperation::QueryManaged,
                },
                &FakeLifecycle::default(),
            )
        });
        barrier.wait();
        assert!(matches!(
            revoke.join().unwrap().outcome,
            AgentManagementOutcome::Complete { .. }
        ));
        let action_response = action_thread.join().unwrap();
        assert!(matches!(
            action_response.outcome,
            AgentManagementOutcome::Complete { .. } | AgentManagementOutcome::NoWrite { .. }
        ));
        let snapshot = provider.store().snapshot().unwrap();
        assert!(!snapshot.operator_grants[&project()].contains_key(&operator.cutex_session_id));
        if matches!(
            action_response.outcome,
            AgentManagementOutcome::NoWrite { .. }
        ) {
            assert!(!snapshot.actions.contains_key(&action("race-query")));
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
