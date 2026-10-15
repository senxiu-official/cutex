use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::Context;

use cutex::agent_bus::model::AgentBusSendResponse;
use cutex::agent_bus::model::AgentRegistrationClass;
use cutex::agent_management::{
    AgentLifecycle, AgentManagementInvocation, AgentManagementMessageMetadata,
    AgentManagementOutcome, AgentManagementPhaseEvent, AgentManagementPhaseObserver,
    AgentManagementProvider, AgentManagementRequest, AgentManagementResponse,
    AgentManagementSchema, AgentReservationReconciliationEvidence,
    AgentReservationReconciliationOutcome, AgentReservationReconciliationRequest,
    AgentReservationReconciliationResponse, AgentRuntimeObservation,
    HistoricalRuntimeOccurrenceReconciliation, LegacyDirectorOwnershipEvidence,
    LegacyDirectorOwnershipImportOutcome, LegacyDirectorOwnershipImportRequest,
    LegacyDirectorOwnershipImportResponse, LifecycleFailure, ManagedAgentSpec,
    NativeBootstrapIdentityReconciliation, NativeBootstrapReconciliation, ProjectAuthorityOutcome,
    ProjectAuthorityRequest, ProjectAuthorityResponse, RuntimeOccurrenceFence,
    RuntimeRecoveryOutcome,
};
use cutex::management::v2::integration_events::{
    append_agent_management_outcome, append_agent_management_phase,
};
use cutex::role_revision::{CutexSessionId, Rfc3339};
use cutex::runtime::alden::cute_alden_sessions;
use cutex::runtime::lifecycle::default_cutex_alden_session_name;
use cutex::session::model::{
    parse_cutex_session_runtime_backend, CutexSessionQuickActionMode, CutexSessionRecord,
    CutexSessionRuntimeBackend,
};
use cutex::session::service::{
    adopt_cutex_session, coding_registration_from_cutex_session_record,
    cutex_session_key_for_user_id_including_retired, cutex_session_launch_cwd,
    persist_cutex_session_store_and_im_record, CutexSessionAdoptOptions, CutexSessionEnsureSeed,
};
use cutex::session::store::load_cutex_session_store;

use super::{management_archive, management_lifecycle};

fn is_claude_spec(spec: &ManagedAgentSpec) -> anyhow::Result<bool> {
    let store = super::account_store::load_store_read_only()?;
    let name = spec.profile.as_deref().context("Profile required")?;
    let account = cutex::profiles::lookup::find_account(&store, name)?.context("Profile not found")?;
    Ok(account.cli_kind == cutex::profiles::model::CliKind::Claude)
}

/// Claude Management creation launches with full access / approval never.
/// Accept every alias cutex accepts for that level (`full-access`,
/// `danger-full-access`, `:danger-full-access`, `danger`), not just one
/// spelling: the ordinary (codex) create path stores `full-access`.
fn claude_create_permissions_supported(spec: &ManagedAgentSpec) -> bool {
    use cutex::agent_management::envelope::{ApprovalStrictness, SandboxLevel};
    SandboxLevel::parse(&spec.permissions) == Some(SandboxLevel::FullAccess)
        && SandboxLevel::parse(&spec.sandbox_mode) == Some(SandboxLevel::FullAccess)
        && ApprovalStrictness::parse(&spec.approval_policy) == Some(ApprovalStrictness::Never)
}

/// Whether a durable record's creation time lies clearly before a bootstrap
/// attempt started (margin for second-granular timestamps). Such a record
/// belongs to another Agent that merely shares the cwd. Unparsable times are
/// never excluded (fail closed: they stay evidence).
fn predates_attempt(created_at: &str, started_at: &Rfc3339) -> bool {
    match (
        chrono::DateTime::parse_from_rfc3339(created_at),
        chrono::DateTime::parse_from_rfc3339(started_at.as_str()),
    ) {
        (Ok(created), Ok(started)) => created < started - chrono::Duration::seconds(2),
        _ => false,
    }
}

/// Agent Bus endpoints in `cwd` that may belong to a bootstrap attempt: an
/// endpoint whose native thread is a durable session saved before the
/// attempt is another Agent sharing the cwd.
fn attempt_bus_agents_in_cwd<'a>(
    agents: impl Iterator<Item = &'a cutex::agent_bus::model::AgentBusAgent>,
    durable: &cutex::session::model::CutexSessionStore,
    cwd: &str,
    started_at: &Rfc3339,
) -> Vec<&'a cutex::agent_bus::model::AgentBusAgent> {
    agents
        .filter(|agent| agent.cwd == cwd)
        .filter(|agent| {
            !agent.session_id.as_deref().is_some_and(|session_id| {
                durable.sessions.values().any(|record| {
                    record.codex_session_id.as_deref() == Some(session_id)
                        && predates_attempt(&record.created_at, started_at)
                })
            })
        })
        .collect()
}

fn reconcile_claude_bootstrap(spec: &ManagedAgentSpec, started: &Rfc3339, failed: &Rfc3339) -> Result<NativeBootstrapIdentityReconciliation, LifecycleFailure> {
    let store = load_cutex_session_store().map_err(unknown("claude_bootstrap_store_unavailable"))?;
    // Agents share cwds: a record saved before this attempt began is another
    // Agent, never this attempt's creation.
    let candidates: Vec<_> = store.sessions.values().filter(|r| !predates_attempt(&r.created_at, started)
        && (r.cwd == spec.cwd || r.formal_agent_name.as_deref() == Some(spec.name.as_str()))).collect();
    if candidates.is_empty() { return Ok(NativeBootstrapIdentityReconciliation::Absent { reason: "No saved Claude adapter record; bootstrap does not spawn before durable capture".into() }); }
    if let [record] = candidates.as_slice() {
        let timestamp = chrono::DateTime::parse_from_rfc3339(&record.created_at).map_err(unknown("claude_bootstrap_timestamp_invalid"))?;
        let start = chrono::DateTime::parse_from_rfc3339(started.as_str()).map_err(unknown("claude_bootstrap_timestamp_invalid"))?;
        let end = chrono::DateTime::parse_from_rfc3339(failed.as_str()).map_err(unknown("claude_bootstrap_timestamp_invalid"))?;
        if record.claude_session.is_some() && record.cwd == spec.cwd && record.profile == spec.profile
            && record.formal_agent_name.as_deref() == Some(spec.name.as_str()) && timestamp >= start && timestamp <= end {
            return Ok(NativeBootstrapIdentityReconciliation::Exact { native_session_id: record.app_server_thread_id().unwrap().into(), reason: "Exact saved Claude adapter identity and creation interval".into() });
        }
    }
    Ok(NativeBootstrapIdentityReconciliation::Ambiguous { reason: "Reserved name/cwd has conflicting durable identity".into() })
}

pub(crate) fn handle_agent_management(
    state: &std::sync::Arc<std::sync::Mutex<cutex::agent_bus::store::AgentBusState>>,
    invocation: AgentManagementInvocation,
    request: AgentManagementRequest,
) -> anyhow::Result<serde_json::Value> {
    Ok(execute_agent_management(state, invocation, request))
}

fn execute_agent_management(
    state: &std::sync::Arc<std::sync::Mutex<cutex::agent_bus::store::AgentBusState>>,
    invocation: AgentManagementInvocation,
    request: AgentManagementRequest,
) -> serde_json::Value {
    let started = std::time::Instant::now();
    let action_id = request.action_id.clone();
    let caller_cutex_session = invocation.caller_cutex_session.clone();
    let response = match AgentManagementProvider::open_default() {
        Ok(provider) => {
            let publisher = std::sync::Arc::new(ManagementPhasePublisher);
            let provider = provider.with_phase_observer(publisher.clone());
            let response = provider.execute(
                &invocation,
                &request,
                &CutexAgentLifecycle {
                    state: std::sync::Arc::clone(state),
                },
            );
            // Exact replay and process restart may not invoke a fresh phase
            // callback. Re-drain the durable per-action journal; the
            // Management v2 append is idempotent by phase-event identity.
            if let Ok(snapshot) = provider.store().snapshot() {
                for phase in snapshot
                    .phase_events
                    .values()
                    .filter(|phase| phase.action_id == request.action_id)
                {
                    publisher.phase_committed(phase);
                }
            }
            response
        }
        Err(error) => AgentManagementResponse {
            schema: AgentManagementSchema::V1,
            action_id,
            outcome: AgentManagementOutcome::NoWrite {
                code: "persistence_unavailable".to_string(),
                detail: format!("Agent Management provider is unavailable: {error:#}"),
            },
        },
    };
    refresh_lifecycle_presence(&response);
    if let Err(error) = append_agent_management_outcome(&caller_cutex_session, &response) {
        eprintln!(
            "warning: failed to project Agent Management outcome into Management v2: {error:#}"
        );
    }
    let mut value = serde_json::to_value(response).expect("Agent Management response is serializable");
    super::agent_start_message::add_first_message(&mut value, started);
    value
}

/// The subject whose runtime a completed lifecycle action changed, with the
/// generation it validated as online (`None` when it left it offline).
fn lifecycle_presence_subject(
    response: &AgentManagementResponse,
) -> Option<(CutexSessionId, Option<u64>)> {
    use cutex::agent_management::AgentManagementResult;
    let AgentManagementOutcome::Complete { receipt } = &response.outcome else {
        return None;
    };
    let (id, observation) = match &receipt.result {
        AgentManagementResult::Created {
            agent, observation, ..
        }
        | AgentManagementResult::Lifecycle { agent, observation } => {
            (agent.cutex_session_id.clone(), observation)
        }
        AgentManagementResult::Replaced {
            successor,
            observation,
            ..
        } => (successor.cutex_session_id.clone(), observation),
        _ => return None,
    };
    let online = observation.app_server_runtime && !observation.runtime_agent_ids.is_empty();
    Some((id, online.then_some(observation.runtime_generation)))
}

/// `agent_list` presence is refreshed periodically by the management process;
/// refresh the subject now so a just-started member is not listed offline.
fn refresh_lifecycle_presence(response: &AgentManagementResponse) {
    if let Some((id, generation)) = lifecycle_presence_subject(response) {
        super::asks_runtime::refresh_session_presence(id.as_str(), generation);
    }
}

struct ManagementPhasePublisher;

impl AgentManagementPhaseObserver for ManagementPhasePublisher {
    fn phase_committed(&self, phase: &AgentManagementPhaseEvent) {
        if let Err(error) = append_agent_management_phase(phase) {
            eprintln!(
                "warning: failed to publish Agent Management phase into Management v2: {error:#}"
            );
        }
    }
}

/// r219: finish `director_rotate` actions an older release left in flight
/// (the operation was removed). Run once when `cutex management serve` or
/// `cutex agent serve` starts; never fatal. Idempotent: a finished action is
/// not touched again.
pub(crate) fn finish_removed_director_rotations_at_startup() {
    let provider = match AgentManagementProvider::open_default() {
        Ok(provider) => provider.with_phase_observer(std::sync::Arc::new(ManagementPhasePublisher)),
        Err(error) => {
            eprintln!(
                "warning: Agent Management is unavailable; in-flight director_rotate actions were not finished: {error:#}"
            );
            return;
        }
    };
    match provider.finish_removed_director_rotations() {
        Ok(0) => {}
        Ok(count) => eprintln!(
            "cutex: finished {count} in-flight director_rotate action(s) as failed (director_rotate was removed in r219)"
        ),
        Err(error) => eprintln!(
            "warning: failed to finish in-flight director_rotate actions: {error:#}"
        ),
    }
}

pub(crate) fn bind_project_authority(request: ProjectAuthorityRequest) -> serde_json::Value {
    let action_id = request.action_id.clone();
    let outcome = match AgentManagementProvider::open_default() {
        Ok(provider) => match provider.bind_project_authority(&request) {
            Ok(receipt) => ProjectAuthorityOutcome::Complete { receipt },
            // r219: the correction is one Agent Management write (no seat copy).
            Err(error) => ProjectAuthorityOutcome::NoWrite {
                code: error.wire().0,
                detail: error.wire().1,
            },
        },
        Err(error) => ProjectAuthorityOutcome::NoWrite {
            code: "persistence_unavailable".to_string(),
            detail: format!("Agent Management provider is unavailable: {error:#}"),
        },
    };
    serde_json::to_value(ProjectAuthorityResponse {
        schema: AgentManagementSchema::V1,
        action_id,
        outcome,
    })
    .expect("project authority response is serializable")
}

pub(crate) fn import_legacy_director_ownership(
    request: LegacyDirectorOwnershipImportRequest,
) -> serde_json::Value {
    let action_id = request.action_id.clone();
    let outcome = match AgentManagementProvider::open_default() {
        Ok(provider) => match provider.import_legacy_director_ownership(&request, || {
            load_legacy_director_ownership_evidence(&request)
        }) {
            Ok((receipt, replayed)) => {
                LegacyDirectorOwnershipImportOutcome::Complete { receipt, replayed }
            }
            Err(error) => LegacyDirectorOwnershipImportOutcome::NoWrite {
                code: error.wire().0,
                detail: error.wire().1,
            },
        },
        Err(error) => LegacyDirectorOwnershipImportOutcome::NoWrite {
            code: "persistence_unavailable".to_string(),
            detail: format!("Agent Management provider is unavailable: {error:#}"),
        },
    };
    serde_json::to_value(LegacyDirectorOwnershipImportResponse {
        schema: request.schema,
        action_id,
        outcome,
    })
    .expect("legacy Director ownership import response is serializable")
}

