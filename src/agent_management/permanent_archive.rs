use super::*;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PermanentAgentRetirementRequest {
    pub action_id: AgentActionId,
    pub cutex_session_id: CutexSessionId,
    pub confirmed_name: String,
    pub confirmation_token: String,
    pub expected_revision: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PermanentAgentRetirementReviewRequest {
    pub cutex_session_id: CutexSessionId,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PermanentAgentRetirementReview {
    pub confirmation_token: String,
    pub cutex_session_id: CutexSessionId,
    pub formal_name: String,
    pub revision: u64,
    pub runtime_generation: u64,
    pub expires_at_unix: i64,
}

const CONFIRMATIONS: &str = "permanent_retirement_confirmations_v1";

#[derive(Clone, Deserialize, Serialize)]
struct Confirmation {
    target: CutexSessionId,
    name: String,
    revision: u64,
    generation: u64,
    thread: Option<String>,
    expires: i64,
    consumed_by: Option<String>,
}

impl AgentManagementProvider {
    pub fn review_permanent_agent_retirement(
        &self, _principal: &HumanManagementPrincipal, path: &Path,
        request: &PermanentAgentRetirementReviewRequest,
    ) -> Result<PermanentAgentRetirementReview, AgentManagementError> {
        let _execution = super::super::provider::provider_execution_lock().lock()
            .map_err(|_| AgentManagementError::PersistenceUnavailable)?;
        let _mutation = self.store().lock_mutations()?;
        let state = self.store().snapshot()?;
        if policy::is_permanently_retired(&state, &request.cutex_session_id) {
            return Err(conflict("permanent_retirement_is_not_restorable"));
        }
        if let Some(error) = archive_blockers_in(&state, &request.cutex_session_id).conflict() {
            return Err(error);
        }
        let sessions = crate::session::store::load_cutex_session_store_from_path(path)
            .map_err(|_| AgentManagementError::PersistenceUnavailable)?;
        let record = sessions.sessions.get(request.cutex_session_id.as_str())
            .ok_or(AgentManagementError::InvalidRequest("durable_agent_missing"))?;
        if !record.agent_enabled || record.registration_class != crate::agent_bus::model::AgentRegistrationClass::Persistent {
            return Err(AgentManagementError::InvalidRequest("requires_persistent_agent"));
        }
        let name = record.formal_agent_name.clone().filter(|n| !n.is_empty())
            .ok_or(conflict("permanent_retirement_name_unavailable"))?;
        let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        let key = super::super::store::request_sha256(&token)?.as_str().to_owned();
        let now = chrono::Utc::now().timestamp();
        let confirmation = Confirmation { target: request.cutex_session_id.clone(), name: name.clone(),
            revision: record.durable_revision(), generation: record.runtime_generation,
            thread: record.app_server_thread_id().map(str::to_owned), expires: now + 300, consumed_by: None };
        self.store().with_state(true, |mut state| {
            let entries = state.extra.entry(CONFIRMATIONS.into()).or_insert_with(|| serde_json::json!({}))
                .as_object_mut().ok_or(AgentManagementError::InvalidStore)?;
            entries.retain(|_, value| value["consumed_by"].is_string() || value["expires"].as_i64().is_some_and(|n| n > now));
            if entries.len() >= 4096 { return Err(conflict("archive_confirmation_capacity")); }
            entries.insert(key, serde_json::to_value(confirmation).map_err(|_| AgentManagementError::InvalidStore)?);
            Ok((state, (), true))
        })?;
        Ok(PermanentAgentRetirementReview { confirmation_token: token, cutex_session_id: request.cutex_session_id.clone(),
            formal_name: name, revision: record.durable_revision(), runtime_generation: record.runtime_generation,
            expires_at_unix: now + 300 })
    }

    /// Only an authenticated HumanManagementPrincipal may enter this path.
    /// Name content is checked under the session lock; input method is irrelevant.
    pub fn permanently_retire_agent(
        &self,
        _principal: &HumanManagementPrincipal,
        path: &Path,
        request: &PermanentAgentRetirementRequest,
        runtime: &mut dyn AgentArchiveRuntime,
    ) -> Result<AgentArchiveReceipt, AgentManagementError> {
        let request_digest = super::super::store::request_sha256(request)?.as_str().to_owned();
        // The original review occurrence survives every unfinished retry.
        let token_key = super::super::store::request_sha256(&request.confirmation_token)?.as_str().to_owned();
        let confirmation: Confirmation = self.store().snapshot()?.extra.get(CONFIRMATIONS)
            .and_then(|v| v.get(&token_key)).cloned().ok_or(conflict("archive_confirmation_invalid"))
            .and_then(|v| serde_json::from_value(v).map_err(|_| AgentManagementError::InvalidStore))?;
        let occurrence = ReviewedArchiveOccurrence { generation: confirmation.generation, thread: confirmation.thread.clone() };
        let archive_request = AgentArchiveRequest {
            action_id: request.action_id.clone(),
            reason: Some(format!("owner-confirmation:{request_digest}")),
            review: AgentArchiveReview {
                cutex_session_id: request.cutex_session_id.clone(),
                formal_name: request.confirmed_name.clone(),
                operation: AgentArchiveOperation::PermanentRetire,
                current_project_id: None,
                revision: request.expected_revision,
                runtime_generation: confirmation.generation,
            },
        };
        let receipt =
            self.execute_agent_archive_guarded(path, &archive_request, runtime, Some(&occurrence), |state, _, _| {
                if let Some(old) = state.agent_archive_actions.get(&request.action_id) {
                    if old.request.reason != archive_request.reason {
                        return Err(conflict("archive_action_target_conflict"));
                    }
                    if matches!(old.stage, AgentArchiveStage::Committed | AgentArchiveStage::StoppedNotArchived) {
                        return Ok(()); // completed outcome only: no runtime side effect
                    }
                    // Prepared/Uncertain/permanent partial commit must still validate occurrence.

                }
                let token_key = super::super::store::request_sha256(&request.confirmation_token)?.as_str().to_owned();
                let sessions = crate::session::store::load_cutex_session_store_from_path(path)
                    .map_err(|_| AgentManagementError::PersistenceUnavailable)?;
                let record = sessions.sessions.get(request.cutex_session_id.as_str())
                    .ok_or(conflict("durable_agent_missing"))?;
                self.store().with_state(true, |mut current| {
                    let value = current.extra.get_mut(CONFIRMATIONS).and_then(|v| v.get_mut(&token_key))
                        .ok_or(conflict("archive_confirmation_invalid"))?;
                    let mut confirmation: Confirmation = serde_json::from_value(value.clone())
                        .map_err(|_| AgentManagementError::InvalidStore)?;
                    if confirmation.target != request.cutex_session_id || confirmation.revision != request.expected_revision {
                        return Err(conflict("archive_confirmation_target_revision_mismatch"));
                    }
                    policy::validate_permanent_retirement_name(&confirmation.name, &request.confirmed_name)?;
                    if record.runtime_generation != confirmation.generation
                        || record.app_server_thread_id() != confirmation.thread.as_deref() {
                        return Err(conflict("archive_confirmation_occurrence_changed"));
                    }
                    if let Some(consumed) = &confirmation.consumed_by {
                        if consumed != &request_digest { return Err(conflict("archive_confirmation_already_used")); }
                    } else {
                        if confirmation.expires <= chrono::Utc::now().timestamp() { return Err(conflict("archive_confirmation_expired")); }
                        if record.durable_revision() != confirmation.revision || record.runtime_generation != confirmation.generation
                            || record.formal_agent_name.as_deref() != Some(confirmation.name.as_str())
                            || record.app_server_thread_id() != confirmation.thread.as_deref() {
                            return Err(conflict("archive_confirmation_stale"));
                        }
                        confirmation.consumed_by = Some(request_digest.clone());
                        *value = serde_json::to_value(confirmation).map_err(|_| AgentManagementError::InvalidStore)?;
                    }
                    Ok((current, (), true))
                })?;
                Ok(())
            })?;
        if receipt.stage == AgentArchiveStage::Committed {
            settle_archive_if_current(path, &receipt);
        }
        Ok(receipt)
    }
}
