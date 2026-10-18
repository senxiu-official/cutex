//! Management v2 Retire/Restore transaction wiring.

use cutex::agent_bus::model::AgentBusAgent;
use cutex::management::v2::user_input::UserInputExecutionError;
use cutex::session::archive::commit_restore as commit_restore_record;
use cutex::session::archive::commit_retire as commit_retire_record;
use cutex::session::archive::record_has_runtime_claim;
use cutex::session::archive::restore_cutex_session;
use cutex::session::archive::retire_cutex_session;
use cutex::session::archive::validate_restore_preconditions;
use cutex::session::archive::validate_retire_preconditions;
use cutex::session::archive::CutexSessionArchiveError;
use cutex::session::archive::CutexSessionArchiveTransaction;
use cutex::session::archive::CutexSessionArchiveTransition;
use cutex::session::im_bridge::coding_registration_from_cutex_session_record;
use cutex::session::model::CutexSessionRecord;
use cutex::session::service::cutex_session_key_for_user_id_including_retired;
use cutex::session::service::persist_cutex_session_store_and_im_record;
use cutex::session::store::load_cutex_session_store;
use cutex::session::store::CutexSessionStoreRevisionConflict;

use super::management_context::load_app_server_runtime_status;
#[cfg(test)]
use super::management_lifecycle::filter_live_agents_for_management_identity;
use super::management_lifecycle::stop_cutex_session_runtime_for_entry;
use super::management_lifecycle::try_live_agents_for_management_entry;
use super::management_lifecycle::try_live_agents_for_management_identity;

const ARCHIVE_STORE_CAS_ATTEMPTS: usize = 3;

pub(super) fn mutate_management_v2_archive(
    cutex_session_id: &str,
    method: &str,
    params: &serde_json::Value,
) -> Result<serde_json::Value, UserInputExecutionError> {
    match method {
        "cutex/session/retire" => mutate_retire(cutex_session_id, params),
        "cutex/session/restore" => mutate_restore(cutex_session_id, params),
        _ => Err(archive_error(CutexSessionArchiveError::UnsupportedMethod {
            method: method.to_string(),
        })),
    }
}

fn mutate_retire(
    cutex_session_id: &str,
    params: &serde_json::Value,
) -> Result<serde_json::Value, UserInputExecutionError> {
    validate_retire_params(params)?;
    let expected_revision = required_u64(params, "expectedRevision")?;
    let expected_runtime_generation = required_u64(params, "expectedRuntimeGeneration")?;
    let transition = retire_cutex_session(
        &mut ManagementArchiveTransaction,
        cutex_session_id,
        expected_revision,
        expected_runtime_generation,
        chrono::Utc::now().to_rfc3339(),
    )
    .map_err(archive_error)?;
    // The transaction released the session store lock: settle the retired
    // session's delegations as target (asks store lock only).
    cutex::asks::lifecycle::settle_retired_target_default(&transition.cutex_session_id);
    Ok(archive_receipt(&transition))
}

fn mutate_restore(
    cutex_session_id: &str,
    params: &serde_json::Value,
) -> Result<serde_json::Value, UserInputExecutionError> {
    validate_restore_params(params)?;
    let expected_revision = required_u64(params, "expectedRevision")?;
    let transition = restore_cutex_session(
        &mut ManagementArchiveTransaction,
        cutex_session_id,
        expected_revision,
        chrono::Utc::now().to_rfc3339(),
    )
    .map_err(archive_error)?;
    Ok(archive_receipt(&transition))
}

fn archive_receipt(transition: &CutexSessionArchiveTransition) -> serde_json::Value {
    serde_json::json!({
        "cutexSessionId": transition.cutex_session_id,
        "revision": transition.revision,
        "lifecycle": transition.lifecycle.label(),
        "runtimeGeneration": transition.runtime_generation,
        "status": "offline",
        "retiredAt": transition.retired_at,
    })
}

struct ManagementArchiveTransaction;

impl CutexSessionArchiveTransaction for ManagementArchiveTransaction {
    fn load_session_including_retired(
        &mut self,
        cutex_session_id: &str,
    ) -> Result<CutexSessionRecord, CutexSessionArchiveError> {
        load_archive_record(cutex_session_id)
    }

