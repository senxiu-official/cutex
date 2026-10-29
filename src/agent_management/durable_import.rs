//! Explicit Human import, with durable identity fences and recoverable step receipts.
use super::*;
use crate::{
    agent_bus::model::AgentRegistrationClass,
    management::control_plane::*,
    role_revision::{CutexSessionId, Sha256},
    session::{
        model::{
            CutexSessionArchiveState, CutexSessionQuickActionMode, CutexSessionRecord,
            CutexSessionRuntimeBackend, CutexSessionStore,
        },
        store::{save_locked_session_store, with_locked_session_store},
    },
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DurableAgentCandidate {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub raw_store_key: String,
    pub cutex_session_id: Option<CutexSessionId>,
    pub formal_name: Option<String>,
    pub durable_revision: u64,
    pub durable_sha256: Sha256,
    pub roster_sha256: Sha256,
    pub agent_sha256: Sha256,
    pub in_roster: bool,
    pub current_project_id: Option<ProjectId>,
    pub online: bool,
    pub rejection: Option<String>,
}

/// Stable Human-reviewed durable facts used by the import authorization fence.
/// Runtime occurrence/observation fields and native presentation metadata are
/// intentionally absent; they neither authorize import nor define Agent identity.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DurableCandidateFence<'a> {
    schema: &'static str,
    cutex_session_id: &'a str,
    revision: u64,
    archive_state: CutexSessionArchiveState,
    retired_at: &'a Option<String>,
    codex_session_id: &'a Option<String>,
    app_server_thread_id: Option<&'a str>,
    app_server_launch_claim_id: &'a Option<String>,
    formal_agent_name: &'a Option<String>,
    host_id: &'a str,
    cwd: &'a str,
    managed_cwd: &'a Option<String>,
    profile: &'a Option<String>,
    runtime_backend: CutexSessionRuntimeBackend,
    agent_enabled: bool,
    agent_groups: &'a [String],
    registration_class: AgentRegistrationClass,
    exposed_to_backend: bool,
    quick_action: CutexSessionQuickActionMode,
    default_cli_args: &'a [String],
    permission_defaults: &'a Option<String>,
    approval_policy: &'a Option<String>,
    sandbox_mode: &'a Option<String>,
    model_defaults: &'a Option<String>,
    reasoning_defaults: &'a Option<String>,
}

pub(super) fn durable_candidate_digest(
    record: &CutexSessionRecord,
) -> Result<Sha256, AgentManagementError> {
    super::store::request_sha256(&DurableCandidateFence {
        schema: "cutex.durable-import-candidate-fence.v1",
        cutex_session_id: &record.cutex_session_id,
        revision: record.revision,
        archive_state: record.archive_state,
        retired_at: &record.retired_at,
        codex_session_id: &record.codex_session_id,
        app_server_thread_id: record.app_server_thread_id(),
        app_server_launch_claim_id: &record.app_server_launch_claim_id,
        formal_agent_name: &record.formal_agent_name,
        host_id: &record.host_id,
        cwd: &record.cwd,
        managed_cwd: &record.managed_cwd,
        profile: &record.profile,
        runtime_backend: record.runtime_backend,
        agent_enabled: record.agent_enabled,
        agent_groups: &record.agent_groups,
        registration_class: record.registration_class,
        exposed_to_backend: record.exposed_to_backend,
        quick_action: record.quick_action,
        default_cli_args: &record.default_cli_args,
        permission_defaults: &record.permission_defaults,
        approval_policy: &record.approval_policy,
        sandbox_mode: &record.sandbox_mode,
        model_defaults: &record.model_defaults,
        reasoning_defaults: &record.reasoning_defaults,
    })
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DurableImportRequest {
    pub action_id: AgentActionId,
    pub candidate: DurableAgentCandidate,
    /// Human entered for historical unnamed records; never a suggested title.
    pub confirmed_formal_name: String,
    /// None means import only. Some explicitly authorizes import AND this step.
    pub assignment: Option<HumanManagementProjectMutationRequest>,
    /// Explicit source Detach, with its source authority/project CAS.
    pub detach: Option<HumanManagementProjectMutationRequest>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DurableImportReceipt {
    pub action_id: AgentActionId,
    pub request_sha256: Sha256,
    pub request: DurableImportRequest,
    pub named: bool,
    pub imported: bool,
    pub imported_agent: Option<ManagedAgentRecord>,
    /// Exact import provenance, preserving nullable configured intent explicitly.
    pub source_record: Option<CutexSessionRecord>,
    pub complete: bool,
    pub error: Option<String>,
    pub performed_by_human_management: bool,
    pub steps: BTreeMap<String, HumanManagementProjectMutationReceipt>,
    pub committed_at: crate::role_revision::Rfc3339,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DurableImportAuditEvent {
    pub action_id: AgentActionId,
    pub request_sha256: Sha256,
    pub cutex_session_id: CutexSessionId,
    pub formal_name: String,
    pub stage: String,
    pub performed_by_human_management: bool,
    pub committed_at: crate::role_revision::Rfc3339,
}

fn append_audit(state: &mut AgentManagementSnapshot, receipt: &DurableImportReceipt, stage: &str) {
    state
        .durable_import_audit
        .entry(format!("{}/{stage}", receipt.action_id))
        .or_insert_with(|| DurableImportAuditEvent {
            action_id: receipt.action_id.clone(),
            request_sha256: receipt.request_sha256.clone(),
            cutex_session_id: receipt
                .request
                .candidate
                .cutex_session_id
                .clone()
                .expect("validated import identity"),
            formal_name: receipt.request.confirmed_formal_name.clone(),
            stage: stage.into(),
            performed_by_human_management: true,
            committed_at: super::now(),
        });
}

#[cfg(test)]
thread_local! { static INTERRUPT_IMPORT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) }; }
#[cfg(test)]
fn test_interrupt(stage: &'static str) -> Result<(), AgentManagementError> {
    INTERRUPT_IMPORT.with(|point| {
        if point.get() == Some(stage) {
            point.set(None);
            Err(conflict("simulated_import_interruption"))
        } else {
            Ok(())
        }
    })
}

fn conflict(code: &'static str) -> AgentManagementError {
    AgentManagementError::Conflict(code)
}
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.trim() == name
        && name.chars().count() <= 128
        && !name.chars().any(char::is_control)
}