pub(crate) fn reconcile_agent_reservation(
    request: AgentReservationReconciliationRequest,
) -> serde_json::Value {
    let action_id = request.action_id.clone();
    let outcome = match AgentManagementProvider::open_default() {
        Ok(provider) => match provider.reconcile_agent_reservation(
            &request,
            |reserved_name, reserved_cwd, started_at, failed_at| {
                load_agent_reservation_absence_evidence(
                    reserved_name,
                    reserved_cwd,
                    started_at,
                    failed_at,
                )
            },
        ) {
            Ok((receipt, replayed)) => {
                AgentReservationReconciliationOutcome::Complete { receipt, replayed }
            }
            Err(error) => AgentReservationReconciliationOutcome::NoWrite {
                code: error.wire().0,
                detail: error.wire().1,
            },
        },
        Err(error) => AgentReservationReconciliationOutcome::NoWrite {
            code: "persistence_unavailable".to_string(),
            detail: format!("Agent Management provider is unavailable: {error:#}"),
        },
    };
    serde_json::to_value(AgentReservationReconciliationResponse {
        schema: request.schema,
        action_id,
        outcome,
    })
    .expect("Agent reservation reconciliation response is serializable")
}

fn load_agent_reservation_absence_evidence(
    reserved_name: &str,
    reserved_cwd: &str,
    started_at: &Rfc3339,
    failed_at: &Rfc3339,
) -> AgentReservationReconciliationEvidence {
    match try_load_agent_reservation_absence_evidence(
        reserved_name,
        reserved_cwd,
        started_at,
        failed_at,
    ) {
        Ok(evidence) => evidence,
        Err(error) => AgentReservationReconciliationEvidence::Unavailable {
            reason: format!("authoritative reservation evidence is unavailable: {error:#}"),
        },
    }
}

fn try_load_agent_reservation_absence_evidence(
    reserved_name: &str,
    reserved_cwd: &str,
    started_at: &Rfc3339,
    failed_at: &Rfc3339,
) -> anyhow::Result<AgentReservationReconciliationEvidence> {
    // Agents share cwds (and a closed Agent's name may be reused): endpoints
    // and records of Agents saved before the reservation began are not
    // evidence for it.
    let durable = load_cutex_session_store().context("failed to load durable session store")?;
    let predates = |session_id: Option<&str>| {
        session_id.is_some_and(|session_id| {
            durable.sessions.values().any(|record| {
                record.codex_session_id.as_deref() == Some(session_id)
                    && predates_attempt(&record.created_at, started_at)
            })
        })
    };
    let bus = cutex::agent_bus::store::load_agent_bus_state_from_registry()
        .context("failed to load current and stale Agent Bus endpoints")?;
    if bus.agents.values().any(|agent| {
        (agent.cwd == reserved_cwd
            || agent.name == reserved_name
            || agent.base_name.as_deref() == Some(reserved_name)
            || agent.thread_name.as_deref() == Some(reserved_name))
            && !predates(agent.session_id.as_deref())
    }) {
        return Ok(AgentReservationReconciliationEvidence::Present {
            reason: "a current or stale Agent Bus endpoint matches the reserved name or cwd"
                .to_string(),
        });
    }

    if durable.sessions.values().any(|record| {
        (record.cwd == reserved_cwd
            || record.managed_cwd.as_deref() == Some(reserved_cwd)
            || record.thread_name.as_deref() == Some(reserved_name)
            || record.display_name_hint.as_deref() == Some(reserved_name))
            && !predates_attempt(&record.created_at, started_at)
    }) {
        return Ok(AgentReservationReconciliationEvidence::Present {
            reason: "a durable native/managed session matches the reserved name or cwd".to_string(),
        });
    }

    let accounts = super::account_store::load_store_read_only()
        .context("failed to load the complete launch profile catalog")?;
    let alden_sessions = cute_alden_sessions().context("failed to query cute-alden directly")?;
    let expected_alden_names = accounts
        .accounts
        .iter()
        .map(|account| {
            cutex::runtime::managed_launch::default_managed_session_name_for_cwd(
                account,
                Path::new(reserved_cwd),
            )
        })
        .collect::<BTreeSet<_>>();
    for session in &alden_sessions {
        if expected_alden_names.contains(session.name.as_deref().unwrap_or_default()) {
            return Ok(
                if cutex::platform::process::process_is_running(session.pid) {
                    AgentReservationReconciliationEvidence::Present {
                        reason:
                            "a live cute-alden occurrence has a deterministic reserved-cwd name"
                                .to_string(),
                    }
                } else {
                    AgentReservationReconciliationEvidence::Ambiguous {
                        reason:
                            "a stale cute-alden occurrence has a deterministic reserved-cwd name"
                                .to_string(),
                    }
                },
            );
        }
        #[cfg(target_os = "linux")]
        {
            let process_cwd = std::fs::read_link(format!("/proc/{}/cwd", session.pid));
            match process_cwd {
                Ok(process_cwd) if process_cwd == Path::new(reserved_cwd) => {
                    return Ok(AgentReservationReconciliationEvidence::Present {
                        reason: "a direct cute-alden process occurrence uses the reserved cwd"
                            .to_string(),
                    });
                }
                Ok(_) => {}
                Err(error) if cutex::platform::process::process_is_running(session.pid) => {
                    return Ok(AgentReservationReconciliationEvidence::Unavailable {
                        reason: format!(
                            "cannot inspect live cute-alden process {} cwd: {error}",
                            session.pid
                        ),
                    });
                }
                Err(_) => {}
            }
        }
    }

    let mut codex_homes = BTreeSet::new();
    codex_homes.insert(
        cutex::config::paths::host_codex_home_dir()
            .context("failed to resolve host native index")?,
    );
    codex_homes.insert(
        cutex::config::paths::legacy_host_codex_home_dir()
            .context("failed to resolve legacy host native index")?,
    );
    codex_homes.insert(
        cutex::config::paths::docker_runtime_home_dir()
            .context("failed to resolve Docker native index")?
            .join(".codex"),
    );
    codex_homes.insert(
        cutex::config::paths::legacy_docker_runtime_home_dir()
            .context("failed to resolve legacy Docker native index")?
            .join(".codex"),
    );
    for codex_home in codex_homes {
        match cutex::runtime::codex_home::correlate_codex_session_between_in_home(
            &codex_home,
            started_at,
            failed_at,
            Path::new(reserved_cwd),
        )? {
            cutex::runtime::codex_home::NativeSessionCorrelation::ProvenAbsent => {}
            cutex::runtime::codex_home::NativeSessionCorrelation::Present { session_id } => {
                return Ok(AgentReservationReconciliationEvidence::Present {
                    reason: format!(
                        "native index/rollout evidence matches the reserved cwd ({session_id})"
                    ),
                });
            }
            cutex::runtime::codex_home::NativeSessionCorrelation::Ambiguous { reason } => {
                return Ok(AgentReservationReconciliationEvidence::Ambiguous { reason });
            }
        }
    }
    Ok(AgentReservationReconciliationEvidence::ProvenAbsent {
        reason: "Agent Management ownership, durable sessions, current/stale Agent Bus endpoints, direct cute-alden occurrences, and every configured native index/rollout source prove the reserved successor absent".to_string(),
    })
}

fn load_legacy_director_ownership_evidence(
    request: &LegacyDirectorOwnershipImportRequest,
) -> Result<LegacyDirectorOwnershipEvidence, cutex::agent_management::AgentManagementError> {
    let store = load_cutex_session_store()
        .map_err(|_| cutex::agent_management::AgentManagementError::PersistenceUnavailable)?;
    let record = store
        .sessions
        .get(request.director_cutex_session_id.as_str())
        .ok_or(cutex::agent_management::AgentManagementError::NotFound(
            "durable_director_session_not_found",
        ))?;
    legacy_director_ownership_evidence_from_record(&request.director_cutex_session_id, record)
}

fn legacy_director_ownership_evidence_from_record(
    director_cutex_session_id: &CutexSessionId,
    record: &CutexSessionRecord,
) -> Result<LegacyDirectorOwnershipEvidence, cutex::agent_management::AgentManagementError> {
    use cutex::agent_bus::model::AgentRegistrationClass;
    use cutex::agent_management::AgentManagementError;
    use cutex::session::model::CutexSessionQuickActionMode;

    if record.cutex_session_id != director_cutex_session_id.as_str() {
        return Err(AgentManagementError::Conflict(
            "durable_session_identity_mismatch",
        ));
    }
    if !record.is_active() || record.retired_at.is_some() {
        return Err(AgentManagementError::Conflict(
            "durable_director_session_retired",
        ));
    }
    if !cutex::runtime::lifecycle::cutex_session_host_is_local(
        &record.host_id,
        &cutex::platform::host::current_host_name(),
    ) {
        return Err(AgentManagementError::Conflict(
            "durable_director_session_not_local",
        ));
    }
    if !record.agent_enabled || record.registration_class != AgentRegistrationClass::Persistent {
        return Err(AgentManagementError::Conflict(
            "durable_director_session_not_managed",
        ));
    }
    if record.app_server_launch_claim_id.is_some() {
        return Err(AgentManagementError::OwnerActionRequired(
            "durable Director has an unresolved app-server launch claim".to_string(),
        ));
    }
    let native_session_id = required_record_field(
        record.codex_session_id.as_deref(),
        "durable_director_native_session_missing",
    )?;
    cutex::session::identity::normalize_codex_session_id(native_session_id)
        .map_err(|_| AgentManagementError::Conflict("invalid_durable_native_session"))?;
    let name = required_record_field(
        record.thread_name.as_deref(),
        "durable_director_name_missing",
    )?;
    if record.display_name_hint.as_deref() != Some(name) {
        return Err(AgentManagementError::Conflict(
            "durable_director_name_ambiguous",
        ));
    }
    let cwd = required_record_field(
        record.managed_cwd.as_deref(),
        "durable_director_managed_cwd_missing",
    )?;
    if cutex_session_launch_cwd(record) != cwd {
        return Err(AgentManagementError::Conflict(
            "durable_director_cwd_ambiguous",
        ));
    }
    let groups = cutex::agent_bus::identity::normalize_agent_groups(record.agent_groups.clone());
    if groups.is_empty() || groups != record.agent_groups {
        return Err(AgentManagementError::Conflict(
            "durable_director_groups_not_canonical",
        ));
    }
    let pin = match record.quick_action {
        CutexSessionQuickActionMode::Auto => false,
        CutexSessionQuickActionMode::Pinned => true,
        CutexSessionQuickActionMode::Hidden => {
            return Err(AgentManagementError::Conflict(
                "durable_director_quick_action_not_representable",
            ))
        }
    };
    let runtime_backend = serde_json::to_value(record.runtime_backend)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .ok_or(AgentManagementError::Conflict(
            "durable_director_runtime_backend_invalid",
        ))?;
    let spec = ManagedAgentSpec {
        name: name.to_string(),
        cwd: cwd.to_string(),
        profile: Some(
            required_record_field(
                record.profile.as_deref(),
                "durable_director_profile_missing",
            )?
            .to_string(),
        ),
        runtime_backend,
        model: required_record_field(
            record.model_defaults.as_deref(),
            "durable_director_model_missing",
        )?
        .to_string(),
        reasoning: required_record_field(
            record.reasoning_defaults.as_deref(),
            "durable_director_reasoning_missing",
        )?
        .to_string(),
        permissions: required_record_field(
            record.permission_defaults.as_deref(),
            "durable_director_permissions_missing",
        )?
        .to_string(),
        approval_policy: required_record_field(
            record.approval_policy.as_deref(),
            "durable_director_approval_policy_missing",
        )?
        .to_string(),
        sandbox_mode: required_record_field(
            record.sandbox_mode.as_deref(),
            "durable_director_sandbox_mode_missing",
        )?
        .to_string(),
        groups,
        expose_to_im: record.exposed_to_backend,
        pin,
    };
    spec.validate()?;
    if record.revision == 0
        || record.revision > cutex::role_revision::MAX_JSON_SAFE_INTEGER
        || record.runtime_generation > cutex::role_revision::MAX_JSON_SAFE_INTEGER
    {
        return Err(AgentManagementError::Conflict(
            "invalid_durable_lifecycle_evidence",
        ));
    }
    Ok(LegacyDirectorOwnershipEvidence {
        director_cutex_session_id: director_cutex_session_id.clone(),
        native_session_id: native_session_id.to_string(),
        durable_session_revision: record.revision,
        runtime_generation: record.runtime_generation,
        spec,
    })
}

fn required_record_field<'a>(
    value: Option<&'a str>,
    reason: &'static str,
) -> Result<&'a str, cutex::agent_management::AgentManagementError> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(cutex::agent_management::AgentManagementError::Conflict(
            reason,
        ))
}

struct CutexAgentLifecycle {
    state: std::sync::Arc<std::sync::Mutex<cutex::agent_bus::store::AgentBusState>>,
}

impl CutexAgentLifecycle {
    fn historical_runtime_occurrence(
        &self,
        cutex_session_id: &CutexSessionId,
    ) -> Result<HistoricalRuntimeOccurrenceReconciliation, LifecycleFailure> {
        let record = load_record(cutex_session_id)?;
        self.historical_runtime_occurrence_for_record(&record)
    }

