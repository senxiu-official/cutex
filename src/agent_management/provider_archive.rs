//! Agent-authorized reversible lifecycle. Permanent retirement has no entry here.
use super::*;
use crate::session::model::CutexSessionRecord;
use std::path::Path;

pub(super) fn archived_caller(snapshot: &AgentManagementSnapshot, id: &CutexSessionId) -> bool {
    snapshot
        .agents
        .get(id)
        .is_some_and(|a| a.retired_at.is_some())
        || snapshot
            .agent_archive_actions
            .values()
            .filter(|r| {
                r.stage == AgentArchiveStage::Committed && r.request.review.cutex_session_id == *id
            })
            .filter_map(|r| r.result.as_ref())
            .max_by_key(|r| r.durable_revision())
            .is_some_and(|r| r.is_retired())
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedAgentArchiveRequest {
    pub action_id: AgentActionId,
    #[serde(default)]
    pub project_id: Option<ProjectId>,
    pub cutex_session_id: CutexSessionId,
    pub operation: AgentArchiveOperation,
}

struct LifecycleArchiveRuntime<'a> {
    id: &'a CutexSessionId,
    lifecycle: &'a dyn AgentLifecycle,
    fence: Option<&'a RuntimeOccurrenceFence>,
}

impl AgentArchiveRuntime for LifecycleArchiveRuntime<'_> {
    fn prepare(&mut self, record: &CutexSessionRecord, _: bool) -> anyhow::Result<()> {
        let observation = self
            .lifecycle
            .observe(self.id)
            .map_err(|e| anyhow::anyhow!(e.detail))?;
        anyhow::ensure!(
            record.cutex_session_id == self.id.as_str()
                && record.app_server_thread_id() == Some(observation.native_session_id.as_str()),
            "archive_native_identity_changed"
        );
        Ok(())
    }
    fn stop_and_verify(&mut self, record: &CutexSessionRecord) -> anyhow::Result<()> {
        match self.fence {
            Some(fence) => self.lifecycle.offline_if_occurrence(self.id, fence),
            None => self.lifecycle.offline(self.id),
        }
        .map_err(|e| anyhow::anyhow!(e.detail))?;
        self.verify_offline(record)
    }
    fn verify_offline(&mut self, _: &CutexSessionRecord) -> anyhow::Result<()> {
        let observed = self
            .lifecycle
            .observe(self.id)
            .map_err(|e| anyhow::anyhow!(e.detail))?;
        anyhow::ensure!(
            !observed.app_server_runtime
                && observed.runtime_agent_ids.is_empty()
                && observed.agent_bus_endpoint_ids.is_empty(),
            "archive_runtime_still_online"
        );
        Ok(())
    }
}

fn close_archive_action(
    request: &AgentManagementRequest,
    id: &CutexSessionId,
) -> Result<AgentActionId, AgentManagementError> {
    // Separate namespace from owner actions and from the parent lifecycle action.
    let hash = request_sha256(&("member-soft-archive", &request.action_id, id))?;
    AgentActionId::new(format!("soft-{}", hash.as_str()))
        .map_err(|_| AgentManagementError::InvalidRequest("archive_action_id"))
}

impl AgentManagementProvider {
    fn archive_store_path(
        &self,
        lifecycle: &dyn AgentLifecycle,
    ) -> Result<std::path::PathBuf, AgentManagementError> {
        self.current_names_path
            .clone()
            .or_else(|| lifecycle.archive_session_store_path())
            .ok_or(AgentManagementError::Conflict(
                "archive_session_store_unavailable",
            ))
    }

    pub(super) fn has_soft_archive_intent(
        &self,
        request: &AuthorizedAgentManagementRequest<'_>,
        id: &CutexSessionId,
    ) -> Result<bool, AgentManagementError> {
        let action = close_archive_action(request.request, id)?;
        Ok(self
            .store
            .snapshot()?
            .agent_archive_actions
            .get(&action)
            .is_some_and(|receipt| {
                receipt.request.review.cutex_session_id == *id
                    && receipt.request.review.operation == AgentArchiveOperation::Archive
            }))
    }

    pub(super) fn soft_archive_existing(
        &self,
        request: &AuthorizedAgentManagementRequest<'_>,
        id: &CutexSessionId,
        fence: Option<&RuntimeOccurrenceFence>,
        lifecycle: &dyn AgentLifecycle,
    ) -> Result<(ManagedAgentRecord, AgentRuntimeObservation), AgentManagementError> {
        let state = self.store.snapshot()?;
        let agent = state
            .agents
            .get(id)
            .cloned()
            .ok_or(AgentManagementError::Unauthorized)?;
        if current_project_id(&state, &agent).as_ref() != Some(&request.project_id) {
            return Err(AgentManagementError::Unauthorized);
        }
        if agent.retired_at.is_some() {
            return Err(AgentManagementError::Conflict(
                "permanent_retirement_is_not_restorable",
            ));
        }
        let path = self.archive_store_path(lifecycle)?;
        let archive_request = AgentArchiveRequest {
            action_id: close_archive_action(request.request, id)?,
            reason: None,
            review: AgentArchiveReview {
                cutex_session_id: id.clone(),
                formal_name: agent.spec.name.clone(),
                operation: AgentArchiveOperation::Archive,
                current_project_id: Some(request.project_id.clone()),
                revision: 0,
                runtime_generation: 0,
            },
        };
        let before = lifecycle.observe(id).map_err(lifecycle_error)?;
        self.validate_current_managed_observation(&agent, &before)?;
        let blockers = self.agent_archive_blockers(id)?;
        if !blockers.is_empty() {
            return Err(AgentManagementError::Conflict(
                "archive_requires_explicit_director_rotation",
            ));
        }
        let mut runtime = LifecycleArchiveRuntime {
            lifecycle,
            id,
            fence,
        };
        let receipt =
            self.execute_agent_archive_under_locks(&path, &archive_request, &mut runtime, None)?;
        if receipt.stage != AgentArchiveStage::Committed {
            return Err(AgentManagementError::OwnerActionRequired(
                receipt
                    .error
                    .unwrap_or_else(|| "archive_outcome_uncertain".into()),
            ));
        }
        if let Some(record) = receipt.result.as_ref() {
            lifecycle.archive_committed(record);
        }
        let mut after = lifecycle.observe(id).map_err(lifecycle_error)?;
        validate_identity_preserved(&before, &after)?;
        // The committed durable transaction is the lifecycle authority, even
        // when an adapter's observation cache still reports active.
        after.active = false;
        Ok((agent, after))
    }