fn eligibility(key: &str, record: &CutexSessionRecord) -> Result<(), AgentManagementError> {
    if key != record.cutex_session_id {
        return Err(conflict("durable_identity_mismatch"));
    }
    if !record.is_active() || record.is_retired() || record.retired_at.is_some() {
        return Err(conflict("durable_agent_retired"));
    }
    if record.registration_class != AgentRegistrationClass::Persistent || !record.agent_enabled {
        return Err(conflict("requires_persistent_durable_agent"));
    }
    if record.revision == 0 || record.revision > crate::role_revision::MAX_JSON_SAFE_INTEGER {
        return Err(conflict("invalid_durable_revision"));
    }
    if record.quick_action == crate::session::model::CutexSessionQuickActionMode::Hidden {
        return Err(conflict("unsupported_hidden_agent_configuration"));
    }
    if record.agent_groups.is_empty()
        || crate::agent_bus::identity::normalize_agent_groups(record.agent_groups.clone())
            != record.agent_groups
    {
        return Err(conflict("malformed_durable_agent_groups"));
    }
    for value in [
        &record.profile,
        &record.model_defaults,
        &record.reasoning_defaults,
        &record.permission_defaults,
        &record.approval_policy,
        &record.sandbox_mode,
    ]
    .into_iter()
    .flatten()
    {
        if value.trim().is_empty() || value.chars().any(char::is_control) {
            return Err(conflict("malformed_durable_configuration"));
        }
    }
    if !crate::runtime::lifecycle::cutex_session_host_is_local(
        &record.host_id,
        &crate::platform::host::current_host_name(),
    ) {
        return Err(conflict("unsupported_remote_durable_agent"));
    }
    if !matches!(
        record.runtime_backend,
        CutexSessionRuntimeBackend::Host
            | CutexSessionRuntimeBackend::HostForeground
            | CutexSessionRuntimeBackend::CuteAlden
            | CutexSessionRuntimeBackend::ClaudeSubscription
            | CutexSessionRuntimeBackend::Dsh
            | CutexSessionRuntimeBackend::Kimi
    ) {
        return Err(conflict("unsupported_durable_backend"));
    }
    if record.runtime_backend.is_acp()
        && (record.dsh_session.is_none() || record.codex_session_id.is_some() || record.claude_session.is_some())
    {
        return Err(conflict("malformed_durable_configuration"));
    }
    if record.app_server_launch_claim_id.is_some() {
        return Err(conflict("unresolved_durable_launch_claim"));
    }
    let native = record
        .app_server_thread_id()
        .ok_or(conflict("durable_native_session_missing"))?;
    // The shared identifier normalizer only rejects empty/path-like strings.
    // Product DSH facade IDs are canonical UUIDs, separate from ACP native IDs.
    if record.runtime_backend.is_acp()
        && uuid::Uuid::parse_str(native).ok().is_none_or(|id| id.to_string() != native)
    {
        return Err(conflict("malformed_durable_native_session"));
    }
    if crate::session::identity::normalize_codex_session_id(native)
        .ok()
        .as_deref()
        != Some(native)
    {
        return Err(conflict("malformed_durable_native_session"));
    }
    let cwd = crate::session::service::cutex_session_launch_cwd(record);
    if !Path::new(cwd).is_absolute() || Path::new(cwd).parent().is_none() {
        return Err(conflict("malformed_durable_cwd"));
    }
    if record
        .formal_agent_name
        .as_deref()
        .is_some_and(|name| !valid_name(name))
    {
        return Err(conflict("malformed_formal_agent_name"));
    }
    Ok(())
}