    fn historical_runtime_occurrence_for_record(
        &self,
        record: &CutexSessionRecord,
    ) -> Result<HistoricalRuntimeOccurrenceReconciliation, LifecycleFailure> {
        let bus_ids = if let Some(entry) = coding_registration_from_cutex_session_record(record) {
            let config = cutex::config::store::load_codez_config();
            management_lifecycle::try_live_agents_for_management_entry(&config, &entry)
                .map_err(unknown("runtime_occurrence_bus_unavailable"))?
                .into_iter()
                .map(|agent| agent.id)
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let manager_status = super::app_server_runtime::runtime_manager()
            .status(&record.cutex_session_id)
            .map_err(unknown("runtime_occurrence_app_server_unavailable"))?;

        let alden_name = record.alden_session_name.clone().or_else(|| {
            (record.runtime_backend == CutexSessionRuntimeBackend::CuteAlden)
                .then(|| default_cutex_alden_session_name(record))
        });
        let alden_session = if let Some(name) = alden_name.as_deref() {
            cute_alden_sessions()
                .map_err(unknown("runtime_occurrence_process_unavailable"))?
                .into_iter()
                .find(|session| session.name.as_deref() == Some(name))
        } else {
            None
        };

        let app_binding = record.app_server_runtime.as_ref();
        let fence = RuntimeOccurrenceFence {
            runtime_generation: record.runtime_generation,
            current_runtime_agent_id: record.current_runtime_agent_id.clone(),
            agent_bus_endpoint_ids: bus_ids,
            pending_launch_id: record.pending_launch_id.clone(),
            app_server_launch_claim_id: record.app_server_launch_claim_id.clone(),
            alden_session_name: alden_name,
            alden_pid: record
                .alden_pid
                .or_else(|| alden_session.as_ref().map(|session| session.pid)),
            runtime_pid: record.runtime_pid,
            app_server_pid: app_binding.map(|binding| binding.pid),
            app_server_endpoint: app_binding.map(|binding| binding.endpoint.clone()),
            app_server_connected: manager_status
                .as_ref()
                .is_some_and(|status| status.connected),
        };
        if fence.is_proven_absent() && manager_status.is_none() && alden_session.is_none() {
            return Ok(
                HistoricalRuntimeOccurrenceReconciliation::ProvenAbsent {
                    fence,
                    reason: "durable session, Agent Bus, app-server manager, process registry, and launch claims prove the runtime occurrence absent".to_string(),
                },
            );
        }
        if manager_status
            .as_ref()
            .is_some_and(|status| !status.connected)
            && fence.current_runtime_agent_id.is_none()
            && fence.agent_bus_endpoint_ids.is_empty()
            && fence.pending_launch_id.is_none()
            && fence.app_server_launch_claim_id.is_none()
            && fence.alden_pid.is_none()
            && fence.runtime_pid.is_none()
            && fence.app_server_pid.is_none()
        {
            return Ok(HistoricalRuntimeOccurrenceReconciliation::Ambiguous {
                reason: "app-server manager retains a disconnected occurrence without a durable ownership claim".to_string(),
            });
        }
        Ok(HistoricalRuntimeOccurrenceReconciliation::Present {
            reason: format!(
                "runtime occurrence generation {} retains an endpoint, process, app-server binding, or launch claim",
                fence.runtime_generation
            ),
        })
    }
}

impl AgentLifecycle for CutexAgentLifecycle {
    fn capture_offline_occurrence(
        &self,
        id: &CutexSessionId,
        legacy_timeout: Option<(&Rfc3339, &[u32])>,
    ) -> Result<Option<RuntimeOccurrenceFence>, LifecycleFailure> {
        let record = load_record(id)?;
        let fence = match legacy_timeout {
            Some((started, pids)) => {
                cutex::session::offline_reconciliation::legacy_timeout_occurrence(
                    &record,
                    started.as_str(),
                    pids,
                )
                .map_err(unknown("scope_timeout_occurrence_unknown"))?
            }
            None => cutex::session::offline_reconciliation::durable_offline_occurrence(&record),
        };
        Ok(Some(fence))
    }

    fn reconcile_offline_scope_timeout(
        &self,
        id: &CutexSessionId,
        expected: &RuntimeOccurrenceFence,
    ) -> Result<RuntimeOccurrenceFence, LifecycleFailure> {
        let path = cutex::session::store::cutex_sessions_path()
            .map_err(unknown("scope_timeout_store_unavailable"))?;
        cutex::session::offline_reconciliation::reconcile_offline_occurrence(
            &path,
            id.as_str(),
            &cutex::platform::host::current_host_name(),
            expected,
            |current| {
                anyhow::ensure!(
                    cutex::runtime::process_scope::managed_agent_scope_is_absent(id.as_str())?,
                    "original Agent scope is still populated"
                );
                cutex::session::offline_reconciliation::prove_processes_and_endpoint_absent(
                    expected,
                )?;
                let entry = coding_registration_from_cutex_session_record(current)
                    .context("original Agent native identity unavailable")?;
                let config = cutex::config::store::load_codez_config();
                anyhow::ensure!(
                    management_lifecycle::try_live_agents_for_management_entry(&config, &entry)?
                        .is_empty(),
                    "Agent Bus still contains the runtime"
                );
                anyhow::ensure!(
                    super::app_server_runtime::runtime_manager()
                        .status(id.as_str())?
                        .is_none(),
                    "app-server manager still owns a runtime"
                );
                if let Some(name) = expected.alden_session_name.as_deref() {
                    anyhow::ensure!(
                        !cute_alden_sessions()?
                            .iter()
                            .any(|session| session.name.as_deref() == Some(name)),
                        "cute-alden runtime still exists"
                    );
                }
                Ok(())
            },
        )
        .map_err(unknown("scope_timeout_reconciliation_failed"))
    }

    fn bootstrap_reviewed(
        &self,
        permit: &cutex::agent_management::BootstrapExecutionPermit<'_>,
    ) -> Result<String, LifecycleFailure> {
        super::stock_lifecycle::bootstrap_native(permit, None)
    }
    fn confirm_reviewed_bootstrap(
        &self,
        permit: &cutex::agent_management::BootstrapExecutionPermit<'_>,
        native: &str,
    ) -> Result<(), LifecycleFailure> {
        super::stock_lifecycle::bootstrap_native(permit, Some(native)).map(|_| ())
    }
    fn adopt_reviewed(
        &self,
        permit: &cutex::agent_management::BootstrapExecutionPermit<'_>,
        native: &str,
    ) -> Result<CutexSessionId, LifecycleFailure> {
        let path = cutex::session::store::cutex_sessions_path()
            .map_err(definite("session_store_unavailable"))?;
        permit
            .adopt(&path, native)
            .map_err(unknown("bootstrap_adoption_uncertain"))
    }
    fn online_reviewed(
        &self,
        permit: &cutex::agent_management::BootstrapExecutionPermit<'_>,
        id: &CutexSessionId,
    ) -> Result<(), LifecycleFailure> {
        let operation = || -> anyhow::Result<()> {
            let path = cutex::session::store::cutex_sessions_path()?;
            let mut runtime = super::stock_lifecycle::StockExecutor::default();
            let receipt = permit.online(&path, id, &mut runtime)?;
            anyhow::ensure!(
                receipt.stage == cutex::agent_management::StockRuntimeStage::Ready,
                "bootstrap runtime not ready"
            );
            Ok(())
        };
        operation().map_err(unknown("bootstrap_runtime_uncertain"))
    }
    fn prepare_private_cwd(&self, spec: &ManagedAgentSpec) -> Result<(), LifecycleFailure> {
        prepare_private_managed_cwd(Path::new(&spec.cwd)).map_err(definite("private_cwd_failed"))
    }

    fn effective_create_spec(&self, spec: &ManagedAgentSpec) -> Result<ManagedAgentSpec, LifecycleFailure> {
        let mut effective = spec.clone();
        if is_claude_spec(spec).map_err(definite("profile_preflight_failed"))? {
            effective.runtime_backend = "claude_subscription".into();
            if effective.reasoning.trim().is_empty() { effective.reasoning = "default".into(); }
            if !claude_create_permissions_supported(&effective) {
                return Err(LifecycleFailure::definite(
                    "unsupported_claude_launch_permissions",
                    format!(
                        "Claude agents are created with full access and approval never, got permissions={:?}, sandbox_mode={:?}, approval_policy={:?}; retry with permissions=\"danger-full-access\" (or \"full-access\"), sandbox_mode=\"danger-full-access\", approval_policy=\"never\", then narrow permissions in the session permissions UI after creation",
                        effective.permissions, effective.sandbox_mode, effective.approval_policy
                    ),
                ));
            }
            // `full-access` / `danger` are accepted aliases; store the
            // canonical value the Claude runtime is launched with.
            effective.permissions = "danger-full-access".into();
            effective.sandbox_mode = "danger-full-access".into();
            effective.approval_policy = "never".into();
            super::agent_start_message::check_reasoning(&effective, None, true)?;
            cutex::profiles::appearance::ClaudeEffort::parse(&effective.reasoning)
                .map_err(definite("invalid_claude_effort"))?;
            return Ok(effective);
        }
        // The pure configuration checks `bootstrap_native` repeats, run here
        // before the reservation's cwd step so an invalid permission alias,
        // sandbox, approval policy, backend or profile is a NoWrite with the
        // name free again, not an owner_action_required reservation.
        parse_cutex_session_runtime_backend(&effective.runtime_backend)
            .map_err(definite("invalid_runtime_backend"))?;
        if cutex::launch::local_deployment::LocalDeployment::selected()
            .map_err(definite("native_bootstrap_preflight_failed"))?
            .is_some()
        {
            super::agent_start_message::resolve_local_reasoning(&mut effective)?;
            cutex::launch::stock::local_bootstrap_configuration(&effective)
                .map_err(definite("native_bootstrap_preflight_failed"))?;
        } else {
            super::agent_start_message::check_reasoning(&effective, None, false)?;
            if cutex::profiles::reasoning::is_default(&effective.reasoning) {
                effective.reasoning.clear();
            }
            super::launch::resolve_launch_profile_override(
                effective.profile.as_deref().ok_or_else(|| {
                    LifecycleFailure::definite("invalid_profile", "Create requires an explicit profile")
                })?,
            )
            .map_err(definite("native_bootstrap_preflight_failed"))?;
        }
        Ok(effective)
    }

    fn bootstrap_native(&self, spec: &ManagedAgentSpec) -> Result<String, LifecycleFailure> {
        if is_claude_spec(spec).map_err(definite("profile_preflight_failed"))? {
            let effort = cutex::profiles::appearance::ClaudeEffort::parse(&spec.reasoning)
                .map_err(definite("invalid_claude_effort"))?;
            // Write / exec follow the profile's rules (default on), as for
            // owner-created Claude agents and quick chats.
            let record = cutex::runtime::claude::create_profile(&spec.name, Path::new(&spec.cwd),
                &spec.model, None, None, spec.profile.clone(), true, effort)
                .map_err(unknown("claude_bootstrap_failed"))?;
            return Ok(record.app_server_thread_id().unwrap().to_string());
        }
        if cutex::launch::local_deployment::LocalDeployment::selected()
            .map_err(definite("native_bootstrap_preflight_failed"))?
            .is_some()
        {
            let configuration = cutex::launch::stock::local_bootstrap_configuration(spec)
                .map_err(definite("native_bootstrap_preflight_failed"))?;
            let mut known_native_session_id = None;
            return super::light_new::create_native(
                Path::new(&spec.cwd),
                &configuration,
                |native| known_native_session_id = Some(native.to_string()),
            )
            .map_err(|error| LifecycleFailure {
                code: "native_bootstrap_failed".into(),
                detail: format!("{error:#}"),
                outcome_unknown: true,
                known_native_session_id,
            });
        }
        // Validate the selected profile and its materialized credential/config
        // files in this process. A failure here is provably before Command::output
        // can spawn the native runtime and is therefore safe for exact retry.
        super::launch::resolve_launch_profile_override(spec.profile.as_deref().ok_or_else(
            || LifecycleFailure::definite("invalid_profile", "Create requires an explicit profile"),
        )?)
        .map_err(definite("native_bootstrap_preflight_failed"))?;
        let plan = native_bootstrap_plan(spec).map_err(definite("bootstrap_plan_failed"))?;
        let output = plan
            .command()
            .output()
            .map_err(unknown("native_bootstrap_spawn_failed"))?;
        captured_native_session_id(&output)
    }