    pub(super) fn settle_soft_archive_for_request(
        &self,
        request: &AgentManagementRequest,
        lifecycle: &dyn AgentLifecycle,
    ) {
        let id = match &request.operation {
            AgentOperation::Close { cutex_session_id } => cutex_session_id,
            AgentOperation::Replace {
                predecessor_cutex_session_id,
                policy,
                ..
            } if *policy != AgentReplacePolicy::KeepOld => predecessor_cutex_session_id,
            _ => return,
        };
        let Ok(action) = close_archive_action(request, id) else {
            return;
        };
        if let Some(receipt) = self
            .store
            .snapshot()
            .ok()
            .and_then(|s| s.agent_archive_actions.get(&action).cloned())
            .filter(|r| r.stage == AgentArchiveStage::Committed)
        {
            if let Ok(path) = self.archive_store_path(lifecycle) {
                super::super::archive::settle_archive_if_current(&path, &receipt);
            }
        }
    }

    pub(super) fn completed_archive_disposition(
        &self,
        request: &AgentManagementRequest,
    ) -> Result<Option<AgentArchiveKind>, AgentManagementError> {
        let id = match &request.operation {
            AgentOperation::Close { cutex_session_id } => cutex_session_id,
            AgentOperation::Replace {
                predecessor_cutex_session_id,
                policy,
                ..
            } if *policy != AgentReplacePolicy::KeepOld => predecessor_cutex_session_id,
            _ => return Ok(None),
        };
        let state = self.store.snapshot()?;
        if state.agents.get(id).is_some_and(|a| a.retired_at.is_some()) {
            return Ok(Some(AgentArchiveKind::Permanent));
        }
        Ok(state
            .agent_archive_actions
            .get(&close_archive_action(request, id)?)
            .filter(|r| r.stage == AgentArchiveStage::Committed)
            .and_then(|r| r.archive_kind))
    }

    /// Separate wire entry for managed Archive/Restore. The invocation must
    /// come from Agent Bus authentication; no request field grants authority.
    pub fn execute_managed_archive(
        &self,
        invocation: &AgentManagementInvocation,
        request: &ManagedAgentArchiveRequest,
        path: &Path,
        runtime: &mut dyn AgentArchiveRuntime,
    ) -> Result<AgentArchiveReceipt, AgentManagementError> {
        let authorization_request = AgentManagementRequest {
            schema: AgentManagementSchema::V1,
            action_id: request.action_id.clone(),
            project_id: request.project_id.clone(),
            operation: if request.operation == AgentArchiveOperation::Archive {
                AgentOperation::Close {
                    cutex_session_id: request.cutex_session_id.clone(),
                }
            } else {
                AgentOperation::Online {
                    cutex_session_id: request.cutex_session_id.clone(),
                }
            },
        };
        let key = request_sha256(&(
            "managed-archive",
            &invocation.caller_cutex_session,
            &request.action_id,
        ))?;
        let archive_request = AgentArchiveRequest {
            action_id: AgentActionId::new(format!("managed-{}", key.as_str()))
                .map_err(|_| AgentManagementError::InvalidStore)?,
            reason: Some(format!(
                "managed-request:{}",
                request_sha256(request)?.as_str()
            )),
            review: AgentArchiveReview {
                cutex_session_id: request.cutex_session_id.clone(),
                formal_name: String::new(),
                operation: request.operation,
                current_project_id: request.project_id.clone(),
                revision: 0,
                runtime_generation: 0,
            },
        };
        self.execute_authorized_agent_archive(path, &archive_request, runtime, |state, id, _| {
            let (project, _) = authorize_new_request(
                state,
                &invocation.caller_cutex_session,
                &authorization_request,
            )?;
            let agent = state
                .agents
                .get(id)
                .ok_or(AgentManagementError::Unauthorized)?;
            if current_project_id(state, agent).as_ref() != Some(&project) {
                return Err(AgentManagementError::Unauthorized);
            }
            if let Some(previous) = state.agent_archive_actions.get(&archive_request.action_id) {
                if previous.request.review.current_project_id
                    != archive_request.review.current_project_id
                {
                    return Err(AgentManagementError::Conflict(
                        "archive_action_target_conflict",
                    ));
                }
            }
            Ok(())
        })
    }
}