fn roster_digest(
    state: &AgentManagementSnapshot,
    id: &CutexSessionId,
) -> Result<Sha256, AgentManagementError> {
    super::store::request_sha256(&(
        state.agents.get(id),
        state.current_project_memberships.get(id),
        state
            .operator_grants
            .iter()
            .filter_map(|(project, grants)| grants.get(id).map(|grant| (project, grant)))
            .collect::<Vec<_>>(),
    ))
}

fn candidate(
    state: &AgentManagementSnapshot,
    key: &str,
    record: &CutexSessionRecord,
) -> Result<DurableAgentCandidate, AgentManagementError> {
    let id = CutexSessionId::new(key.to_string()).ok();
    let agent = id.as_ref().and_then(|id| state.agents.get(id));
    let rejection = if id.is_none() {
        Some("malformed_durable_id".to_string())
    } else if state.agents.values().any(|other| {
        Some(&other.cutex_session_id) != id.as_ref()
            && record.app_server_thread_id() == Some(other.native_session_id.as_str())
    }) {
        Some("native_session_already_owned_by_other_durable_agent".to_string())
    } else if agent.is_some_and(|a| a.retired_at.is_some()) {
        Some("roster_agent_retired".to_string())
    } else {
        eligibility(key, record)
            .err()
            .map(|error| error.to_string())
    };
    Ok(DurableAgentCandidate {
        raw_store_key: key.to_string(),
        cutex_session_id: id.clone(),
        formal_name: record
            .formal_agent_name
            .clone()
            .or_else(|| agent.map(|a| a.spec.name.clone())),
        durable_revision: record.revision,
        durable_sha256: durable_candidate_digest(record)?,
        roster_sha256: match &id {
            Some(id) => roster_digest(state, id)?,
            None => super::store::request_sha256(&Option::<()>::None)?,
        },
        agent_sha256: super::store::request_sha256(&agent)?,
        in_roster: agent.is_some(),
        current_project_id: agent.and_then(|a| super::projects::current_project_id(state, a)),
        // The authenticated adapter joins live endpoints by exact durable ID.
        // Persisted runtime bindings alone do not prove an Online occurrence.
        online: false,
        rejection,
    })
}

impl AgentManagementProvider {
    pub fn durable_agent_candidates(
        &self,
        _principal: &HumanManagementPrincipal,
        session_path: &Path,
    ) -> Result<Vec<DurableAgentCandidate>, AgentManagementError> {
        let _execution = super::provider::provider_execution_lock()
            .lock()
            .map_err(|_| AgentManagementError::PersistenceUnavailable)?;
        let _mutation = self.store().lock_mutations()?;
        with_locked_session_store(session_path, |sessions| {
            let state = self.store().snapshot()?;
            let mut rows = sessions
                .sessions
                .iter()
                .map(|(key, record)| candidate(&state, key, record))
                .collect::<Result<Vec<_>, _>>()?;
            for row in &mut rows {
                let record = &sessions.sessions[&row.raw_store_key];
                if record.app_server_thread_id().is_some()
                    && sessions.sessions.iter().any(|(key, other)| {
                        key != &row.raw_store_key
                            && other.app_server_thread_id() == record.app_server_thread_id()
                    })
                {
                    row.rejection = Some("ambiguous_durable_native_identity".into());
                }
            }
            rows.sort_by(|a, b| a.raw_store_key.cmp(&b.raw_store_key));
            Ok(rows)
        })
        .map_err(import_error)
    }