    fn reconcile_pre_sid_bootstrap(
        &self,
        spec: &ManagedAgentSpec,
        started_at: &Rfc3339,
        failed_at: &Rfc3339,
    ) -> Result<NativeBootstrapReconciliation, LifecycleFailure> {
        if is_claude_spec(spec).map_err(unknown("profile_preflight_failed"))? {
            return Ok(match reconcile_claude_bootstrap(spec, started_at, failed_at)? {
                NativeBootstrapIdentityReconciliation::Exact { reason, .. } => NativeBootstrapReconciliation::Present { reason },
                NativeBootstrapIdentityReconciliation::Absent { reason } => NativeBootstrapReconciliation::ProvenAbsent { reason },
                NativeBootstrapIdentityReconciliation::Ambiguous { reason } => NativeBootstrapReconciliation::Ambiguous { reason },
                NativeBootstrapIdentityReconciliation::Unavailable { reason } => NativeBootstrapReconciliation::Unavailable { reason },
            });
        }
        let resolved = super::launch::resolve_launch_profile_override(
            spec.profile.as_deref().ok_or_else(|| {
                LifecycleFailure::definite("invalid_profile", "Create requires an explicit profile")
            })?,
        )
        .map_err(unknown("native_bootstrap_reconciliation_unavailable"))?;
        let expected_agent_name = cutex::agent_bus::identity::account_agent_name(&resolved.account);
        let expected_alden_name =
            cutex::runtime::managed_launch::default_managed_session_name_for_cwd(
                &resolved.account,
                Path::new(&spec.cwd),
            );
        // The cwd is a correlation marker only. Exact correlation additionally
        // requires the managed name/groups; the selected profile determines
        // where native rollout evidence is read, but is not durable identity.
        // Agents share cwds: endpoints and records of Agents saved before
        // this attempt started are not evidence for it.
        let durable = load_cutex_session_store()
            .map_err(unknown("native_bootstrap_reconciliation_unavailable"))?;
        let state = self.state.lock().map_err(|_| {
            LifecycleFailure::outcome_unknown(
                "native_bootstrap_reconciliation_unavailable",
                "Agent Bus state lock is poisoned",
            )
        })?;
        let matching_agents =
            attempt_bus_agents_in_cwd(state.agents.values(), &durable, &spec.cwd, started_at);
        if !matching_agents.is_empty() {
            let exact = matching_agents.len() == 1
                && (matching_agents[0].name == expected_agent_name
                    || matching_agents[0].base_name.as_deref()
                        == Some(expected_agent_name.as_str()))
                && spec
                    .groups
                    .iter()
                    .all(|group| matching_agents[0].groups.contains(group));
            return Ok(if exact {
                NativeBootstrapReconciliation::Present {
                    reason: "exact Agent Bus name/cwd/groups correlation is registered".to_string(),
                }
            } else {
                NativeBootstrapReconciliation::Ambiguous {
                    reason: "Agent Bus runtime occupies the reserved cwd with mismatched identity"
                        .to_string(),
                }
            });
        }
        drop(state);

        let matching_records = durable
            .sessions
            .values()
            .filter(|record| {
                (record.cwd == spec.cwd
                    || record.managed_cwd.as_deref() == Some(spec.cwd.as_str()))
                    && !predates_attempt(&record.created_at, started_at)
            })
            .collect::<Vec<_>>();
        if !matching_records.is_empty() {
            let exact = matching_records.len() == 1
                && (matching_records[0].thread_name.as_deref() == Some(spec.name.as_str())
                    || matching_records[0].display_name_hint.as_deref()
                        == Some(spec.name.as_str()))
                && spec
                    .groups
                    .iter()
                    .all(|group| matching_records[0].agent_groups.contains(group));
            return Ok(if exact {
                NativeBootstrapReconciliation::Present {
                    reason: "exact durable session name/cwd/groups correlation exists".to_string(),
                }
            } else {
                NativeBootstrapReconciliation::Ambiguous {
                    reason: "durable session occupies the reserved cwd with mismatched identity"
                        .to_string(),
                }
            });
        }

        let alden_sessions = cutex::runtime::alden::cute_alden_sessions()
            .map_err(unknown("native_bootstrap_reconciliation_unavailable"))?;
        if let Some(session) = alden_sessions
            .iter()
            .find(|session| session.name.as_deref() == Some(expected_alden_name.as_str()))
        {
            return Ok(
                if cutex::platform::process::process_is_running(session.pid) {
                    NativeBootstrapReconciliation::Present {
                        reason: "exact deterministic cute-alden session name has a live PID"
                            .to_string(),
                    }
                } else {
                    NativeBootstrapReconciliation::Ambiguous {
                        reason: "exact deterministic cute-alden session name has a stale PID"
                            .to_string(),
                    }
                },
            );
        }

        let codex_home = selected_profile_codex_home(&resolved)
            .map_err(unknown("native_bootstrap_reconciliation_unavailable"))?;
        match cutex::runtime::codex_home::correlate_codex_session_between_in_home(
            &codex_home,
            started_at,
            failed_at,
            Path::new(&spec.cwd),
        )
        .map_err(unknown("native_bootstrap_reconciliation_unavailable"))?
        {
            cutex::runtime::codex_home::NativeSessionCorrelation::Present { .. } => {
                Ok(NativeBootstrapReconciliation::Present {
                    reason: "native rollout/index evidence matches the exact managed cwd"
                        .to_string(),
                })
            }
            cutex::runtime::codex_home::NativeSessionCorrelation::Ambiguous { reason } => {
                Ok(NativeBootstrapReconciliation::Ambiguous { reason })
            }
            cutex::runtime::codex_home::NativeSessionCorrelation::ProvenAbsent => {
                Ok(NativeBootstrapReconciliation::ProvenAbsent {
                    reason: "Agent Bus, durable session, cute-alden, and correlated native sources are absent"
                        .to_string(),
                })
            }
        }
    }