    fn runtime_is_offline(
        &mut self,
        record: &CutexSessionRecord,
    ) -> Result<bool, CutexSessionArchiveError> {
        runtime_is_offline(record)
    }

    fn stop_runtime(
        &mut self,
        record: &CutexSessionRecord,
    ) -> Result<(), CutexSessionArchiveError> {
        if record.claude_session.is_some() {
            return stop_claude_runtime(record);
        }
        let entry = coding_registration_from_cutex_session_record(record).ok_or_else(|| {
            CutexSessionArchiveError::RuntimeStopFailed {
                detail: "session has runtime state but no active Codex thread identity".to_string(),
            }
        })?;
        let config = cutex::config::store::load_codez_config();
        let live_agents =
            try_live_agents_for_management_entry(&config, &entry).map_err(|error| {
                CutexSessionArchiveError::RuntimeStopFailed {
                    detail: format!(
                        "failed to inspect exact Agent Bus roster before stop: {error:#}"
                    ),
                }
            })?;
        let stop =
            stop_cutex_session_runtime_for_entry(&entry, &live_agents, false).map_err(|error| {
                CutexSessionArchiveError::RuntimeStopFailed {
                    detail: format!("{error:#}"),
                }
            })?;
        if !stop.stopped {
            return Err(CutexSessionArchiveError::RuntimeStopFailed {
                detail: stop.detail,
            });
        }
        Ok(())
    }

    fn commit_retire(
        &mut self,
        cutex_session_id: &str,
        expected_revision: u64,
        expected_runtime_generation: u64,
        retired_at: String,
    ) -> Result<CutexSessionArchiveTransition, CutexSessionArchiveError> {
        for attempt in 0..ARCHIVE_STORE_CAS_ATTEMPTS {
            let (mut store, key) = load_archive_store_and_key(cutex_session_id)?;
            let current = store
                .sessions
                .get(&key)
                .cloned()
                .ok_or_else(|| session_missing(cutex_session_id))?;
            validate_retire_commit_snapshot(
                &current,
                expected_revision,
                expected_runtime_generation,
            )?;
            let transition = commit_retire_record(
                store
                    .sessions
                    .get_mut(&key)
                    .ok_or_else(|| session_missing(cutex_session_id))?,
                expected_revision,
                expected_runtime_generation,
                true,
                retired_at.clone(),
            )?;
            match persist_cutex_session_store_and_im_record(&store, &key) {
                Ok(()) => return Ok(transition),
                Err(error) if is_store_revision_conflict(&error) => {
                    validate_latest_retire_after_store_conflict(
                        cutex_session_id,
                        expected_revision,
                        expected_runtime_generation,
                    )?;
                    if attempt + 1 == ARCHIVE_STORE_CAS_ATTEMPTS {
                        return Err(prewrite_contention(error));
                    }
                }
                Err(error) => {
                    return Err(CutexSessionArchiveError::PersistenceUncertain {
                        detail: format!("{error:#}"),
                    });
                }
            }
        }
        unreachable!("archive CAS retry loop always returns or reaches its bounded error")
    }

    fn commit_restore(
        &mut self,
        cutex_session_id: &str,
        expected_revision: u64,
        restored_at: String,
    ) -> Result<CutexSessionArchiveTransition, CutexSessionArchiveError> {
        for attempt in 0..ARCHIVE_STORE_CAS_ATTEMPTS {
            let (mut store, key) = load_archive_store_and_key(cutex_session_id)?;
            let current = store
                .sessions
                .get(&key)
                .cloned()
                .ok_or_else(|| session_missing(cutex_session_id))?;
            validate_restore_commit_snapshot(&current, expected_revision)?;
            if let Some(holder) =
                cutex::session::metadata::live_formal_name_holder(&store, &current)
            {
                return Err(CutexSessionArchiveError::AgentNameTaken {
                    name: current.formal_agent_name.clone().unwrap_or_default(),
                    holder: holder.cutex_session_id.clone(),
                });
            }
            let transition = commit_restore_record(
                store
                    .sessions
                    .get_mut(&key)
                    .ok_or_else(|| session_missing(cutex_session_id))?,
                expected_revision,
                restored_at.clone(),
            )?;
            match persist_cutex_session_store_and_im_record(&store, &key) {
                Ok(()) => return Ok(transition),
                Err(error) if is_store_revision_conflict(&error) => {
                    validate_latest_restore_after_store_conflict(
                        cutex_session_id,
                        expected_revision,
                    )?;
                    if attempt + 1 == ARCHIVE_STORE_CAS_ATTEMPTS {
                        return Err(prewrite_contention(error));
                    }
                }
                Err(error) => {
                    return Err(CutexSessionArchiveError::PersistenceUncertain {
                        detail: format!("{error:#}"),
                    });
                }
            }
        }
        unreachable!("archive CAS retry loop always returns or reaches its bounded error")
    }
}