    pub fn import_durable_agent(
        &self,
        _principal: &HumanManagementPrincipal,
        session_path: &Path,
        request: &DurableImportRequest,
    ) -> Result<DurableImportReceipt, AgentManagementError> {
        validate_request(request)?;
        let _execution = super::provider::provider_execution_lock()
            .lock()
            .map_err(|_| AgentManagementError::PersistenceUnavailable)?;
        let _mutation = self.store().lock_mutations()?;
        let digest = super::store::request_sha256(request)?;
        let state = self.store().snapshot()?;
        let mut receipt =
            if let Some(receipt) = state.durable_import_actions.get(&request.action_id) {
                if receipt.request_sha256 != digest {
                    return Err(conflict("action_id_payload_conflict"));
                }
                if receipt.complete {
                    return Ok(receipt.clone());
                }
                receipt.clone()
            } else {
                reject_action_collision(&state, &request.action_id)?;
                for step in [request.detach.as_ref(), request.assignment.as_ref()]
                    .into_iter()
                    .flatten()
                {
                    reject_action_collision(&state, &step.action_id)?;
                    if state.durable_import_actions.contains_key(&step.action_id) {
                        return Err(conflict("action_id_domain_conflict"));
                    }
                }
                DurableImportReceipt {
                    action_id: request.action_id.clone(),
                    request_sha256: digest,
                    request: request.clone(),
                    named: false,
                    imported: false,
                    imported_agent: None,
                    source_record: None,
                    complete: false,
                    error: None,
                    performed_by_human_management: true,
                    steps: BTreeMap::new(),
                    committed_at: super::now(),
                }
            };
        let outcome = with_locked_session_store(session_path, |sessions| {
            self.import_locked(session_path, sessions, request, &mut receipt)
                .map_err(anyhow::Error::new)
        });
        match outcome {
            Ok(()) => {
                receipt.complete = true;
                receipt.error = None;
            }
            Err(error) => {
                // If no journal exists no effect was admitted; do not reserve an action.
                if !self
                    .store()
                    .snapshot()?
                    .durable_import_actions
                    .contains_key(&request.action_id)
                {
                    return Err(import_error(error));
                }
                receipt.error = Some(error.to_string());
            }
        }
        self.save_import_receipt(&receipt)?;
        Ok(receipt)
    }

    fn save_import_receipt(
        &self,
        receipt: &DurableImportReceipt,
    ) -> Result<(), AgentManagementError> {
        self.store().with_state(true, |mut state| {
            append_audit(
                &mut state,
                receipt,
                if receipt.complete {
                    "complete"
                } else {
                    "confirmed"
                },
            );
            state
                .durable_import_actions
                .insert(receipt.action_id.clone(), receipt.clone());
            Ok((state, (), true))
        })
    }