    fn reconcile_interrupted_native_bootstrap(
        &self, spec: &ManagedAgentSpec, started_at: &Rfc3339, failed_at: &Rfc3339,
    ) -> Result<NativeBootstrapIdentityReconciliation, LifecycleFailure> {
        if is_claude_spec(spec).map_err(unknown("profile_preflight_failed"))? {
            return reconcile_claude_bootstrap(spec, started_at, failed_at);
        }

        // light_new owns the empty-thread creator with PDEATHSIG. The provider
        // mutation lock excludes a concurrent creator; its child cannot outlive
        // an interrupted owning process. Other backends do not make that claim.
        if !cfg!(target_os = "linux") || cutex::launch::local_deployment::LocalDeployment::selected()
            .map_err(unknown("native_bootstrap_reconciliation_unavailable"))?.is_none() {
            return Err(LifecycleFailure::outcome_unknown("native_bootstrap_reconciliation_unavailable", "interrupted recovery requires the local owned bootstrap backend"));
        }
        for entry in std::fs::read_dir("/proc").map_err(unknown("native_bootstrap_reconciliation_unavailable"))? {
            let entry = entry.map_err(unknown("native_bootstrap_reconciliation_unavailable"))?;
            if !entry.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()) { continue; }
            if std::fs::read_link(entry.path().join("cwd")).ok().as_deref() == Some(Path::new(&spec.cwd))
                && std::fs::read_link(entry.path().join("exe")).ok().and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned())).is_some_and(|n| n == "codex-app-server")
                // Only a light_new creator (private `cn-…/native.sock`
                // listener); live runtimes of Agents sharing the cwd are not.
                && std::fs::read(entry.path().join("cmdline")).ok().is_none_or(|cmdline| {
                    let cmdline = String::from_utf8_lossy(&cmdline);
                    cmdline.contains("/cn-") && cmdline.contains("native.sock")
                }) {
                return Ok(NativeBootstrapIdentityReconciliation::Unavailable { reason: "native creator is still exiting; retry after it has stopped".into() });
            }
        }
        self.reconcile_ambiguous_native_bootstrap(spec, started_at, failed_at)
    }

    fn reconcile_ambiguous_native_bootstrap(
        &self,
        spec: &ManagedAgentSpec,
        started_at: &Rfc3339,
        failed_at: &Rfc3339,
    ) -> Result<NativeBootstrapIdentityReconciliation, LifecycleFailure> {
        if is_claude_spec(spec).map_err(unknown("profile_preflight_failed"))? {
            return reconcile_claude_bootstrap(spec, started_at, failed_at);
        }

        let resolved = super::launch::resolve_launch_profile_override(
            spec.profile.as_deref().ok_or_else(|| {
                LifecycleFailure::definite("invalid_profile", "Create requires an explicit profile")
            })?,
        )
        .map_err(unknown("native_bootstrap_reconciliation_unavailable"))?;
        let expected_agent_name = cutex::agent_bus::identity::account_agent_name(&resolved.account);
        let expected_alden_name =
            cutex::runtime::managed_launch::default_managed_session_name_for_cwd(
                &resolved.account,
                Path::new(&spec.cwd),
            );
        let cwd_hash = cutex::agent_bus::identity::fnv1a_hex(&spec.cwd);
        let expected_runtime_groups = cutex::agent_bus::groups::normalize_registered_agent_groups(
            spec.groups.clone(),
            Some(&cwd_hash[..7]),
            &spec.cwd,
        );
        let mut candidates = BTreeSet::new();
        let mut identity_present_without_sid = false;
        let mut attempt_window_native_sid = None;

        // Agents share cwds: endpoints and records of Agents saved before
        // this attempt started are not evidence for it.
        let durable = load_cutex_session_store()
            .map_err(unknown("native_bootstrap_reconciliation_unavailable"))?;
        let state = self.state.lock().map_err(|_| {
            LifecycleFailure::outcome_unknown(
                "native_bootstrap_reconciliation_unavailable",
                "Agent Bus state lock is poisoned",
            )
        })?;
        let matching_agents =
            attempt_bus_agents_in_cwd(state.agents.values(), &durable, &spec.cwd, started_at);
        if !matching_agents.is_empty() {
            let exact = matching_agents.len() == 1
                && (matching_agents[0].name == expected_agent_name
                    || matching_agents[0].base_name.as_deref()
                        == Some(expected_agent_name.as_str()))
                && (matching_agents[0].groups == spec.groups
                    || matching_agents[0].groups == expected_runtime_groups)
                && cutex::platform::process::process_is_running(matching_agents[0].pid);
            if !exact {
                return Ok(NativeBootstrapIdentityReconciliation::Ambiguous {
                    reason: "Agent Bus runtime occupies the reserved cwd with mismatched identity"
                        .to_string(),
                });
            }
            match validated_reconciled_sid(
                matching_agents[0].session_id.as_deref(),
                "exact Agent Bus runtime",
            ) {
                Ok(session_id) => {
                    candidates.insert(session_id);
                    identity_present_without_sid = true;
                }
                Err(reason) => {
                    return Ok(NativeBootstrapIdentityReconciliation::Ambiguous { reason });
                }
            }
        }
        drop(state);

        let matching_records = durable
            .sessions
            .values()
            .filter(|record| {
                (record.cwd == spec.cwd
                    || record.managed_cwd.as_deref() == Some(spec.cwd.as_str()))
                    && !predates_attempt(&record.created_at, started_at)
            })
            .collect::<Vec<_>>();
        if !matching_records.is_empty() {
            let exact = matching_records.len() == 1
                && matching_records[0].is_active()
                && matching_records[0].retired_at.is_none()
                && cutex::runtime::lifecycle::cutex_session_host_is_local(
                    &matching_records[0].host_id,
                    &cutex::platform::host::current_host_name(),
                )
                && (matching_records[0].thread_name.as_deref() == Some(spec.name.as_str())
                    || matching_records[0].display_name_hint.as_deref()
                        == Some(spec.name.as_str()))
                && (matching_records[0].agent_groups == spec.groups
                    || matching_records[0].agent_groups == expected_runtime_groups);
            if !exact {
                return Ok(NativeBootstrapIdentityReconciliation::Ambiguous {
                    reason: "durable session occupies the reserved cwd with mismatched identity"
                        .to_string(),
                });
            }
            match validated_reconciled_sid(
                matching_records[0].codex_session_id.as_deref(),
                "exact durable session",
            ) {
                Ok(session_id) => {
                    candidates.insert(session_id);
                    identity_present_without_sid = true;
                }
                Err(reason) => {
                    return Ok(NativeBootstrapIdentityReconciliation::Ambiguous { reason });
                }
            }
        }

        let alden_sessions = cutex::runtime::alden::cute_alden_sessions()
            .map_err(unknown("native_bootstrap_reconciliation_unavailable"))?;
        let matching_alden = alden_sessions
            .iter()
            .filter(|session| session.name.as_deref() == Some(expected_alden_name.as_str()))
            .collect::<Vec<_>>();
        if matching_alden.len() > 1 {
            return Ok(NativeBootstrapIdentityReconciliation::Ambiguous {
                reason: "multiple cute-alden sessions have the deterministic managed name"
                    .to_string(),
            });
        }
        if let Some(session) = matching_alden.first() {
            if !cutex::platform::process::process_is_running(session.pid) {
                return Ok(NativeBootstrapIdentityReconciliation::Ambiguous {
                    reason: "deterministic cute-alden session has a stale PID".to_string(),
                });
            }
            identity_present_without_sid = true;
        }

        let codex_home = selected_profile_codex_home(&resolved)
            .map_err(unknown("native_bootstrap_reconciliation_unavailable"))?;
        match cutex::runtime::codex_home::correlate_codex_session_between_in_home(
            &codex_home,
            started_at,
            failed_at,
            Path::new(&spec.cwd),
        )
        .map_err(unknown("native_bootstrap_reconciliation_unavailable"))?
        {
            cutex::runtime::codex_home::NativeSessionCorrelation::Present { session_id } => {
                match validated_reconciled_sid(
                    Some(&session_id),
                    "selected-profile native rollout/index evidence",
                ) {
                    Ok(session_id) => {
                        candidates.insert(session_id.clone());
                        attempt_window_native_sid = Some(session_id);
                    }
                    Err(reason) => {
                        return Ok(NativeBootstrapIdentityReconciliation::Ambiguous { reason });
                    }
                }
            }
            cutex::runtime::codex_home::NativeSessionCorrelation::Ambiguous { reason } => {
                return Ok(NativeBootstrapIdentityReconciliation::Ambiguous { reason });
            }
            cutex::runtime::codex_home::NativeSessionCorrelation::ProvenAbsent => {}
        }

        let candidates = candidates.into_iter().collect::<Vec<_>>();
        match (attempt_window_native_sid.as_deref(), candidates.as_slice()) {
            (Some(window_sid), [native_session_id]) if window_sid == native_session_id => {
                let reason = "one native SID is shared by selected-profile rollout evidence and exact managed runtime sources"
                    .to_string();
                Ok(NativeBootstrapIdentityReconciliation::Exact {
                    native_session_id: native_session_id.clone(),
                    reason,
                })
            }
            (None, []) if identity_present_without_sid => {
                Ok(NativeBootstrapIdentityReconciliation::Ambiguous {
                    reason:
                        "exact runtime identity is present but no unique native SID is available"
                            .to_string(),
                })
            }
            (None, []) => Ok(NativeBootstrapIdentityReconciliation::Absent {
                reason:
                    "selected-profile native, Agent Bus, durable, and cute-alden evidence is absent"
                        .to_string(),
            }),
            (None, _) => Ok(NativeBootstrapIdentityReconciliation::Ambiguous {
                reason: "identity sources expose a native SID but no exact attempt-window rollout/index record"
                    .to_string(),
            }),
            (Some(_), candidates) => Ok(NativeBootstrapIdentityReconciliation::Ambiguous {
                reason: format!(
                    "provider-owned sources expose conflicting native SIDs: {}",
                    candidates.join(", ")
                ),
            }),
        }
    }

    fn reconcile_historical_runtime_occurrence(
        &self,
        cutex_session_id: &CutexSessionId,
    ) -> Result<HistoricalRuntimeOccurrenceReconciliation, LifecycleFailure> {
        self.historical_runtime_occurrence(cutex_session_id)
    }

    fn adopt_native(
        &self,
        native_session_id: &str,
        spec: &ManagedAgentSpec,
    ) -> Result<CutexSessionId, LifecycleFailure> {
        if is_claude_spec(spec).map_err(definite("profile_preflight_failed"))? {
            let store = load_cutex_session_store().map_err(definite("session_store_unavailable"))?;
            let matches: Vec<_> = store.sessions.values().filter(|r| r.claude_session.is_some()
                && r.app_server_thread_id() == Some(native_session_id)).collect();
            if let [record] = matches.as_slice() {
                return CutexSessionId::new(record.cutex_session_id.clone()).map_err(|_| LifecycleFailure::definite("invalid_durable_session", "Claude ID"));
            }
            return Err(LifecycleFailure::definite("claude_adoption_identity_mismatch", "Expected exactly one saved Claude adapter thread"));
        }
        let mut store =
            load_cutex_session_store().map_err(definite("session_store_unavailable"))?;
        let before = store.clone();
        let outcome = adopt_cutex_session(
            &mut store,
            native_session_id,
            CutexSessionEnsureSeed {
                host_id: cutex::platform::host::current_host_name(),
                cwd: spec.cwd.clone(),
                profile: spec.profile.clone(),
            },
            CutexSessionAdoptOptions {
                display_name: Some(&spec.name),
                managed_cwd: Some(spec.cwd.clone()),
                groups: spec.groups.clone(),
                expose_to_im: spec.expose_to_im,
                pin: spec.pin,
            },
        )
        .map_err(definite("session_adopt_failed"))?;
        cutex::session::store::save_cutex_session_record(&before, &store, &outcome.key)
            .and_then(|_| cutex::session::im_bridge::persist_cutex_session_im_record(&store, &outcome.key))
            .map_err(unknown("session_adopt_persistence_unknown"))?;
        CutexSessionId::new(outcome.key)
            .map_err(|_| LifecycleFailure::definite("invalid_durable_session", "adopted ID"))
    }

    fn configure(
        &self,
        cutex_session_id: &CutexSessionId,
        native_session_id: &str,
        spec: &ManagedAgentSpec,
    ) -> Result<(), LifecycleFailure> {
        let mut store =
            load_cutex_session_store().map_err(definite("session_store_unavailable"))?;
        let before = store.clone();
        let key =
            cutex_session_key_for_user_id_including_retired(&store, cutex_session_id.as_str())
                .ok_or_else(|| {
                    LifecycleFailure::definite("session_not_found", "adopted Agent missing")
                })?;
        let record = store.sessions.get_mut(&key).ok_or_else(|| {
            LifecycleFailure::definite("session_not_found", "adopted Agent disappeared")
        })?;
        if record.is_retired() || record.app_server_thread_id() != Some(native_session_id) {
            return Err(LifecycleFailure::definite(
                "session_identity_conflict",
                "adopted Agent does not bind the captured native session",
            ));
        }
        record.thread_name = Some(spec.name.clone());
        record.display_name_hint = Some(spec.name.clone());
        record.formal_agent_name = Some(spec.name.clone());
        record.managed_cwd = Some(spec.cwd.clone());
        record.profile = spec.profile.clone();
        record.runtime_backend = if record.claude_session.is_some() {
            CutexSessionRuntimeBackend::ClaudeSubscription
        } else if record.explicit_launch.is_some() {
            // Adoption installs a native host contract even for a legacy create
            // specification. Do not put its migrated record back on cute-alden.
            CutexSessionRuntimeBackend::Host
        } else {
            parse_cutex_session_runtime_backend(&spec.runtime_backend)
                .map_err(definite("invalid_runtime_backend"))?
        };
        record.agent_enabled = true;
        record.agent_groups = spec.groups.clone();
        record.registration_class = AgentRegistrationClass::Persistent;
        // `expose_to_im` is retired (`im-exposure-retired`): always set, so
        // older binaries list the agent after a rollback too.
        record.exposed_to_backend = true;
        record.quick_action = if spec.pin {
            CutexSessionQuickActionMode::Pinned
        } else {
            CutexSessionQuickActionMode::Auto
        };
        record.permission_defaults = Some(spec.permissions.clone());
        record.approval_policy = Some(spec.approval_policy.clone());
        record.sandbox_mode = Some(spec.sandbox_mode.clone());
        record.model_defaults = Some(spec.model.clone());
        record.reasoning_defaults = Some(spec.reasoning.clone());
        record
            .bump_durable_revision()
            .map_err(definite("session_revision_failed"))?;
        record.updated_at = chrono::Utc::now().to_rfc3339();
        cutex::session::store::save_cutex_session_record(&before, &store, &key)
            .and_then(|_| cutex::session::im_bridge::persist_cutex_session_im_record(&store, &key))
            .map_err(unknown("session_configuration_persistence_unknown"))
    }

    fn recover_runtime(
        &self,
        cutex_session_id: &CutexSessionId,
        native_session_id: &str,
        spec: &ManagedAgentSpec,
    ) -> Result<RuntimeRecoveryOutcome, LifecycleFailure> {
        let before = load_record(cutex_session_id)?;
        validate_managed_recovery_record(&before, cutex_session_id, native_session_id, spec)?;
        if before.claude_session.is_some() {
            return Ok(RuntimeRecoveryOutcome::NoClaim);
        }
        let generation = before.runtime_generation;
        let config = cutex::config::store::load_codez_config();
        let outcome = if before.explicit_launch.is_some() {
            super::native_management_lifecycle::recover(&before)
        } else {
            super::app_server_runtime::recover_persisted_runtime_for_lifecycle(&config, &before)
        }
        .map_err(unknown("runtime_recovery_failed"))?;
        let after = load_record(cutex_session_id)?;
        validate_managed_recovery_record(&after, cutex_session_id, native_session_id, spec)?;
        if after.runtime_generation != generation {
            return Err(LifecycleFailure::outcome_unknown(
                "runtime_recovery_generation_changed",
                "runtime recovery changed the claimed generation",
            ));
        }
        match outcome {
            super::app_server_runtime::ManagedRuntimeRecoveryOutcome::NoClaim => {
                Ok(RuntimeRecoveryOutcome::NoClaim)
            }
            super::app_server_runtime::ManagedRuntimeRecoveryOutcome::RecoveredExact => {
                if after.app_server_runtime != before.app_server_runtime
                    || after.current_runtime_agent_id != before.current_runtime_agent_id
                    || after.runtime_pid != before.runtime_pid
                {
                    return Err(LifecycleFailure::outcome_unknown(
                        "runtime_recovery_claim_changed",
                        "exact runtime recovery changed durable ownership",
                    ));
                }
                Ok(RuntimeRecoveryOutcome::RecoveredExact)
            }
            super::app_server_runtime::ManagedRuntimeRecoveryOutcome::ClearedDeadClaim => {
                if after.app_server_runtime.is_some()
                    || after.current_runtime_agent_id.is_some()
                    || after.runtime_pid.is_some()
                    || after.alden_pid.is_some()
                    || after.app_server_launch_claim_id.is_some()
                {
                    return Err(LifecycleFailure::outcome_unknown(
                        "runtime_recovery_cleanup_incomplete",
                        "dead runtime ownership claim was not fully fenced",
                    ));
                }
                Ok(RuntimeRecoveryOutcome::ClearedDeadClaim)
            }
        }
    }

    fn migration_required(&self, cutex_session_id: &CutexSessionId) -> Option<String> {
        let record = load_record(cutex_session_id).ok()?;
        cutex::agent_management::require_migrated_runtime(&record)
            .err()
            .map(|error| error.to_string())
    }

    fn online_managed(
        &self,
        permit: &cutex::agent_management::RuntimeExecutionPermit<'_>,
        cutex_session_id: &CutexSessionId,
    ) -> Result<(), LifecycleFailure> {
        let record = load_record(cutex_session_id)?;
        if record.explicit_launch.is_some() {
            super::native_management_lifecycle::online(permit, &record)
                .map_err(unknown("session_online_failed"))
        } else {
            self.online(cutex_session_id)
        }
    }

    fn online(&self, cutex_session_id: &CutexSessionId) -> Result<(), LifecycleFailure> {
        let record = load_record(cutex_session_id)?;
        if record.claude_session.is_some() {
            cutex::runtime::claude::start(cutex_session_id.as_str()).map_err(unknown("claude_online_failed"))?;
            return super::claude_runtime::observe(cutex_session_id.as_str()).map_err(unknown("claude_registration_failed"));
        }
        let entry = coding_registration_from_cutex_session_record(&record).ok_or_else(|| {
            LifecycleFailure::definite(
                "native_session_missing",
                "managed Agent has no bound native session",
            )
        })?;
        if record.app_server_runtime.is_some()
            && record.current_runtime_agent_id.is_some()
            && super::app_server_runtime::runtime_manager()
                .status(cutex_session_id.as_str())
                .ok()
                .flatten()
                .is_some_and(|status| status.connected)
        {
            return Ok(());
        }
        let config = cutex::config::store::load_codez_config();
        management_lifecycle::start_cutex_session_online_with_profile(&config, &entry, None)
            .map(|_| ())
            .map_err(unknown("session_online_failed"))
    }

    fn offline(&self, cutex_session_id: &CutexSessionId) -> Result<(), LifecycleFailure> {
        let record = load_record(cutex_session_id)?;
        if record.claude_session.is_some() {
            super::claude_runtime::disconnect(cutex_session_id.as_str()).map_err(unknown("claude_disconnect_failed"))?;
            return cutex::runtime::claude::close(cutex_session_id.as_str()).map(|_| ()).map_err(unknown("claude_close_failed"));
        }
        let Some(entry) = coding_registration_from_cutex_session_record(&record) else {
            if record.app_server_runtime.is_none()
                && record.runtime_pid.is_none()
                && record.current_runtime_agent_id.is_none()
            {
                return Ok(());
            }
            return Err(LifecycleFailure::definite(
                "runtime_identity_unknown",
                "managed Agent cannot project an exact runtime identity",
            ));
        };
        let config = cutex::config::store::load_codez_config();
        let live = management_lifecycle::try_live_agents_for_management_entry(&config, &entry)
            .map_err(unknown("agent_bus_observation_failed"))?;
        let result =
            management_lifecycle::stop_cutex_session_runtime_for_entry(&entry, &live, false)
                .map_err(unknown("session_offline_failed"))?;
        if result.stopped {
            Ok(())
        } else {
            Err(LifecycleFailure::outcome_unknown(
                "session_offline_failed",
                result.detail,
            ))
        }
    }

    fn offline_if_occurrence(
        &self,
        cutex_session_id: &CutexSessionId,
        expected: &RuntimeOccurrenceFence,
    ) -> Result<(), LifecycleFailure> {
        match self.historical_runtime_occurrence(cutex_session_id)? {
            HistoricalRuntimeOccurrenceReconciliation::ProvenAbsent { fence, .. }
                if &fence == expected =>
            {
                self.offline(cutex_session_id)
            }
            HistoricalRuntimeOccurrenceReconciliation::ProvenAbsent { .. } => {
                Err(LifecycleFailure::outcome_unknown(
                    "runtime_occurrence_changed",
                    "runtime absence fence changed before the offline effect",
                ))
            }
            HistoricalRuntimeOccurrenceReconciliation::Present { reason }
            | HistoricalRuntimeOccurrenceReconciliation::Ambiguous { reason }
            | HistoricalRuntimeOccurrenceReconciliation::Unavailable { reason } => Err(
                LifecycleFailure::outcome_unknown("runtime_occurrence_changed", reason),
            ),
        }
    }

    fn restart_managed_if_occurrence(
        &self,
        permit: &cutex::agent_management::RuntimeExecutionPermit<'_>,
        id: &CutexSessionId,
        expected: &RuntimeOccurrenceFence,
    ) -> Result<(AgentRuntimeObservation, AgentRuntimeObservation), LifecycleFailure> {
        let record = load_record(id)?;
        if record.explicit_launch.is_none() {
            return self.restart_if_occurrence(id, expected);
        }
        match self.historical_runtime_occurrence_for_record(&record)? {
            HistoricalRuntimeOccurrenceReconciliation::ProvenAbsent { fence, .. }
                if &fence == expected => {}
            _ => {
                return Err(LifecycleFailure::definite(
                    "runtime_occurrence_changed",
                    "fenced restart owner changed",
                ))
            }
        }
        let before = self.observe(id)?;
        self.online_managed(permit, id)?;
        Ok((before, self.observe(id)?))
    }

    fn restart_if_occurrence(
        &self,
        cutex_session_id: &CutexSessionId,
        expected: &RuntimeOccurrenceFence,
    ) -> Result<(AgentRuntimeObservation, AgentRuntimeObservation), LifecycleFailure> {
        let record = load_record(cutex_session_id)?;
        let entry = coding_registration_from_cutex_session_record(&record).ok_or_else(|| {
            LifecycleFailure::definite(
                "native_session_missing",
                "managed Agent has no bound native session",
            )
        })?;
        let before = self.observe(cutex_session_id)?;
        let validate =
            |current: &CutexSessionRecord| -> anyhow::Result<()> {
                match self.historical_runtime_occurrence_for_record(current) {
                    Ok(HistoricalRuntimeOccurrenceReconciliation::ProvenAbsent {
                        fence, ..
                    }) if &fence == expected => Ok(()),
                    Ok(HistoricalRuntimeOccurrenceReconciliation::ProvenAbsent { .. }) => {
                        anyhow::bail!("runtime absence fence changed before the fenced restart")
                    }
                    Ok(HistoricalRuntimeOccurrenceReconciliation::Present { reason })
                    | Ok(HistoricalRuntimeOccurrenceReconciliation::Ambiguous { reason })
                    | Ok(HistoricalRuntimeOccurrenceReconciliation::Unavailable { reason }) => {
                        anyhow::bail!(reason)
                    }
                    Err(error) => anyhow::bail!(error.detail),
                }
            };
        let config = cutex::config::store::load_codez_config();
        management_lifecycle::start_cutex_session_online_with_profile_if(
            &config, &entry, None, &validate,
        )
        .map_err(unknown("fenced_restart_failed"))?;
        let after = self.observe(cutex_session_id)?;
        Ok((before, after))
    }

    fn retire(&self, cutex_session_id: &CutexSessionId) -> Result<(), LifecycleFailure> {
        let record = load_record(cutex_session_id)?;
        if record.is_retired() {
            return Ok(());
        }
        management_archive::mutate_management_v2_archive(
            cutex_session_id.as_str(),
            "cutex/session/retire",
            &serde_json::json!({
                "expectedRevision": record.revision,
                "expectedRuntimeGeneration": record.runtime_generation,
            }),
        )
        .map(|_| ())
        .map_err(|error| {
            LifecycleFailure::outcome_unknown(
                "session_retire_failed",
                format!("typed retirement failed: {error:?}"),
            )
        })
    }

    fn observe(
        &self,
        cutex_session_id: &CutexSessionId,
    ) -> Result<AgentRuntimeObservation, LifecycleFailure> {
        let record = load_record(cutex_session_id)?;
        let native_session_id = record.app_server_thread_id().map(str::to_string).ok_or_else(|| {
            LifecycleFailure::definite(
                "native_session_missing",
                "managed Agent has no bound native session",
            )
        })?;
        let runtime_agent_ids = record
            .current_runtime_agent_id
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        let bus_ids = if record.claude_session.is_some() {
            let state = self.state.lock().map_err(|_| LifecycleFailure::outcome_unknown("bus_lock_failed", "Agent Bus unavailable"))?;
            state.agents.values().filter(|a| a.cutex_session_id.as_deref() == Some(cutex_session_id.as_str())
                && record.current_runtime_agent_id.as_deref() == Some(a.id.as_str())).map(|a| a.id.clone()).collect()
        } else if let Some(entry) = coding_registration_from_cutex_session_record(&record) {
            let config = cutex::config::store::load_codez_config();
            management_lifecycle::try_live_agents_for_management_entry(&config, &entry)
                .map_err(unknown("agent_bus_observation_failed"))?
                .into_iter()
                .map(|agent| agent.id)
                .collect()
        } else {
            Vec::new()
        };
        let manager = if record.claude_session.is_some() { super::claude_runtime::manager() } else { super::app_server_runtime::runtime_manager() };
        let app_server_runtime = (record.app_server_runtime.is_some() || record.claude_session.is_some())
            && manager
                .status(cutex_session_id.as_str())
                .map_err(unknown("app_server_observation_failed"))?
                .is_some_and(|status| status.connected);
        Ok(AgentRuntimeObservation {
            cutex_session_id: cutex_session_id.clone(),
            native_session_id,
            active: record.is_active(),
            cwd: cutex_session_launch_cwd(&record).to_string(),
            profile: record.profile.unwrap_or_default(),
            runtime_backend: serde_json::to_value(record.runtime_backend)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_default(),
            model: record.model_defaults.unwrap_or_default(),
            reasoning: record.reasoning_defaults.unwrap_or_default(),
            permissions: record.permission_defaults.unwrap_or_default(),
            approval_policy: record.approval_policy.unwrap_or_default(),
            sandbox_mode: record.sandbox_mode.unwrap_or_default(),
            groups: record.agent_groups,
            runtime_generation: record.runtime_generation,
            runtime_version: record
                .app_server_runtime
                .as_ref()
                .and_then(|binding| {
                    cutex::launch::version_registry::load_relaxed().and_then(|registry| {
                        registry.runtime_version_for_pid(binding.pid)
                    })
                })
                .or_else(|| {
                    record.runtime_pid.and_then(|pid| {
                        cutex::launch::version_registry::load_relaxed()
                            .and_then(|registry| registry.runtime_version_for_pid(pid))
                    })
                })
                .or_else(|| {
                    record
                .explicit_launch
                .as_ref()
                .and_then(|contract| {
                    cutex::launch::version_registry::load_relaxed().and_then(|registry| {
                        registry.runtime_version_for_launch_contract(contract)
                    })
                    })
                }),
            runtime_agent_ids,
            app_server_runtime,
            agent_bus_endpoint_ids: bus_ids,
        })
    }

    fn send_message(
        &self,
        system: &cutex::agent_bus::identity::AgentManagementSystemPrincipal,
        metadata: &AgentManagementMessageMetadata,
        to_agent: &CutexSessionId,
        exact_message: &str,
        external_message_id: &str,
    ) -> Result<String, LifecycleFailure> {
        let target = load_record(to_agent)?;
        let target_runtime = target.current_runtime_agent_id.clone().ok_or_else(|| {
            LifecycleFailure::definite("target_offline", "target Agent has no runtime endpoint")
        })?;
        let source = load_record(&metadata.requested_by_director)?;
        let (from_agent_id, from_session_id) = agent_message_source_binding(
            &metadata.requested_by_director,
            source.current_runtime_agent_id,
        );
        let response = super::agent_bus_server::send_agent_management_system_message_response(
            &self.state,
            system,
            super::agent_bus_server::AgentManagementSystemMessage {
                metadata,
                from_agent_id,
                from_session_id,
                target_runtime_agent_id: &target_runtime,
                target_cutex_session_id: to_agent,
                exact_message,
                external_message_id,
            },
        )
        .map_err(unknown("agent_message_outcome_unknown"))?;
        let response: AgentBusSendResponse =
            serde_json::from_value(response).map_err(definite("invalid_agent_message_receipt"))?;
        if response.to_runtime_agent_id.as_deref() != Some(target_runtime.as_str())
            || response.external_message_id.as_deref() != Some(external_message_id)
        {
            return Err(LifecycleFailure::outcome_unknown(
                "agent_message_receipt_conflict",
                "Agent Bus receipt did not preserve the exact target and external message ID",
            ));
        }
        Ok(response.id)
    }
}