/// Claude runtimes are owned by the adapter controller, not the Codex
/// app-server stop path: close the exact occurrence fenced by the reviewed
/// runtime generation, then drop the management connection.
fn stop_claude_runtime(record: &CutexSessionRecord) -> Result<(), CutexSessionArchiveError> {
    let (_, key) = load_archive_store_and_key(&record.cutex_session_id)?;
    let stop_failed = |error: anyhow::Error| CutexSessionArchiveError::RuntimeStopFailed {
        detail: format!("{error:#}"),
    };
    cutex::runtime::claude::close_expected(&key, Some(record.runtime_generation))
        .map_err(stop_failed)?;
    super::claude_runtime::disconnect(&key).map_err(stop_failed)?;
    Ok(())
}

fn validate_retire_commit_snapshot(
    record: &CutexSessionRecord,
    expected_revision: u64,
    expected_runtime_generation: u64,
) -> Result<(), CutexSessionArchiveError> {
    validate_retire_preconditions(record, expected_revision, expected_runtime_generation)?;
    if !runtime_is_offline(record)? {
        return Err(CutexSessionArchiveError::RuntimeStillOnline);
    }
    Ok(())
}

fn validate_restore_commit_snapshot(
    record: &CutexSessionRecord,
    expected_revision: u64,
) -> Result<(), CutexSessionArchiveError> {
    validate_restore_preconditions(record, expected_revision)?;
    if !runtime_is_offline(record)? {
        return Err(CutexSessionArchiveError::RuntimeStillOnline);
    }
    Ok(())
}

fn validate_latest_retire_after_store_conflict(
    cutex_session_id: &str,
    expected_revision: u64,
    expected_runtime_generation: u64,
) -> Result<(), CutexSessionArchiveError> {
    let current = load_archive_record(cutex_session_id)?;
    validate_retire_commit_snapshot(&current, expected_revision, expected_runtime_generation)
}

fn validate_latest_restore_after_store_conflict(
    cutex_session_id: &str,
    expected_revision: u64,
) -> Result<(), CutexSessionArchiveError> {
    let current = load_archive_record(cutex_session_id)?;
    validate_restore_commit_snapshot(&current, expected_revision)
}

fn is_store_revision_conflict(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<CutexSessionStoreRevisionConflict>()
        .is_some()
}

fn prewrite_contention(error: anyhow::Error) -> CutexSessionArchiveError {
    CutexSessionArchiveError::PersistenceUnavailable {
        detail: format!(
            "session store changed during {} bounded pre-write attempts: {error:#}",
            ARCHIVE_STORE_CAS_ATTEMPTS
        ),
    }
}

fn load_archive_record(
    cutex_session_id: &str,
) -> Result<CutexSessionRecord, CutexSessionArchiveError> {
    let (store, key) = load_archive_store_and_key(cutex_session_id)?;
    store
        .sessions
        .get(&key)
        .cloned()
        .ok_or_else(|| session_missing(cutex_session_id))
}

fn load_archive_store_and_key(
    cutex_session_id: &str,
) -> Result<(cutex::session::model::CutexSessionStore, String), CutexSessionArchiveError> {
    let store = load_cutex_session_store().map_err(|error| {
        CutexSessionArchiveError::PersistenceUnavailable {
            detail: format!("{error:#}"),
        }
    })?;
    let key = cutex_session_key_for_user_id_including_retired(&store, cutex_session_id)
        .ok_or_else(|| session_missing(cutex_session_id))?;
    Ok((store, key))
}

