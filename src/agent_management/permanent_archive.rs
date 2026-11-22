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

// r290 P10 (A1): authoritative read-only status of a permanent retirement by action id.
/// A request runs only with a live review token: unconsumed and unexpired, or
/// consumed by exactly that request. Late copies are possible until `expires`
/// plus this margin (the expiry is checked before the consuming write).
pub const RETIREMENT_LATE_MARGIN_SECS: i64 = 60;

#[derive(Clone, Debug, PartialEq)]
pub enum PermanentRetirementActionStatus {
    /// Receipt found (either durable store); the caller maps its stage.
    Receipt(AgentArchiveReceipt),
    /// No receipt, but an unconsumed review token for this agent is still
    /// usable by a late copy until `until_unix`. Ask again after it.
    PossiblyPending { until_unix: i64 },
    /// No receipt, and a consumed token for this agent has no receipt either:
    /// some retirement passed its checks and its result is unknown.
    Unknown,
    /// No receipt and no token that could still run it: it never ran and cannot.
    None,
}

pub fn permanent_retirement_action_status(
    state: &AgentManagementSnapshot, session_receipt: Option<&AgentArchiveReceipt>,
    action_id: &AgentActionId, target: &CutexSessionId, now_unix: i64,
) -> Result<PermanentRetirementActionStatus, AgentManagementError> {
    let receipt = state.agent_archive_actions.get(action_id).cloned().or_else(|| session_receipt.cloned());
    if let Some(receipt) = receipt {
        if receipt.request.review.operation != AgentArchiveOperation::PermanentRetire
            || &receipt.request.review.cutex_session_id != target {
            return Err(conflict("archive_action_target_conflict"));
        }
        return Ok(PermanentRetirementActionStatus::Receipt(receipt));
    }
    let entries = state.extra.get(CONFIRMATIONS).and_then(serde_json::Value::as_object);
    let mut pending_until = None;
    let mut unknown = false;
    for value in entries.into_iter().flat_map(|m| m.values()) {
        let Ok(confirmation) = serde_json::from_value::<Confirmation>(value.clone()) else { unknown = true; continue };
        if &confirmation.target != target { continue; }
        match &confirmation.consumed_by {
            None if confirmation.expires + RETIREMENT_LATE_MARGIN_SECS > now_unix =>
                pending_until = pending_until.max(Some(confirmation.expires + RETIREMENT_LATE_MARGIN_SECS)),
            None => {}
            Some(digest) => {
                let reason = format!("owner-confirmation:{digest}");
                let has_receipt = state.agent_archive_actions.values().chain(state.agent_archive_audit.values())
                    .any(|r| r.request.reason.as_deref() == Some(reason.as_str()));
                if !has_receipt { unknown = true; }
            }
        }
    }
    Ok(if unknown { PermanentRetirementActionStatus::Unknown }
        else if let Some(until_unix) = pending_until { PermanentRetirementActionStatus::PossiblyPending { until_unix } }
        else { PermanentRetirementActionStatus::None })
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

#[cfg(test)]
mod r290_action_status_tests { // r290 P10 (A1)
    use super::*;
    fn receipt(action: &str, target: &str, operation: &str, stage: &str, reason: Option<&str>) -> AgentArchiveReceipt {
        serde_json::from_value(serde_json::json!({
            "request":{"action_id":action,"reason":reason,"review":{"cutex_session_id":target,"formal_name":"Formal","operation":operation,"revision":1,"runtime_generation":1}},
            "request_sha256":"a".repeat(64),"stage":stage,"result":null,"error":null,"committed_at":"2026-11-17T00:00:00Z"
        })).unwrap()
    }
    fn confirmation(target: &str, expires: i64, consumed_by: Option<&str>) -> serde_json::Value {
        serde_json::json!({"target":target,"name":"Formal","revision":1,"generation":1,"thread":null,"expires":expires,"consumed_by":consumed_by})
    }
    #[test]
    fn retirement_status_is_receipt_pending_unknown_or_none() {
        let target = CutexSessionId::new("cutex.retire-a").unwrap();
        let action = AgentActionId::new("retire-1").unwrap();
        let now = 1_000_000;
        let mut state = AgentManagementSnapshot::empty();
        let status = |state: &AgentManagementSnapshot, session: Option<&AgentArchiveReceipt>|
            permanent_retirement_action_status(state, session, &action, &target, now);
        // Nothing anywhere: it never ran and cannot.
        assert_eq!(status(&state, None).unwrap(), PermanentRetirementActionStatus::None);
        // Another agent's live token does not keep this one pending.
        state.extra.insert(CONFIRMATIONS.into(), serde_json::json!({"k-other": confirmation("cutex.other", now + 200, None)}));
        assert_eq!(status(&state, None).unwrap(), PermanentRetirementActionStatus::None);
        // A live (or just expired, within the margin) token for this agent: a late copy could still run.
        state.extra.insert(CONFIRMATIONS.into(), serde_json::json!({"k-live": confirmation("cutex.retire-a", now - 10, None)}));
        assert_eq!(status(&state, None).unwrap(), PermanentRetirementActionStatus::PossiblyPending { until_unix: now - 10 + RETIREMENT_LATE_MARGIN_SECS });
        state.extra.insert(CONFIRMATIONS.into(), serde_json::json!({"k-old": confirmation("cutex.retire-a", now - RETIREMENT_LATE_MARGIN_SECS, None)}));
        assert_eq!(status(&state, None).unwrap(), PermanentRetirementActionStatus::None);
        // A consumed token without any receipt: some retirement passed its checks; outcome unknown.
        state.extra.insert(CONFIRMATIONS.into(), serde_json::json!({"k-used": confirmation("cutex.retire-a", now - 1000, Some("d1"))}));
        assert_eq!(status(&state, None).unwrap(), PermanentRetirementActionStatus::Unknown);
        // ... unless that consumer's receipt exists (another action): then this one is none.
        state.agent_archive_audit.insert("other/committed".into(), receipt("retire-0", "cutex.retire-a", "permanent_retire", "committed", Some("owner-confirmation:d1")));
        assert_eq!(status(&state, None).unwrap(), PermanentRetirementActionStatus::None);
        // The receipt for this action wins (either store) and must name this agent and operation.
        let committed = receipt("retire-1", "cutex.retire-a", "permanent_retire", "committed", Some("owner-confirmation:d2"));
        assert_eq!(status(&state, Some(&committed)).unwrap(), PermanentRetirementActionStatus::Receipt(committed.clone()));
        state.agent_archive_actions.insert(action.clone(), committed.clone());
        assert_eq!(status(&state, None).unwrap(), PermanentRetirementActionStatus::Receipt(committed));
        let other_target = CutexSessionId::new("cutex.other").unwrap();
        assert!(permanent_retirement_action_status(&state, None, &action, &other_target, now).is_err());
        state.agent_archive_actions.insert(action.clone(), receipt("retire-1", "cutex.retire-a", "archive", "committed", None));
        assert!(status(&state, None).is_err());
    }
}