fn agent_message_source_binding(
    from_director: &CutexSessionId,
    current_runtime_agent_id: Option<String>,
) -> (Option<String>, Option<String>) {
    let from_session_id = current_runtime_agent_id
        .as_ref()
        .map(|_| from_director.as_str().to_string());
    (current_runtime_agent_id, from_session_id)
}

fn load_record(
    cutex_session_id: &CutexSessionId,
) -> Result<cutex::session::model::CutexSessionRecord, LifecycleFailure> {
    let store = load_cutex_session_store().map_err(definite("session_store_unavailable"))?;
    let key = cutex_session_key_for_user_id_including_retired(&store, cutex_session_id.as_str())
        .ok_or_else(|| LifecycleFailure::definite("session_not_found", "managed Agent missing"))?;
    store
        .sessions
        .get(&key)
        .cloned()
        .ok_or_else(|| LifecycleFailure::definite("session_not_found", "managed Agent disappeared"))
}

pub(super) fn validate_managed_recovery_record(
    record: &CutexSessionRecord,
    cutex_session_id: &CutexSessionId,
    native_session_id: &str,
    spec: &ManagedAgentSpec,
) -> Result<(), LifecycleFailure> {
    let backend = parse_cutex_session_runtime_backend(&spec.runtime_backend)
        .map_err(definite("invalid_runtime_backend"))?;
    let cwd_hash = cutex::agent_bus::identity::fnv1a_hex(&spec.cwd);
    let effective_groups = cutex::agent_bus::groups::normalize_registered_agent_groups(
        spec.groups.clone(),
        Some(&cwd_hash[..7]),
        &spec.cwd,
    );
    let groups_match =
        record.agent_groups == spec.groups || record.agent_groups == effective_groups;
    let quick_action = if spec.pin {
        CutexSessionQuickActionMode::Pinned
    } else {
        CutexSessionQuickActionMode::Auto
    };
    // The durable record's profile may be inherited, cleared, or changed after
    // creation. It is launch configuration, not recovery identity.
    let mismatch = if !record.is_active() {
        Some("active")
    } else if record.cutex_session_id != cutex_session_id.as_str() {
        Some("cutex_session_id")
    } else if !cutex::runtime::lifecycle::cutex_session_host_is_local(
        &record.host_id,
        &cutex::platform::host::current_host_name(),
    ) {
        Some("host_id")
    } else if record.app_server_thread_id() != Some(native_session_id) {
        Some("native_session_id")
    } else if record
        .managed_cwd
        .as_deref()
        .is_some_and(|cwd| cwd != spec.cwd)
    {
        Some("managed_cwd")
    } else if cutex_session_launch_cwd(record) != spec.cwd {
        Some("launch_cwd")
    } else if record.runtime_backend != backend
        && !(record.explicit_launch.is_some()
            && record.runtime_backend == CutexSessionRuntimeBackend::Host
            && backend == CutexSessionRuntimeBackend::CuteAlden)
    {
        Some("runtime_backend")
    } else if record.formal_agent_name.is_none()
        && record.thread_name.as_deref() != Some(spec.name.as_str())
    {
        Some("thread_name")
    } else if record.formal_agent_name.is_none()
        && record.display_name_hint.as_deref() != Some(spec.name.as_str())
    {
        Some("display_name_hint")
    } else if !record.agent_enabled {
        Some("agent_enabled")
    } else if !groups_match {
        Some("groups")
    } else if record.registration_class != AgentRegistrationClass::Persistent {
        Some("registration_class")
    } else if record.quick_action != quick_action {
        Some("pin")
    } else if record.permission_defaults.as_deref().unwrap_or_default() != spec.permissions {
        Some("permissions")
    } else if record.approval_policy.as_deref().unwrap_or_default() != spec.approval_policy {
        Some("approval_policy")
    } else if record.sandbox_mode.as_deref().unwrap_or_default() != spec.sandbox_mode {
        Some("sandbox_mode")
    } else if record.model_defaults.as_deref().unwrap_or_default() != spec.model {
        Some("model")
    } else if record.reasoning_defaults.as_deref().unwrap_or_default() != spec.reasoning {
        Some("reasoning")
    } else {
        None
    };
    match mismatch {
        Some(field) => Err(LifecycleFailure::definite(
            "runtime_recovery_spec_mismatch",
            format!("durable managed runtime mismatch: {field}"),
        )),
        None => Ok(()),
    }
}

fn prepare_private_managed_cwd(path: &Path) -> anyhow::Result<()> {
    if !path.is_absolute() || path.parent().is_none() {
        anyhow::bail!("managed Agent cwd must be an absolute non-root path");
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            anyhow::bail!("managed Agent cwd must be a real directory")
        }
        // An existing directory (often the shared project workspace) keeps
        // its permissions: only a directory created here is made private.
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path).with_context(|| {
                format!("failed to create managed Agent cwd: {}", path.display())
            })?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            }
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct NativeBootstrapPlan {
    executable: PathBuf,
    cwd: PathBuf,
    args: Vec<String>,
}