fn session_missing(cutex_session_id: &str) -> CutexSessionArchiveError {
    CutexSessionArchiveError::SessionNotFound {
        cutex_session_id: cutex_session_id.to_string(),
    }
}

fn runtime_is_offline(record: &CutexSessionRecord) -> Result<bool, CutexSessionArchiveError> {
    let manager_connected = load_app_server_runtime_status(&record.cutex_session_id)
        .map_err(|error| CutexSessionArchiveError::RuntimeStopFailed {
            detail: format!("failed to inspect app-server runtime: {error:#}"),
        })?
        .is_some_and(|status| status.connected);
    runtime_is_offline_from_observations(record, manager_connected, load_exact_live_agents(record))
}

/// The identity this session's runtime registers on the Agent Bus with, plus
/// the runtime agent id it last held. Codex runtimes register under their
/// native thread id; Claude runtimes under the adapter thread id
/// (`claude_session.thread_id`, runtime agent `claude.<connection>`). A record
/// with neither cannot prove its roster is empty, so Archive fails closed.
fn archive_roster_identity(
    record: &CutexSessionRecord,
) -> Result<(&str, Option<&str>), CutexSessionArchiveError> {
    let session_id = record.app_server_thread_id().ok_or_else(|| {
        CutexSessionArchiveError::RuntimeStopFailed {
            detail: "cannot inspect the exact Agent Bus roster without a native session identity"
                .to_string(),
        }
    })?;
    let last_runtime_agent_id = record
        .current_runtime_agent_id
        .as_deref()
        .or(record.last_runtime_agent_id.as_deref());
    Ok((session_id, last_runtime_agent_id))
}

fn load_exact_live_agents(
    record: &CutexSessionRecord,
) -> Result<Vec<AgentBusAgent>, CutexSessionArchiveError> {
    let (session_id, last_runtime_agent_id) = archive_roster_identity(record)?;
    let config = cutex::config::store::load_codez_config();
    try_live_agents_for_management_identity(&config, session_id, last_runtime_agent_id).map_err(
        |error| CutexSessionArchiveError::RuntimeStopFailed {
            detail: format!("failed to inspect exact Agent Bus roster: {error:#}"),
        },
    )
}

fn runtime_is_offline_from_observations(
    record: &CutexSessionRecord,
    manager_connected: bool,
    live_agents: Result<Vec<AgentBusAgent>, CutexSessionArchiveError>,
) -> Result<bool, CutexSessionArchiveError> {
    let live_agents = live_agents?;
    Ok(!record_has_runtime_claim(record) && !manager_connected && live_agents.is_empty())
}

fn validate_retire_params(params: &serde_json::Value) -> Result<(), UserInputExecutionError> {
    let object = params
        .as_object()
        .ok_or_else(|| invalid_request("params must be an object"))?;
    if !object.contains_key("expectedRevision")
        || !object.contains_key("expectedRuntimeGeneration")
        || object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "expectedRevision" | "expectedRuntimeGeneration" | "reason"
            )
        })
        || object
            .get("reason")
            .is_some_and(|reason| !reason.is_string())
    {
        return Err(invalid_request(
            "retire params require expectedRevision and expectedRuntimeGeneration; reason is optional",
        ));
    }
    Ok(())
}

fn validate_restore_params(params: &serde_json::Value) -> Result<(), UserInputExecutionError> {
    let object = params
        .as_object()
        .ok_or_else(|| invalid_request("params must be an object"))?;
    if object.len() != 1 || !object.contains_key("expectedRevision") {
        return Err(invalid_request(
            "restore params must contain exactly expectedRevision",
        ));
    }
    Ok(())
}

fn required_u64(params: &serde_json::Value, key: &str) -> Result<u64, UserInputExecutionError> {
    params
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .filter(|value| *value <= cutex::management::v2::model::MAX_SAFE_SEQUENCE)
        .ok_or_else(|| invalid_request(&format!("{key} must be a JSON-safe integer")))
}

fn session_not_found(cutex_session_id: &str) -> UserInputExecutionError {
    UserInputExecutionError {
        stage: "route".to_string(),
        code: "session_not_found".to_string(),
        message: "the durable cutex session does not exist".to_string(),
        retryable: false,
        details: serde_json::json!({ "cutexSessionId": cutex_session_id }),
        outcome_unknown: false,
    }
}