    fn import_locked(
        &self,
        session_path: &Path,
        sessions: &mut CutexSessionStore,
        request: &DurableImportRequest,
        receipt: &mut DurableImportReceipt,
    ) -> Result<(), AgentManagementError> {
        let id = request
            .candidate
            .cutex_session_id
            .as_ref()
            .ok_or(conflict("malformed_durable_id"))?;
        if !request.candidate.raw_store_key.is_empty()
            && request.candidate.raw_store_key != id.as_str()
        {
            return Err(conflict("malformed_durable_id"));
        }
        let record = sessions
            .sessions
            .get(id.as_str())
            .ok_or(conflict("durable_agent_disappeared"))?;
        if sessions.sessions.iter().any(|(key, other)| {
            key != id.as_str() && other.app_server_thread_id() == record.app_server_thread_id()
        }) {
            return Err(conflict("ambiguous_durable_native_identity"));
        }
        eligibility(id.as_str(), record)?;
        let state = self.store().snapshot()?;
        let mut actual = candidate(&state, id.as_str(), record)?;
        // Preserve the original wire shape/digest for pre-repair confirmations.
        if request.candidate.raw_store_key.is_empty() {
            actual.raw_store_key.clear();
        }
        // Runtime liveness is informational, never import authorization or CAS.
        actual.online = request.candidate.online;
        if let Some(reason) = actual.rejection {
            return Err(AgentManagementError::OwnerActionRequired(reason));
        }
        let journal_exists = state
            .durable_import_actions
            .contains_key(&request.action_id);
        if !journal_exists {
            if &actual != &request.candidate {
                return Err(conflict("stale_durable_candidate"));
            }
            if actual
                .formal_name
                .as_deref()
                .is_some_and(|name| name != request.confirmed_formal_name)
            {
                return Err(conflict("formal_name_changed"));
            }
            self.save_import_receipt(receipt)?;
        } else {
            // Only our own explicit naming step may change the durable snapshot.
            let mut original = record.clone();
            if request.candidate.formal_name.is_none()
                && record.formal_agent_name.as_deref()
                    == Some(request.confirmed_formal_name.as_str())
            {
                let naming = sessions
                    .formal_name_receipts
                    .get(request.action_id.as_str())
                    .ok_or(conflict("formal_name_changed_by_other_action"))?;
                if naming.request_sha256 != receipt.request_sha256
                    || naming.cutex_session_id != id.as_str()
                    || naming.name != request.confirmed_formal_name
                    || naming.previous_revision != request.candidate.durable_revision
                    || naming.revision != record.revision
                    || naming.updated_at != record.updated_at
                {
                    return Err(conflict("formal_name_receipt_conflict"));
                }
                receipt.named = true;
                if record.revision != request.candidate.durable_revision + 1 {
                    return Err(conflict("durable_revision_changed_after_naming"));
                }
                original.formal_agent_name = None;
                original.revision = request.candidate.durable_revision;
                original.updated_at = naming.previous_updated_at.clone();
            }
            if durable_candidate_digest(&original)? != request.candidate.durable_sha256 {
                return Err(conflict("durable_candidate_changed_after_partial_import"));
            }
            self.validate_import_roster_replay(&state, request, receipt)?;
        }
        if record.formal_agent_name.is_none() && !request.candidate.in_roster {
            #[cfg(test)]
            test_interrupt("before_name")?;
            let record = sessions
                .sessions
                .get_mut(id.as_str())
                .expect("checked record");
            record.formal_agent_name = Some(request.confirmed_formal_name.clone());
            record
                .bump_durable_revision()
                .map_err(|_| conflict("durable_revision_overflow"))?;
            let previous_updated_at = record.updated_at.clone();
            record.updated_at = chrono::Utc::now().to_rfc3339();
            sessions.formal_name_receipts.insert(
                request.action_id.as_str().to_string(),
                crate::session::model::FormalAgentNameReceipt {
                    request_sha256: receipt.request_sha256.clone(),
                    cutex_session_id: id.as_str().to_string(),
                    name: request.confirmed_formal_name.clone(),
                    previous_revision: request.candidate.durable_revision,
                    revision: record.revision,
                    previous_updated_at,
                    updated_at: record.updated_at.clone(),
                },
            );
            save_locked_session_store(session_path, sessions).map_err(import_error)?;
            #[cfg(test)]
            test_interrupt("after_name")?;
            receipt.named = true;
            self.save_import_receipt(receipt)?;
        }
        let record = sessions.sessions.get(id.as_str()).expect("checked record");
        if !request.candidate.in_roster && !state.agents.contains_key(id) {
            let agent = imported_record(record, &request.confirmed_formal_name)?;
            let mut imported_receipt = receipt.clone();
            imported_receipt.imported = true;
            imported_receipt.imported_agent = Some(agent.clone());
            imported_receipt.source_record = Some(record.clone());
            self.store().with_state(true, |mut state| {
                if state.agents.contains_key(id) {
                    return Err(conflict("roster_agent_already_exists"));
                }
                state.agents.insert(id.clone(), agent);
                state.current_project_memberships.insert(
                    id.clone(),
                    CurrentProjectMembership {
                        cutex_session_id: id.clone(),
                        project_id: None,
                        revision: 1,
                        updated_at: super::now(),
                    },
                );
                state
                    .durable_import_actions
                    .insert(request.action_id.clone(), imported_receipt.clone());
                append_audit(&mut state, &imported_receipt, "roster_imported");
                Ok((state, (), true))
            })?;
            *receipt = imported_receipt;
        }
        for (step, operation) in [
            ("detach", request.detach.as_ref()),
            ("assignment", request.assignment.as_ref()),
        ] {
            if let Some(operation) = operation {
                let result = self.execute_project_mutation_locked(operation)?;
                #[cfg(test)]
                test_interrupt(step)?;
                receipt.steps.insert(step.to_string(), result);
                self.save_import_receipt(receipt)?;
            }
        }
        Ok(())
    }