fn validated_reconciled_sid(value: Option<&str>, source: &str) -> Result<String, String> {
    let value = value
        .and_then(|value| uuid::Uuid::parse_str(value).ok())
        .ok_or_else(|| format!("{source} does not expose one valid native SID"))?;
    Ok(value.to_string())
}

fn selected_profile_codex_home(
    resolved: &super::launch::ResolvedLaunchProfile,
) -> anyhow::Result<PathBuf> {
    match &resolved.account.runtime {
        cutex::profiles::model::RuntimeConfig::Host => cutex::config::paths::host_codex_home_dir(),
        cutex::profiles::model::RuntimeConfig::Docker { .. } => anyhow::bail!("Cutex Docker integration was retired"),
    }
}

impl NativeBootstrapPlan {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.executable);
        command.current_dir(&self.cwd).args(&self.args);
        command
    }
}

fn native_bootstrap_plan(spec: &ManagedAgentSpec) -> anyhow::Result<NativeBootstrapPlan> {
    let executable = std::env::current_exe().context("failed to resolve Cutex executable")?;
    let mut args = vec![
        "run".to_string(),
        spec.profile
            .as_deref()
            .context("Create requires an explicit profile")?
            .to_string(),
        "--agent".to_string(),
    ];
    for group in &spec.groups {
        args.push("--group".to_string());
        args.push(group.clone());
    }
    args.extend([
        "--".to_string(),
        "exec".to_string(),
        "--json".to_string(),
        "--skip-git-repo-check".to_string(),
        "--cutex-top-level-session".to_string(),
        "Hi.".to_string(),
    ]);
    Ok(NativeBootstrapPlan {
        executable,
        cwd: PathBuf::from(&spec.cwd),
        args,
    })
}

fn captured_native_session_id(output: &Output) -> Result<String, LifecycleFailure> {
    let mut candidates = BTreeSet::new();
    let stdout = std::str::from_utf8(&output.stdout).map_err(|_| {
        LifecycleFailure::outcome_unknown(
            "native_bootstrap_output_malformed",
            "native exec JSONL stdout is not valid UTF-8",
        )
    })?;
    for (line_index, line) in stdout.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event: serde_json::Value = serde_json::from_str(line).map_err(|_| {
            LifecycleFailure::outcome_unknown(
                "native_bootstrap_output_malformed",
                format!(
                    "native exec JSONL stdout line {} is not valid JSON",
                    line_index + 1
                ),
            )
        })?;
        let event = event.as_object().ok_or_else(|| {
            LifecycleFailure::outcome_unknown(
                "native_bootstrap_output_malformed",
                format!(
                    "native exec JSONL stdout line {} is not an event object",
                    line_index + 1
                ),
            )
        })?;
        let event_type = event
            .get("type")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                LifecycleFailure::outcome_unknown(
                    "native_bootstrap_output_malformed",
                    format!(
                        "native exec JSONL stdout line {} omitted an event type",
                        line_index + 1
                    ),
                )
            })?;
        if event_type != "thread.started" {
            continue;
        }
        let thread_id = event
            .get("thread_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                LifecycleFailure::outcome_unknown(
                    "native_bootstrap_output_malformed",
                    format!(
                        "native exec thread.started event on line {} omitted a valid thread_id",
                        line_index + 1
                    ),
                )
            })?;
        let thread_id = uuid::Uuid::parse_str(thread_id).map_err(|_| {
            LifecycleFailure::outcome_unknown(
                "native_bootstrap_output_malformed",
                format!(
                    "native exec thread.started event on line {} omitted a valid thread_id",
                    line_index + 1
                ),
            )
        })?;
        candidates.insert(thread_id.to_string());
    }
    let candidates = candidates.into_iter().collect::<Vec<_>>();
    if candidates.is_empty() && required_top_level_flag_was_rejected(output) {
        return Err(LifecycleFailure::definite(
            "native_bootstrap_incompatible",
            "cute-codex does not support the required --cutex-top-level-session flag; upgrade cute-codex before creating a managed Agent",
        ));
    }
    match candidates.as_slice() {
        [session_id] if output.status.success() => Ok(session_id.clone()),
        [session_id] => Err(LifecycleFailure {
            code: "native_bootstrap_failed".to_string(),
            detail: format!(
                "native exec exited {} after creating a thread",
                output.status
            ),
            outcome_unknown: true,
            known_native_session_id: Some(session_id.clone()),
        }),
        [] => {
            let diagnostic = native_bootstrap_diagnostic(output)
                .map(|diagnostic| format!("; diagnostic: {diagnostic}"))
                .unwrap_or_default();
            Err(LifecycleFailure::outcome_unknown(
                "native_session_unknown",
                format!(
                    "native exec exited {} without one captured SID{diagnostic}",
                    output.status
                ),
            ))
        }
        _ => Err(LifecycleFailure::outcome_unknown(
            "native_session_ambiguous",
            "native exec exposed multiple possible session identities",
        )),
    }
}

fn native_bootstrap_diagnostic(output: &Output) -> Option<String> {
    let mut diagnostics = Vec::new();
    for (source, bytes) in [
        ("stdout", output.stdout.as_slice()),
        ("stderr", output.stderr.as_slice()),
    ] {
        let value = String::from_utf8_lossy(bytes);
        if let Some(value) = sanitized_output_tail(&value) {
            diagnostics.push(format!("{source}: {value}"));
        }
    }
    (!diagnostics.is_empty()).then(|| diagnostics.join("; "))
}

fn sanitized_output_tail(value: &str) -> Option<String> {
    let safe = cutex::observability::sanitize_visible_output(value)?;
    if safe == "[redacted sensitive output]" {
        return Some(safe);
    }
    // Startup wrappers commonly write several informational lines before the
    // actionable failure. Preserve the bounded tail while still running the
    // sensitive-data scan above against the complete raw stream.
    let tail = value
        .lines()
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    let tail = tail
        .chars()
        .rev()
        .take(cutex::observability::OBSERVABILITY_TEXT_LIMIT)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    cutex::observability::sanitize_visible_output(&tail)
}

fn required_top_level_flag_was_rejected(output: &Output) -> bool {
    if output.status.success() {
        return false;
    }
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    stderr.contains("--cutex-top-level-session")
        && [
            "unexpected argument",
            "unknown argument",
            "unrecognized argument",
            "unknown option",
            "unrecognized option",
        ]
        .iter()
        .any(|marker| stderr.contains(marker))
}

fn definite<E: std::fmt::Display>(code: &'static str) -> impl FnOnce(E) -> LifecycleFailure {
    move |error| LifecycleFailure::definite(code, error.to_string())
}

fn unknown<E: std::fmt::Display>(code: &'static str) -> impl FnOnce(E) -> LifecycleFailure {
    move |error| LifecycleFailure::outcome_unknown(code, error.to_string())
}