fn runtime_stop_failed(detail: impl Into<String>) -> UserInputExecutionError {
    UserInputExecutionError {
        stage: "runtime".to_string(),
        code: "runtime_stop_failed".to_string(),
        message: "the runtime was not safely stopped; the session remains active".to_string(),
        retryable: false,
        details: serde_json::json!({
            "diagnostic": detail.into(),
            "lifecycleCommitted": false,
        }),
        outcome_unknown: false,
    }
}

fn persistence_uncertain(detail: impl Into<String>) -> UserInputExecutionError {
    UserInputExecutionError {
        stage: "persistence".to_string(),
        code: "persistence_uncertain".to_string(),
        message: "the durable session outcome is uncertain; resync before retrying".to_string(),
        retryable: false,
        details: serde_json::json!({
            "diagnostic": detail.into(),
            "resyncRequired": true,
        }),
        outcome_unknown: true,
    }
}

fn persistence_error(detail: impl Into<String>) -> UserInputExecutionError {
    UserInputExecutionError {
        stage: "route".to_string(),
        code: "event_persistence_unavailable".to_string(),
        message: detail.into(),
        retryable: true,
        details: serde_json::json!({}),
        outcome_unknown: false,
    }
}

fn invalid_request(message: &str) -> UserInputExecutionError {
    UserInputExecutionError {
        stage: "route".to_string(),
        code: "invalid_request".to_string(),
        message: message.to_string(),
        retryable: false,
        details: serde_json::json!({}),
        outcome_unknown: false,
    }
}

fn archive_error(error: CutexSessionArchiveError) -> UserInputExecutionError {
    match error {
        CutexSessionArchiveError::SessionNotFound { cutex_session_id } => {
            session_not_found(&cutex_session_id)
        }
        CutexSessionArchiveError::PersistenceUnavailable { detail } => persistence_error(detail),
        CutexSessionArchiveError::StaleRevision { expected, actual } => UserInputExecutionError {
            stage: "route".to_string(),
            code: "revision_conflict".to_string(),
            message: format!(
                "durable session revision conflict: expected {expected}, current {actual}"
            ),
            retryable: true,
            details: serde_json::json!({
                "expectedRevision": expected,
                "currentRevision": actual,
                "resyncRequired": true,
            }),
            outcome_unknown: false,
        },
        CutexSessionArchiveError::StaleRuntimeFence { expected, actual } => {
            UserInputExecutionError {
                stage: "runtime".to_string(),
                code: "revision_conflict".to_string(),
                message: format!(
                    "runtime generation conflict: expected {expected}, current {actual}"
                ),
                retryable: true,
                details: serde_json::json!({
                    "expectedRuntimeGeneration": expected,
                    "currentRuntimeGeneration": actual,
                    "resyncRequired": true,
                }),
                outcome_unknown: false,
            }
        }
        CutexSessionArchiveError::AlreadyRetired => {
            archive_state_error("already_retired", "the cutex session is already retired")
        }
        CutexSessionArchiveError::AlreadyActive => {
            archive_state_error("already_active", "the cutex session is already active")
        }
        CutexSessionArchiveError::RuntimeStillOnline => {
            runtime_stop_failed("runtime remained online after safe stop confirmation")
        }
        CutexSessionArchiveError::RuntimeStopFailed { detail } => runtime_stop_failed(detail),
        CutexSessionArchiveError::PersistenceUncertain { detail } => persistence_uncertain(detail),
        CutexSessionArchiveError::AgentNameTaken { name, holder } => UserInputExecutionError {
            stage: "route".to_string(),
            code: "agent_name_taken".to_string(),
            message: format!(
                "the live Agent {holder} now holds the name '{name}'; rename or archive it before restoring this one"
            ),
            retryable: false,
            details: serde_json::json!({ "name": name, "holderCutexSessionId": holder }),
            outcome_unknown: false,
        },
        CutexSessionArchiveError::UnsupportedMethod { method } => UserInputExecutionError {
            stage: "route".to_string(),
            code: "cutex_method_denied".to_string(),
            message: format!("unsupported method: {method}"),
            retryable: false,
            details: serde_json::json!({
                "method": method,
                "registryVersion": cutex::management::v2::session::CUTEX_METHOD_REGISTRY_VERSION,
            }),
            outcome_unknown: false,
        },
    }
}