    fn validate_import_roster_replay(
        &self,
        state: &AgentManagementSnapshot,
        request: &DurableImportRequest,
        receipt: &DurableImportReceipt,
    ) -> Result<(), AgentManagementError> {
        let id = request
            .candidate
            .cutex_session_id
            .as_ref()
            .ok_or(conflict("malformed_durable_id"))?;
        if !request.candidate.raw_store_key.is_empty()
            && request.candidate.raw_store_key != id.as_str()
        {
            return Err(conflict("malformed_durable_id"));
        }
        if let Some(imported) = &receipt.imported_agent {
            if state.agents.get(id) != Some(imported) {
                return Err(conflict("imported_roster_record_changed"));
            }
        } else if super::store::request_sha256(&state.agents.get(id))?
            != request.candidate.agent_sha256
        {
            return Err(conflict("roster_record_changed"));
        }
        // Project receipts survive a crash before the composite receipt advances.
        let last = request
            .assignment
            .as_ref()
            .and_then(|r| state.human_management_project_mutations.get(&r.action_id))
            .or_else(|| {
                request
                    .detach
                    .as_ref()
                    .and_then(|r| state.human_management_project_mutations.get(&r.action_id))
            });
        if let Some(last) = last {
            if last.receipt.membership.as_ref() != state.current_project_memberships.get(id) {
                return Err(conflict("membership_changed_after_partial_import"));
            }
            let agent = state
                .agents
                .get(id)
                .ok_or(conflict("roster_agent_disappeared"))?;
            if agent.retired_at.is_some() {
                return Err(conflict("roster_agent_changed_after_partial_import"));
            }
        } else if receipt.imported {
            let membership = state
                .current_project_memberships
                .get(id)
                .ok_or(conflict("import_membership_missing"))?;
            if membership.project_id.is_some() || membership.revision != 1 {
                return Err(conflict("import_membership_changed"));
            }
        } else if roster_digest(state, id)? != request.candidate.roster_sha256 {
            return Err(conflict("roster_candidate_changed"));
        }
        Ok(())
    }
}

fn validate_request(request: &DurableImportRequest) -> Result<(), AgentManagementError> {
    if !valid_name(&request.confirmed_formal_name) {
        return Err(conflict("explicit_formal_name_required"));
    }
    let id = request
        .candidate
        .cutex_session_id
        .as_ref()
        .ok_or(conflict("malformed_durable_id"))?;
    if !request.candidate.raw_store_key.is_empty() && request.candidate.raw_store_key != id.as_str()
    {
        return Err(conflict("malformed_durable_id"));
    }
    if let Some(assignment) = &request.assignment {
        let target = match &assignment.operation {
            HumanManagementProjectMutationKind::Create {
                director_cutex_session_id,
                ..
            } => director_cutex_session_id,
            HumanManagementProjectMutationKind::AddMember { cutex_session_id } => cutex_session_id,
            _ => return Err(conflict("unsupported_import_assignment")),
        };
        if target != id || assignment.action_id == request.action_id {
            return Err(conflict("import_confirmation_target_mismatch"));
        }
    }
    if let Some(detach) = &request.detach {
        if !matches!(&detach.operation, HumanManagementProjectMutationKind::DetachMember { cutex_session_id } if cutex_session_id == id)
            || request.candidate.current_project_id.as_ref() != Some(&detach.project_id)
            || detach.action_id == request.action_id
            || request.assignment.as_ref().is_none_or(|a| {
                a.action_id == detach.action_id || a.project_id == detach.project_id
            })
        {
            return Err(conflict("invalid_explicit_detach_confirmation"));
        }
    } else if request.assignment.is_some() && request.candidate.current_project_id.is_some() {
        return Err(conflict("explicit_source_detach_required"));
    }
    Ok(())
}

fn reject_action_collision(
    state: &AgentManagementSnapshot,
    id: &AgentActionId,
) -> Result<(), AgentManagementError> {
    if state.actions.contains_key(id)
        || state.authority_receipts.contains_key(id)
        || state.human_management_project_mutations.contains_key(id)
        || state.human_management_operator_actions.contains_key(id)
        || state
            .legacy_director_ownership_import_receipts
            .contains_key(id)
        || state.reservation_reconciliation_receipts.contains_key(id)
    {
        return Err(conflict("action_id_domain_conflict"));
    }
    Ok(())
}