/// Test hook for the cross-path creation matrix (management_context tests):
/// agent_create's configuration check + native bootstrap for `spec`, exactly
/// as `execute` runs them, without the reservation / adoption steps.
#[cfg(test)]
pub(super) fn bootstrap_for_test(spec: &ManagedAgentSpec) -> Result<String, LifecycleFailure> {
    let lifecycle = CutexAgentLifecycle {
        state: std::sync::Arc::new(std::sync::Mutex::new(Default::default())),
    };
    let effective = lifecycle.effective_create_spec(spec)?;
    lifecycle.bootstrap_native(&effective)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Agents share cwds: records saved before a bootstrap attempt are other
    /// Agents; unparsable times stay evidence.
    #[test]
    fn shared_cwd_records_before_the_attempt_are_not_evidence() {
        let started = Rfc3339::new("2026-10-11T10:00:00Z".to_string()).unwrap();
        assert!(predates_attempt("2026-10-11T09:00:00Z", &started));
        assert!(!predates_attempt("2026-10-11T09:59:59Z", &started));
        assert!(!predates_attempt("2026-10-11T10:00:05Z", &started));
        assert!(!predates_attempt("not a time", &started));
    }

    /// An existing (shared) workspace keeps its permissions; only a created
    /// cwd is made private.
    #[test]
    fn private_cwd_preparation_leaves_an_existing_workspace_alone() {
        let root = std::env::temp_dir().join(format!("cutex-private-cwd-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        prepare_private_managed_cwd(&root).unwrap();
        let created = root.join("member");
        prepare_private_managed_cwd(&created).unwrap();
        assert!(created.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&root), 0o755);
            assert_eq!(mode(&created), 0o700);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    fn resolved_profile(
        runtime: cutex::profiles::model::RuntimeConfig,
    ) -> super::super::launch::ResolvedLaunchProfile {
        let account = cutex::profiles::model::StoredAccount {
            options: Default::default(),
            browser_tools: false,
            search_tool: false,
            quota_reader: None,
            private_chatgpt_auth_file: false,
            id: "account-id".to_string(),
            name: "profile".to_string(),
            email: None,
            plan_type: None,
            plan_label: None,
            quota_preset: Default::default(),
            source: None,
            runtime,
            proxy: None,
            session: None,
            cli_kind: cutex::profiles::model::CliKind::Codex,
            default_cli_args: Vec::new(),
            agent_name: None,
            last_used_at: None,
            provider_category: None,
            claude_account: None,
            model_display_names: Default::default(),
        };
        let files = cutex::profiles::materialize::materialized_account_files(&account).unwrap();
        super::super::launch::ResolvedLaunchProfile {
            requested: account.name.clone(),
            account,
            files,
        }
    }

    fn spec() -> ManagedAgentSpec {
        ManagedAgentSpec {
            name: "worker-r1".to_string(),
            cwd: if cfg!(windows) {
                r"C:\tmp\project\agent-home\worker-r1"
            } else {
                "/tmp/project/agent-home/worker-r1"
            }
            .to_string(),
            profile: Some("orchid".to_string()),
            runtime_backend: "cute_alden".to_string(),
            model: "gpt-5.6-sol".to_string(),
            reasoning: "high".to_string(),
            permissions: "danger-full-access".to_string(),
            approval_policy: "never".to_string(),
            sandbox_mode: "danger-full-access".to_string(),
            groups: vec!["cutex".to_string(), "task:cutex-205".to_string()],
            expose_to_im: true,
            pin: false,
        }
    }

    fn managed_recovery_record(spec: &ManagedAgentSpec) -> CutexSessionRecord {
        let mut record = CutexSessionRecord::new_at(
            "cutex.worker-r1".to_string(),
            Some("018e0000-0000-7000-8000-0000b0009ae4".to_string()),
            cutex::platform::host::current_host_name(),
            spec.cwd.clone(),
            spec.profile.clone(),
            "2026-08-29T00:00:00Z".to_string(),
        )
        .expect("managed recovery record");
        record.managed_cwd = Some(spec.cwd.clone());
        record.runtime_backend =
            parse_cutex_session_runtime_backend(&spec.runtime_backend).unwrap();
        record.thread_name = Some(spec.name.clone());
        record.display_name_hint = Some(spec.name.clone());
        record.formal_agent_name = Some(spec.name.clone());
        record.agent_enabled = true;
        record.agent_groups = spec.groups.clone();
        record.registration_class = AgentRegistrationClass::Persistent;
        record.exposed_to_backend = spec.expose_to_im;
        record.quick_action = CutexSessionQuickActionMode::Auto;
        record.permission_defaults = Some(spec.permissions.clone());
        record.approval_policy = Some(spec.approval_policy.clone());
        record.sandbox_mode = Some(spec.sandbox_mode.clone());
        record.model_defaults = Some(spec.model.clone());
        record.reasoning_defaults = Some(spec.reasoning.clone());
        record
    }

    #[test]
    fn migrated_native_backend_does_not_reapply_legacy_worker_backend() {
        let expected = spec();
        let mut record = managed_recovery_record(&expected);
        let id = CutexSessionId::new(record.cutex_session_id.clone()).unwrap();
        let native = record.codex_session_id.clone().unwrap();
        record.runtime_backend = CutexSessionRuntimeBackend::Host;
        assert!(validate_managed_recovery_record(&record, &id, &native, &expected).is_err());
        record.explicit_launch = Some(cutex::agent_management::ExplicitLaunchContract {
            version: 4,
            migration_action_id: None,
            native_id: native.clone(),
            native_home: "/tmp/native".into(),
            bundle_manifest: "/tmp/bundle.json".into(),
            bundle_sha256: cutex::role_revision::Sha256::new("a".repeat(64)).unwrap(),
        });
        validate_managed_recovery_record(&record, &id, &native, &expected).unwrap();
        record.runtime_backend = CutexSessionRuntimeBackend::HostForeground;
        assert!(validate_managed_recovery_record(&record, &id, &native, &expected).is_err());
    }

    #[test]
    fn legacy_director_import_evidence_uses_exact_authoritative_session_fields() {
        let expected = spec();
        let director = CutexSessionId::new("cutex.worker-r1").unwrap();
        let mut record = managed_recovery_record(&expected);
        record.revision = 9;
        record.runtime_generation = 4;
        let evidence = legacy_director_ownership_evidence_from_record(&director, &record)
            .expect("exact legacy Director session evidence");
        assert_eq!(evidence.director_cutex_session_id, director);
        assert_eq!(
            evidence.native_session_id,
            "018e0000-0000-7000-8000-0000b0009ae4"
        );
        assert_eq!(evidence.durable_session_revision, 9);
        assert_eq!(evidence.runtime_generation, 4);
        assert_eq!(evidence.spec, expected);
    }

    #[test]
    fn legacy_director_import_evidence_rejects_missing_mismatched_and_ambiguous_sessions() {
        let expected = spec();
        let director = CutexSessionId::new("cutex.worker-r1").unwrap();
        let record = managed_recovery_record(&expected);

        macro_rules! reject_record_change {
            ($label:literal, $change:expr) => {{
                let mut changed = record.clone();
                $change(&mut changed);
                assert!(
                    legacy_director_ownership_evidence_from_record(&director, &changed).is_err(),
                    "accepted {} state",
                    $label
                );
            }};
        }

        reject_record_change!(
            "durable identity mismatch",
            |value: &mut CutexSessionRecord| {
                value.cutex_session_id = "cutex.other-director".to_string()
            }
        );
        reject_record_change!("retired", |value: &mut CutexSessionRecord| {
            value.archive_state = cutex::session::model::CutexSessionArchiveState::Retired;
            value.retired_at = Some("2026-08-29T01:00:00Z".to_string())
        });
        reject_record_change!(
            "native identity missing",
            |value: &mut CutexSessionRecord| { value.codex_session_id = None }
        );
        reject_record_change!("remote host", |value: &mut CutexSessionRecord| {
            value.host_id = "definitely-remote-host".to_string()
        });
        reject_record_change!("non-managed", |value: &mut CutexSessionRecord| {
            value.agent_enabled = false
        });
        reject_record_change!(
            "unresolved launch claim",
            |value: &mut CutexSessionRecord| {
                value.app_server_launch_claim_id = Some("ambiguous-claim".to_string())
            }
        );
        reject_record_change!("name mismatch", |value: &mut CutexSessionRecord| {
            value.display_name_hint = Some("other-name".to_string())
        });
        reject_record_change!("managed cwd missing", |value: &mut CutexSessionRecord| {
            value.managed_cwd = None
        });
        reject_record_change!("hidden quick action", |value: &mut CutexSessionRecord| {
            value.quick_action = CutexSessionQuickActionMode::Hidden
        });
        reject_record_change!("profile missing", |value: &mut CutexSessionRecord| {
            value.profile = None
        });
        reject_record_change!("noncanonical groups", |value: &mut CutexSessionRecord| {
            value.agent_groups.push("cutex".to_string())
        });
        reject_record_change!("zero durable revision", |value: &mut CutexSessionRecord| {
            value.revision = 0
        });
    }

    #[test]
    fn managed_runtime_recovery_requires_exact_durable_native_and_stable_spec_identity() {
        let spec = spec();
        let cutex_session_id = CutexSessionId::new("cutex.worker-r1").unwrap();
        let native_session_id = "018e0000-0000-7000-8000-0000b0009ae4";
        let record = managed_recovery_record(&spec);
        validate_managed_recovery_record(&record, &cutex_session_id, native_session_id, &spec)
            .expect("exact managed record is adoptable");

        macro_rules! reject_record_change {
            ($label:literal, $change:expr) => {{
                let mut changed = record.clone();
                $change(&mut changed);
                assert!(
                    validate_managed_recovery_record(
                        &changed,
                        &cutex_session_id,
                        native_session_id,
                        &spec,
                    )
                    .is_err(),
                    "accepted {} mismatch",
                    $label
                );
            }};
        }

        reject_record_change!("durable session", |value: &mut CutexSessionRecord| {
            value.cutex_session_id = "cutex.other-worker".to_string()
        });
        reject_record_change!("native session", |value: &mut CutexSessionRecord| {
            value.codex_session_id = Some("018e0000-0000-7000-8000-0000b0009ae5".to_string())
        });
        reject_record_change!("host", |value: &mut CutexSessionRecord| {
            value.host_id = "definitely-remote-host".to_string()
        });
        reject_record_change!("managed cwd", |value: &mut CutexSessionRecord| {
            value.managed_cwd = Some("/tmp/other-managed-cwd".to_string())
        });
        let mut renamed_title = record.clone();
        renamed_title.thread_name = Some("other native title".to_string());
        renamed_title.formal_agent_name = Some("Renamed formal Agent".into());
        validate_managed_recovery_record(
            &renamed_title,
            &cutex_session_id,
            native_session_id,
            &spec,
        )
        .expect("formal rename and native title do not change identity");
        reject_record_change!("groups", |value: &mut CutexSessionRecord| {
            value.agent_groups.push("unexpected-authority".to_string())
        });
        reject_record_change!("permissions", |value: &mut CutexSessionRecord| {
            value.permission_defaults = Some("read-only".to_string())
        });
        reject_record_change!("approval policy", |value: &mut CutexSessionRecord| {
            value.approval_policy = Some("on-request".to_string())
        });
        reject_record_change!("sandbox", |value: &mut CutexSessionRecord| {
            value.sandbox_mode = Some("workspace-write".to_string())
        });
        reject_record_change!("model", |value: &mut CutexSessionRecord| {
            value.model_defaults = Some("other-model".to_string())
        });
        reject_record_change!("reasoning", |value: &mut CutexSessionRecord| {
            value.reasoning_defaults = Some("low".to_string())
        });
    }

    #[test]
    fn managed_runtime_recovery_accepts_null_empty_or_changed_durable_profile() {
        let spec = spec();
        let cutex_session_id = CutexSessionId::new("cutex.worker-r1").unwrap();
        let native_session_id = "018e0000-0000-7000-8000-0000b0009ae4";

        for profile in [None, Some(""), Some("quince")] {
            let mut record = managed_recovery_record(&spec);
            record.profile = profile.map(str::to_string);
            let result = validate_managed_recovery_record(
                &record,
                &cutex_session_id,
                native_session_id,
                &spec,
            );
            assert!(
                result.is_ok(),
                "spec profile orchid must not reject observed profile {:?}: {:?}",
                profile,
                result
            );
        }
    }

    #[test]
    fn managed_runtime_recovery_accepts_only_deterministic_system_groups() {
        let spec = spec();
        let cutex_session_id = CutexSessionId::new("cutex.worker-r1").unwrap();
        let native_session_id = "018e0000-0000-7000-8000-0000b0009ae4";
        let mut record = managed_recovery_record(&spec);
        let cwd_hash = cutex::agent_bus::identity::fnv1a_hex(&spec.cwd);
        record.agent_groups = cutex::agent_bus::groups::normalize_registered_agent_groups(
            spec.groups.clone(),
            Some(&cwd_hash[..7]),
            &spec.cwd,
        );

        validate_managed_recovery_record(&record, &cutex_session_id, native_session_id, &spec)
            .expect("deterministic system group expansion is allowed");
    }

    #[test]
    fn bootstrap_plan_marks_new_top_level_cutex_session_before_prompt() {
        let spec = spec();
        let plan = native_bootstrap_plan(&spec).unwrap();
        assert_eq!(plan.cwd, PathBuf::from(&spec.cwd));
        assert_eq!(
            plan.args,
            [
                "run",
                "orchid",
                "--agent",
                "--group",
                "cutex",
                "--group",
                "task:cutex-205",
                "--",
                "exec",
                "--json",
                "--skip-git-repo-check",
                "--cutex-top-level-session",
                "Hi."
            ]
        );
    }

    #[test]
    fn reconciliation_selects_host_home_and_rejects_retired_docker() {
        let home = crate::cli_app::test_home::IsolatedTestHome::new(
            "cutex-agent-management-profile-codex-home",
        )
        .unwrap();
        let host = resolved_profile(cutex::profiles::model::RuntimeConfig::Host);
        assert_eq!(
            selected_profile_codex_home(&host).unwrap(),
            home.root().join(".cutex").join("codex-home")
        );

        let docker = resolved_profile(cutex::profiles::model::RuntimeConfig::Docker {
            image: "cutex-dev".to_string(),
            user_name: Some("worker".to_string()),
        });
        assert!(selected_profile_codex_home(&docker).unwrap_err().to_string().contains("Docker integration was retired"));
    }

    #[cfg(unix)]
    fn output(status: i32, stdout: &str, stderr: &str) -> Output {
        use std::os::unix::process::ExitStatusExt;
        Output {
            status: std::process::ExitStatus::from_raw(status << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn sid_capture_accepts_only_structured_thread_started_identity() {
        let sid = "018e0000-0000-7000-8000-0000b0009ae4";
        let unrelated = "018e0000-0000-7000-8000-0000b0009ae9";
        assert_eq!(
            captured_native_session_id(&output(
                0,
                &format!(
                    "{{\"type\":\"profile.loaded\",\"account_id\":\"{unrelated}\"}}\n\
                     {{\"type\":\"thread.started\",\"thread_id\":\"{sid}\"}}\n\
                     {{\"type\":\"action.completed\",\"action_id\":\"{unrelated}\"}}\n"
                ),
                &format!("profile id: {unrelated}\n"),
            ))
            .unwrap(),
            sid
        );
    }

    #[cfg(unix)]
    #[test]
    fn sid_capture_rejects_conflicting_or_malformed_structured_events() {
        let sid = "018e0000-0000-7000-8000-0000b0009ae4";
        let other_sid = "018e0000-0000-7000-8000-0000b0009ae5";
        let error = captured_native_session_id(&output(
            0,
            &format!(
                "{{\"type\":\"thread.started\",\"thread_id\":\"{sid}\"}}\n\
                 {{\"type\":\"thread.started\",\"thread_id\":\"{other_sid}\"}}\n"
            ),
            "",
        ))
        .unwrap_err();
        assert_eq!(error.code, "native_session_ambiguous");
        assert!(error.outcome_unknown);

        for malformed in [
            "not-json\n",
            "Running profile jsonl without changing active profile\n",
            "[]\n",
            "{}\n",
            "{\"type\":\"thread.started\"}\n",
            "{\"type\":\"thread.started\",\"thread_id\":\"not-a-uuid\"}\n",
        ] {
            let error = captured_native_session_id(&output(0, malformed, ""))
                .expect_err("malformed structured output must remain fail-closed");
            assert_eq!(error.code, "native_bootstrap_output_malformed");
            assert!(error.outcome_unknown);
        }

        for output in [
            output(
                1,
                "",
                "profile auth failed with UUID 018e0000-0000-7000-8000-0000b0009ae9\n",
            ),
            output(0, "", ""),
        ] {
            let missing = captured_native_session_id(&output)
                .expect_err("native output without a SID remains ambiguous");
            assert_eq!(missing.code, "native_session_unknown");
            assert!(missing.outcome_unknown);
            assert!(missing.known_native_session_id.is_none());
        }

        let diagnostic =
            captured_native_session_id(&output(1, "", "provider startup failed\nsecond line\n"))
                .expect_err("failed bootstrap without a SID must retain a safe diagnostic");
        assert!(diagnostic
            .detail
            .contains("diagnostic: stderr: provider startup failed\nsecond line"));

        let redacted =
            captured_native_session_id(&output(1, "", "Authorization: Bearer secret-value\n"))
                .expect_err("credential-bearing diagnostics remain fail-closed");
        assert!(redacted
            .detail
            .contains("diagnostic: stderr: [redacted sensitive output]"));
        assert!(!redacted.detail.contains("secret-value"));

        let both_streams = captured_native_session_id(&output(
            1,
            "{\"type\":\"error\",\"message\":\"provider rejected request\"}\n",
            "wrapper line 1\nwrapper line 2\nwrapper line 3\nwrapper line 4\nwrapper line 5\nwrapper line 6\nactionable stderr tail\n",
        ))
        .expect_err("both diagnostic streams remain available");
        assert!(both_streams.detail.contains("stdout: {\"type\":\"error\""));
        assert!(both_streams.detail.contains("actionable stderr tail"));
        assert!(!both_streams.detail.contains("wrapper line 1"));

        let non_utf8 = Output {
            status: output(0, "", "").status,
            stdout: vec![0xff, b'\n'],
            stderr: Vec::new(),
        };
        let malformed = captured_native_session_id(&non_utf8)
            .expect_err("non-UTF-8 protocol output must remain fail-closed");
        assert_eq!(malformed.code, "native_bootstrap_output_malformed");
        assert!(malformed.detail.contains("not valid UTF-8"));
        assert!(malformed.outcome_unknown);
    }

    #[cfg(unix)]
    #[test]
    fn unsupported_top_level_flag_fails_clearly_without_fallback() {
        let error = captured_native_session_id(&output(
            2,
            "",
            "error: unexpected argument '--cutex-top-level-session' found\n",
        ))
        .unwrap_err();

        assert_eq!(error.code, "native_bootstrap_incompatible");
        assert!(!error.outcome_unknown);
        assert!(error.detail.contains("upgrade cute-codex"));
    }

    #[test]
    fn agent_message_source_binding_never_claims_a_retired_runtime() {
        let director = CutexSessionId::new("cutex.director-r10").unwrap();
        assert_eq!(
            agent_message_source_binding(&director, Some("runtime-r10".to_string())),
            (
                Some("runtime-r10".to_string()),
                Some("cutex.director-r10".to_string())
            )
        );
        assert_eq!(agent_message_source_binding(&director, None), (None, None));
    }
}
