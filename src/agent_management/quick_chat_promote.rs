//! Upgrade an offline owned chat in place; journal the cross-store import.
use super::*;
use crate::agent_names::{NameError, NameScope, RenameRequest};
use crate::management::control_plane::*;
use crate::role_revision::CutexSessionId;
use crate::session::{model::CutexSessionRecord, store::*};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const QUICK_CHAT_PROMOTE_FEATURE: &str = "quick-chat-promote-v1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuickChatPromoteRequest {
    pub request_id: String,
    pub expected_revision: u64,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
}

impl QuickChatPromoteRequest {
    pub fn validate(&self) -> Result<(), NameError> {
        RenameRequest {
            request_id: self.request_id.clone(),
            expected_revision: self.expected_revision,
            name: self.name.clone(),
        }
        .validate_typed()?;
        if self.expected_revision >= crate::role_revision::MAX_JSON_SAFE_INTEGER {
            return Err(NameError::new(
                "invalid_request",
                "expectedRevision must allow a JSON-safe next revision",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuickChatPromoteResult {
    pub request_id: String,
    pub cutex_session_id: String,
    pub name: String,
    pub project_id: Option<ProjectId>,
    pub revision: u64,
    pub complete: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuickChatPromoteReceipt {
    pub request: QuickChatPromoteRequest,
    /// The exact promoted specification and import fence; never refreshed on retry.
    pub record: CutexSessionRecord,
    pub import_request: DurableImportRequest,
    pub result: QuickChatPromoteResult,
}

fn fail(code: &'static str, message: &str) -> anyhow::Error {
    NameError::new(code, message).into()
}

#[cfg(test)]
thread_local! { static INTERRUPT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }

impl AgentManagementProvider {
    pub fn promote_quick_chat(
        &self,
        principal: &HumanManagementPrincipal,
        path: &Path,
        session: &str,
        request: &QuickChatPromoteRequest,
    ) -> anyhow::Result<QuickChatPromoteResult> {
        request.validate()?;
        let id = CutexSessionId::new(session.to_owned())
            .map_err(|_| fail("invalid_request", "a full cutex session ID is required"))?;
        // The receipt key scopes requestId to the exact target. Internal action IDs
        // use its digest, so even a 256-character requestId stays within limits.
        let key = super::store::request_sha256(&(session, &request.request_id))?
            .as_str()
            .to_owned();
        let _reference = crate::runtime::reference_guard::acquire_shared()?;
        let lane = crate::management::v2::session_coordination::session_lane(session)?;
        // Use the established fence-first protocol: never wait for authority
        // while holding a lane. Keep these guards through roster/project import.
        let _locks = super::with_fence_first(Some(session), None, || {
            self.entry_locks(Some(&lane), true, "promote quick chat")
        })
        .map_err(|error| match error {
            AgentManagementError::HostBusy(_) => {
                fail("admission_busy", "session is busy; retry the same request")
            }
            _ => fail("persistence_unavailable", &error.to_string()),
        })?;
        let mut receipt = {
            let state = self.store().snapshot()?;
            let scope = NameScope::for_project(request.project_id.as_ref().map(ProjectId::as_str));
            let _scope = crate::agent_names::mutation_scope::enter(scope.clone());
            with_locked_session_rows(path, |rows| {
                if let Some(prior) = rows.receipt::<QuickChatPromotionReceipts>(&key) {
                    if prior.request != *request || prior.record.cutex_session_id != session {
                        return Err(fail(
                            "request_id_payload_conflict",
                            "requestId already has a different promotion payload",
                        ));
                    }
                    return Ok(prior.clone());
                }
                let source = rows
                    .session(session)
                    .ok_or_else(|| fail("session_not_found", "session does not exist"))?;
                if source.revision != request.expected_revision {
                    return Err(fail(
                        "revision_conflict",
                        "session changed; reload before promoting",
                    ));
                }
                if !source.is_active()
                    || source.is_retired()
                    || source.retired_at.is_some()
                    || source.agent_enabled
                    || source.registration_class
                        != crate::agent_bus::model::AgentRegistrationClass::LocalOnly
                    || source.formal_agent_name.is_some()
                    || state.agents.contains_key(&id)
                    || source.quick_action
                        == crate::session::model::CutexSessionQuickActionMode::Hidden
                {
                    return Err(fail(
                        "not_quick_chat",
                        "target must be an active, visible quick chat",
                    ));
                }
                if source.app_server_runtime.is_some()
                    || source.runtime_pid.is_some()
                    || source.current_runtime_agent_id.is_some()
                    || source.app_server_launch_claim_id.is_some()
                {
                    return Err(fail(
                        "quick_chat_running",
                        "stop the quick chat before promoting it",
                    ));
                }
                if let Some(project) = &request.project_id {
                    if state.project_tombstones.contains_key(project)
                        || !(state.projects.contains_key(project)
                            || state.project_states.contains_key(project))
                    {
                        return Err(fail("project_not_found", "target project does not exist"));
                    }
                    if state
                        .project_states
                        .get(project)
                        .is_some_and(|p| p.lifecycle != ProjectLifecycle::Active)
                    {
                        return Err(fail("project_not_active", "target project must be active"));
                    }
                }
                // Current names live on session rows; old names live in the scoped ledger.
                assert_formal_name_available_at(path, &scope, &request.name, Some(session))?;
                for (other_id, other) in rows.sessions() {
                    let other_project = CutexSessionId::new(other_id.clone()).ok().and_then(|id| {
                        state
                            .agents
                            .get(&id)
                            .and_then(|a| super::projects::current_project_id(&state, a))
                    });
                    if other_id != session
                        && other.is_active()
                        && !other.is_retired()
                        && other_project == request.project_id
                        && other.formal_agent_name.as_deref().is_some_and(|n| {
                            crate::agent_names::normalized_name(n)
                                == crate::agent_names::normalized_name(&request.name)
                        })
                    {
                        return Err(fail(
                            "agent_name_conflict",
                            "formal name is already in use in the target scope",
                        ));
                    }
                    if other_id != session
                        && other.app_server_thread_id() == source.app_server_thread_id()
                    {
                        return Err(fail(
                            "ambiguous_native_identity",
                            "another session uses the same native identity",
                        ));
                    }
                }
                if rows
                    .committed()
                    .quick_chat_promotion_receipts
                    .values()
                    .any(|r| !r.result.complete && r.record.cutex_session_id == session)
                {
                    return Err(fail(
                        "promotion_in_progress",
                        "retry the original promotion request",
                    ));
                }
                let mut record = source.clone();
                record.agent_enabled = true;
                record.registration_class =
                    crate::agent_bus::model::AgentRegistrationClass::Persistent;
                record.formal_agent_name = Some(request.name.clone());
                record.agent_groups = crate::agent_bus::groups::normalize_registered_agent_groups(
                    Vec::new(),
                    None,
                    crate::session::service::cutex_session_launch_cwd(&record),
                );
                record.bump_durable_revision()?;
                record.updated_at = chrono::Utc::now().to_rfc3339();
                let candidate = super::durable_import::candidate(&state, session, &record)?;
                if let Some(reason) = &candidate.rejection {
                    return Err(NameError::new("unsupported_quick_chat", reason.clone()).into());
                }
                let import_request = DurableImportRequest {
                    action_id: AgentActionId::new(format!("quick-chat-promote-{key}"))?,
                    candidate,
                    confirmed_formal_name: request.name.clone(),
                    detach: None,
                    assignment: request.project_id.clone().map(|project_id| {
                        HumanManagementProjectMutationRequest {
                            schema: HumanManagementProjectMutationSchema::V1,
                            action_id: AgentActionId::new(format!("quick-chat-project-{key}"))
                                .expect("bounded digest"),
                            project_id,
                            expected_authority_epoch: None,
                            expected_project_revision: None,
                            operation: HumanManagementProjectMutationKind::AddMember {
                                cutex_session_id: id.clone(),
                            },
                        }
                    }),
                };
                let result = QuickChatPromoteResult {
                    request_id: request.request_id.clone(),
                    cutex_session_id: session.into(),
                    name: request.name.clone(),
                    project_id: request.project_id.clone(),
                    revision: record.revision,
                    complete: false,
                    error: None,
                };
                let receipt = QuickChatPromoteReceipt {
                    request: request.clone(),
                    record: record.clone(),
                    import_request,
                    result,
                };
                // One SQLite transaction: no promoted row can exist without its
                // original recovery/import fence, and no new native thread is made.
                rows.insert_receipt::<QuickChatPromotionReceipts>(&key, receipt.clone());
                rows.insert_session(session, record);
                rows.commit()?;
                Ok(receipt)
            })?
        };
        if receipt.result.complete {
            return Ok(receipt.result);
        }
        #[cfg(test)]
        if INTERRUPT.with(|flag| flag.replace(false)) {
            return Err(fail(
                "simulated_interruption",
                "after session commit, before roster import",
            ));
        }
        // Both fences remain held. The shared import core journals and replays
        // the exact roster and project operations without acquiring them again.
        let outcome = self.import_durable_agent_locked(principal, path, &receipt.import_request);
        match outcome {
            Ok(imported) => {
                receipt.result.complete = imported.complete;
                receipt.result.error = imported.error;
            }
            Err(error) => receipt.result.error = Some(error.to_string()),
        }
        with_locked_session_rows(path, |rows| {
            rows.insert_receipt::<QuickChatPromotionReceipts>(&key, receipt.clone());
            rows.commit()?;
            Ok(())
        })?;
        Ok(receipt.result)
    }

    /// Called before Management serves requests. Failed imports retain their
    /// original fences and are visible to the caller on exact request replay.
    pub fn recover_quick_chat_promotions(&self, path: &Path) -> anyhow::Result<Vec<String>> {
        let pending = pending_quick_chat_promotions_at(path)?;
        let mut failures = Vec::new();
        for receipt in pending {
            match self.promote_quick_chat(
                &HumanManagementPrincipal::authenticated(),
                path,
                &receipt.record.cutex_session_id,
                &receipt.request,
            ) {
                Ok(result) if result.complete => {}
                Ok(result) => failures.push(format!(
                    "{}: {}",
                    result.cutex_session_id,
                    result.error.as_deref().unwrap_or("promotion incomplete")
                )),
                Err(error) => {
                    failures.push(format!("{}: {error:#}", receipt.record.cutex_session_id))
                }
            }
        }
        Ok(failures)
    }
}

#[cfg(test)]
#[path = "quick_chat_promote_tests.rs"]
mod tests;