fn archive_state_error(code: &str, message: &str) -> UserInputExecutionError {
    UserInputExecutionError {
        stage: "route".to_string(),
        code: code.to_string(),
        message: message.to_string(),
        retryable: false,
        details: serde_json::json!({}),
        outcome_unknown: false,
    }
}

/// Archive runtime hooks for the single-call Archive: the caller stops the
/// runtime first (the ordinary offline path), so these only confirm that
/// nothing is left running for the agent.
pub(super) struct OfflineArchiveRuntime;

impl cutex::agent_management::AgentArchiveRuntime for OfflineArchiveRuntime {
    fn prepare(&mut self, record: &CutexSessionRecord, _restoring: bool) -> anyhow::Result<()> {
        anyhow::ensure!(
            cutex::runtime::lifecycle::cutex_session_host_is_local(
                &record.host_id,
                &cutex::platform::host::current_host_name()
            ),
            "archive must run on the agent's host ({})",
            record.host_id
        );
        Ok(())
    }

    fn stop_and_verify(&mut self, record: &CutexSessionRecord) -> anyhow::Result<()> {
        ensure_archive_runtime_offline(record)
    }

    fn verify_offline(&mut self, record: &CutexSessionRecord) -> anyhow::Result<()> {
        ensure_archive_runtime_offline(record)
    }
}

pub(super) fn archive_runtime_looks_online(record: &CutexSessionRecord) -> anyhow::Result<bool> {
    Ok(record_has_runtime_claim(record)
        || load_app_server_runtime_status(&record.cutex_session_id)?
            .is_some_and(|status| status.connected))
}