fn imported_record(
    record: &CutexSessionRecord,
    name: &str,
) -> Result<ManagedAgentRecord, AgentManagementError> {
    let id = CutexSessionId::new(record.cutex_session_id.clone())
        .map_err(|_| conflict("malformed_durable_id"))?;
    Ok(ManagedAgentRecord {
        // Human import creates no historical Project or Director provenance.
        project_id: None,
        created_by_director_session: None,
        created_by_operator_session: None,
        cutex_session_id: id,
        native_session_id: record
            .app_server_thread_id().map(str::to_string)
            .ok_or(conflict("durable_native_session_missing"))?,
        spec: ManagedAgentSpec {
            name: name.to_string(),
            cwd: crate::session::service::cutex_session_launch_cwd(record).to_string(),
            // The durable record remains the authoritative effective-next-launch configuration.
            profile: record.profile.clone(),
            runtime_backend: serde_json::to_value(record.runtime_backend)
                .map_err(|_| AgentManagementError::InvalidStore)?
                .as_str()
                .ok_or(AgentManagementError::InvalidStore)?
                .to_string(),
            model: record.model_defaults.clone().unwrap_or_default(),
            reasoning: record.reasoning_defaults.clone().unwrap_or_default(),
            permissions: record.permission_defaults.clone().unwrap_or_default(),
            approval_policy: record.approval_policy.clone().unwrap_or_default(),
            sandbox_mode: record.sandbox_mode.clone().unwrap_or_default(),
            groups: record.agent_groups.clone(),
            expose_to_im: record.exposed_to_backend,
            pin: record.quick_action == crate::session::model::CutexSessionQuickActionMode::Pinned,
        },
        created_at: super::now(),
        retired_at: None,
    })
}

fn import_error(error: anyhow::Error) -> AgentManagementError {
    match error.downcast::<AgentManagementError>() {
        Ok(error) => error,
        Err(error) => {
            AgentManagementError::OwnerActionRequired(format!("durable import storage: {error}"))
        }
    }
}

#[cfg(test)]
#[path = "durable_import_tests.rs"]
mod tests;

#[cfg(test)]
mod dsh_tests {
    use super::*;
    #[test]
    fn dsh_unstarted_facade_identity_is_importable() {
        let facade = uuid::Uuid::new_v4().to_string();
        let mut record = CutexSessionRecord::new(
            "cutex.dsh-import".into(), None,
            crate::platform::host::current_host_name(),
            std::env::temp_dir().to_string_lossy().into_owned(), Some("DSH".into()),
        ).unwrap();
        record.runtime_backend = CutexSessionRuntimeBackend::Dsh;
        record.agent_enabled = true;
        record.registration_class = AgentRegistrationClass::Persistent;
        record.agent_groups = vec!["default".into()];
        record.dsh_session = Some(serde_json::from_value(serde_json::json!({
            "thread_id":facade, "state_dir":"/unused/state", "node":"/unused/node",
            "adapter":"/unused/adapter", "dsh_cli":"/unused/dsh", "patch":"/unused/patch",
            "install_root":"/unused/install", "native_session_id":null, "spike":false,
            "model":"deepseek-v4-flash", "api_key_env":"DEEPSEEK_API_KEY", "deployment_version":"0.2.1-rc.3"
        })).unwrap());
        assert!(record.codex_session_id.is_none());
        assert!(record.app_server_runtime.is_none());
        eligibility(&record.cutex_session_id, &record).unwrap();
        let imported = imported_record(&record, "DSH agent").unwrap();
        assert_eq!(imported.spec.runtime_backend, "dsh");
        assert_eq!(imported.native_session_id, facade);
        assert!(record.dsh_session.as_ref().unwrap().native_session_id.is_none());
        record.codex_session_id = Some(uuid::Uuid::new_v4().to_string());
        assert!(eligibility(&record.cutex_session_id, &record).is_err());
        record.codex_session_id = None;

        record.dsh_session.as_mut().unwrap().thread_id = "not-a-conversation-uuid".into();
        assert!(eligibility(&record.cutex_session_id, &record).is_err());
    }
}