fn ensure_archive_runtime_offline(record: &CutexSessionRecord) -> anyhow::Result<()> {
    anyhow::ensure!(
        !archive_runtime_looks_online(record)?,
        "archive_runtime_still_online: the agent's runtime is still running"
    );
    if record.app_server_thread_id().is_some() {
        let live = load_exact_live_agents(record).map_err(|error| anyhow::anyhow!("{error}"))?;
        anyhow::ensure!(
            live.is_empty(),
            "archive_runtime_still_online: the agent is still registered on the agent bus"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct ClaimFreeLiveAgentTransaction {
        record: CutexSessionRecord,
        live_agents: Vec<AgentBusAgent>,
        steps: Vec<&'static str>,
    }

    impl CutexSessionArchiveTransaction for ClaimFreeLiveAgentTransaction {
        fn load_session_including_retired(
            &mut self,
            _cutex_session_id: &str,
        ) -> Result<CutexSessionRecord, CutexSessionArchiveError> {
            self.steps.push("load");
            Ok(self.record.clone())
        }

        fn runtime_is_offline(
            &mut self,
            record: &CutexSessionRecord,
        ) -> Result<bool, CutexSessionArchiveError> {
            self.steps.push("observe");
            runtime_is_offline_from_observations(record, false, Ok(self.live_agents.clone()))
        }

        fn stop_runtime(
            &mut self,
            _record: &CutexSessionRecord,
        ) -> Result<(), CutexSessionArchiveError> {
            self.steps.push("stop");
            self.live_agents.clear();
            Ok(())
        }

        fn commit_retire(
            &mut self,
            _cutex_session_id: &str,
            expected_revision: u64,
            expected_runtime_generation: u64,
            retired_at: String,
        ) -> Result<CutexSessionArchiveTransition, CutexSessionArchiveError> {
            self.steps.push("commit_retire");
            commit_retire_record(
                &mut self.record,
                expected_revision,
                expected_runtime_generation,
                true,
                retired_at,
            )
        }

        fn commit_restore(
            &mut self,
            _cutex_session_id: &str,
            _expected_revision: u64,
            _restored_at: String,
        ) -> Result<CutexSessionArchiveTransition, CutexSessionArchiveError> {
            unreachable!("claim-free live-Agent regression exercises Retire only")
        }
    }

    fn archive_record() -> CutexSessionRecord {
        CutexSessionRecord::new_at(
            "cutex.claim-free-live-agent".to_string(),
            Some("thread-claim-free-live-agent".to_string()),
            "devbox".to_string(),
            "/tmp/claim-free-live-agent".to_string(),
            None,
            "2026-08-10T00:00:00Z".to_string(),
        )
        .expect("archive record")
    }

    fn live_agent(id: &str, session_id: &str) -> AgentBusAgent {
        AgentBusAgent {
            id: id.to_string(),
            name: id.to_string(),
            base_name: None,
            thread_name: None,
            path_key: None,
            session_id: Some(session_id.to_string()),
            cutex_session_id: None,
            profile: "default".to_string(),
            cwd: "/tmp/claim-free-live-agent".to_string(),
            pid: std::process::id(),
            host_id: None,
            groups: Vec::new(),
            registration_class: cutex::agent_bus::model::AgentRegistrationClass::Persistent,
            last_seen_epoch_secs: 1,
        }
    }

    #[test]
    fn claim_free_exact_live_agent_is_stopped_before_retire_commit() {
        let record = archive_record();
        assert!(!record_has_runtime_claim(&record));
        let exact = live_agent(
            "runtime-agent-exact",
            record
                .codex_session_id
                .as_deref()
                .expect("native session id"),
        );
        let unrelated = live_agent("runtime-agent-other", "thread-other");
        let live_agents = filter_live_agents_for_management_identity(
            [exact, unrelated],
            record
                .codex_session_id
                .as_deref()
                .expect("native session id"),
            None,
        );
        assert_eq!(live_agents.len(), 1);
        let mut transaction = ClaimFreeLiveAgentTransaction {
            record,
            live_agents,
            steps: Vec::new(),
        };

        let transition = retire_cutex_session(
            &mut transaction,
            "cutex.claim-free-live-agent",
            1,
            0,
            "2026-08-10T00:01:00Z".to_string(),
        )
        .expect("retire after exact live Agent stop");

        assert_eq!(transition.lifecycle.label(), "retired");
        assert_eq!(
            transaction.steps,
            [
                "load",
                "observe",
                "stop",
                "load",
                "observe",
                "commit_retire"
            ]
        );
    }

    #[test]
    fn untrusted_live_agent_roster_cannot_prove_offline() {
        let record = archive_record();
        let error = runtime_is_offline_from_observations(
            &record,
            false,
            Err(CutexSessionArchiveError::RuntimeStopFailed {
                detail: "injected Agent Bus roster failure".to_string(),
            }),
        )
        .expect_err("roster failure must fail closed");

        assert!(matches!(
            error,
            CutexSessionArchiveError::RuntimeStopFailed { .. }
        ));
    }

    #[test]
    fn concurrent_target_revision_maps_to_known_revision_conflict() {
        let error = archive_error(CutexSessionArchiveError::StaleRevision {
            expected: 4,
            actual: 5,
        });

        assert_eq!(error.code, "revision_conflict");
        assert!(error.retryable);
        assert!(!error.outcome_unknown);
        assert_eq!(error.details["expectedRevision"], 4);
        assert_eq!(error.details["currentRevision"], 5);
        assert_eq!(error.details["resyncRequired"], true);
    }

    #[test]
    fn post_write_persistence_uncertainty_remains_outcome_unknown() {
        let error = archive_error(CutexSessionArchiveError::PersistenceUncertain {
            detail: "store replaced before IM projection failed".to_string(),
        });

        assert_eq!(error.code, "persistence_uncertain");
        assert!(!error.retryable);
        assert!(error.outcome_unknown);
        assert_eq!(error.details["resyncRequired"], true);
    }

    #[test]
    fn store_generation_conflict_is_recognized_as_prewrite_only() {
        let error: anyhow::Error = CutexSessionStoreRevisionConflict {
            expected: 2,
            actual: 3,
        }
        .into();

        assert!(is_store_revision_conflict(&error));
        let bounded = archive_error(prewrite_contention(error));
        assert_eq!(bounded.code, "event_persistence_unavailable");
        assert!(bounded.retryable);
        assert!(!bounded.outcome_unknown);
    }

    fn claude_archive_record() -> CutexSessionRecord {
        let mut record = CutexSessionRecord::new_at(
            "cutex.claude-archive".to_string(),
            None,
            "devbox".to_string(),
            "/tmp/claude-archive".to_string(),
            None,
            "2026-10-08T00:00:00Z".to_string(),
        )
        .expect("claude archive record");
        record.runtime_backend = cutex::session::model::CutexSessionRuntimeBackend::ClaudeSubscription;
        record.claude_session = Some(
            serde_json::from_value(serde_json::json!({
                "deployment": {"python": "/fixture/python", "adapter_source": "/fixture/adapter",
                    "claude_cli": "/fixture/claude", "claude_home": "/fixture/home",
                    "tui_binary": "/fixture/tui"},
                "thread_id": "claude-adapter-thread",
                "state_dir": "/fixture/state",
                "model": "sonnet",
                "native_session_id": "claude-native-session"
            }))
            .expect("claude session"),
        );
        record
    }

    #[test]
    fn codex_roster_identity_is_the_native_thread() {
        let mut record = archive_record();
        record.last_runtime_agent_id = Some("runtime-agent-last".to_string());
        let (session_id, last) = archive_roster_identity(&record).expect("codex identity");
        assert_eq!(session_id, "thread-claim-free-live-agent");
        assert_eq!(last, Some("runtime-agent-last"));
    }

    #[test]
    fn claude_roster_identity_is_the_adapter_thread() {
        let mut record = claude_archive_record();
        assert!(record.codex_session_id.is_none());
        record.last_runtime_agent_id = Some("claude.old-connection".to_string());
        let (session_id, last) = archive_roster_identity(&record).expect("claude identity");
        assert_eq!(session_id, "claude-adapter-thread");
        assert_eq!(last, Some("claude.old-connection"));
        record.current_runtime_agent_id = Some("claude.connection".to_string());
        let (_, last) = archive_roster_identity(&record).expect("claude identity");
        assert_eq!(last, Some("claude.connection"));
    }

    #[test]
    fn record_without_any_runtime_identity_still_fails_closed() {
        let mut record = archive_record();
        record.codex_session_id = None;
        let error = archive_roster_identity(&record).expect_err("no identity must fail closed");
        assert!(matches!(
            error,
            CutexSessionArchiveError::RuntimeStopFailed { .. }
        ));
    }

    #[test]
    fn claim_free_claude_bus_agent_blocks_offline_until_stopped() {
        let record = claude_archive_record();
        assert!(!record_has_runtime_claim(&record));
        let (session_id, last) = archive_roster_identity(&record).expect("claude identity");
        let live_agents = filter_live_agents_for_management_identity(
            [
                live_agent("claude.connection", "claude-adapter-thread"),
                live_agent("claude.other", "claude-other-thread"),
            ],
            session_id,
            last,
        );
        assert_eq!(live_agents.len(), 1);
        assert!(!runtime_is_offline_from_observations(&record, false, Ok(live_agents.clone()))
            .expect("observed"));
        assert!(runtime_is_offline_from_observations(&record, false, Ok(Vec::new()))
            .expect("observed"));

        let mut transaction = ClaimFreeLiveAgentTransaction {
            record,
            live_agents,
            steps: Vec::new(),
        };
        let transition = retire_cutex_session(
            &mut transaction,
            "cutex.claude-archive",
            1,
            0,
            "2026-10-08T00:01:00Z".to_string(),
        )
        .expect("retire Claude session after its bus agent stops");
        assert_eq!(transition.lifecycle.label(), "retired");
        assert_eq!(
            transaction.steps,
            ["load", "observe", "stop", "load", "observe", "commit_retire"]
        );
    }

    #[test]
    fn offline_claude_session_retires_without_stop() {
        let mut transaction = ClaimFreeLiveAgentTransaction {
            record: claude_archive_record(),
            live_agents: Vec::new(),
            steps: Vec::new(),
        };
        retire_cutex_session(
            &mut transaction,
            "cutex.claude-archive",
            1,
            0,
            "2026-10-08T00:01:00Z".to_string(),
        )
        .expect("offline Claude session retires");
        assert_eq!(
            transaction.steps,
            ["load", "observe", "load", "observe", "commit_retire"]
        );
    }
}
