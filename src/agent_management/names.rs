//! Owner-only formal-name mutation. The provider supplies the scope/Director
//! and holds the ordinary authority -> session lane -> session store fences.
use super::*;
use crate::agent_names::{NameError, NameScope, RenameCommit, RenameRequest};
use crate::management::control_plane::HumanManagementPrincipal;

impl AgentManagementProvider {
    /// Called inside the host authority fence before a membership mutation.
    /// The session row is the current-name authority; roster spec is history.
    pub(super) fn require_name_scope_available(
        &self,
        session: &crate::role_revision::CutexSessionId,
        project: Option<&ProjectId>,
    ) -> Result<(), AgentManagementError> {
        let Some(path) = self.current_names_path.as_deref() else {
            return Ok(());
        };
        let Some((_, record)) = crate::session::store::load_session_row(path, session.as_str())
            .map_err(|_| AgentManagementError::PersistenceUnavailable)?
        else {
            return Err(AgentManagementError::NotFound("durable_agent"));
        };
        let Some(name) = record.formal_agent_name.as_deref() else {
            return Ok(());
        };
        let scope = NameScope::for_project(project.map(|project| project.as_str()));
        crate::session::store::assert_formal_name_available_at(
            path,
            &scope,
            name,
            Some(session.as_str()),
        )
        .map_err(|error| {
            if error.downcast_ref::<NameError>().is_some() {
                AgentManagementError::Conflict("agent_name_reserved_in_target_scope")
            } else {
                AgentManagementError::PersistenceUnavailable
            }
        })
    }

    pub fn rename_formal_name_for_management(
        &self,
        _principal: &HumanManagementPrincipal,
        session: &str,
        request: &RenameRequest,
    ) -> anyhow::Result<RenameCommit> {
        request.validate_typed()?;
        let id = crate::role_revision::CutexSessionId::new(session.to_owned())
            .map_err(|_| NameError::new("invalid_request", "a full session ID is required"))?;
        let _execution = super::provider::provider_execution_lock()
            .lock()
            .map_err(|_| {
                NameError::new(
                    "persistence_unavailable",
                    "provider execution lock is poisoned",
                )
            })?;
        let _authority = self.store().lock_mutations()?;
        let path = self
            .current_names_path
            .as_ref()
            .cloned()
            .map(Ok)
            .unwrap_or_else(crate::session::store::cutex_sessions_path)?;
        if let Some(replay) = crate::session::store::formal_name_replay_at(&path, session, request)?
        {
            return Ok(replay);
        }
        let snapshot = self.store().directory_snapshot()?;
        if snapshot
            .agents
            .get(&id)
            .is_some_and(|agent| agent.retired_at.is_some())
            || pending_forget::contains(session)
        {
            return Err(NameError::new(
                "agent_permanently_retired",
                "a retired or deleted agent cannot be renamed",
            )
            .into());
        }
        let agent = snapshot.agents.get(&id).ok_or_else(|| {
            NameError::new("agent_not_found", "the target is not a managed agent")
        })?;
        let project = current_project_id(&snapshot, agent);
        let scope = NameScope::for_project(project.as_ref().map(|project| project.as_str()));
        let director = project
            .as_ref()
            .and_then(|project| snapshot.projects.get(project))
            .map(|authority| authority.authorized_director_session.as_str());
        let lane = crate::management::v2::session_coordination::session_lane(session)?;
        let _reservation = lane.try_lock().map_err(|_| {
            NameError::new(
                "admission_busy",
                "the session is busy; retry the same rename request",
            )
        })?;
        let (_, record) =
            crate::session::store::load_session_row(&path, session)?.ok_or_else(|| {
                NameError::new("session_not_found", "the target session no longer exists")
            })?;
        ensure_agent_not_permanently_retired_in(&snapshot, &record).map_err(|_| {
            NameError::new(
                "agent_permanently_retired",
                "a permanently retired agent cannot be renamed",
            )
        })?;
        crate::session::store::rename_formal_name_at(
            &path,
            session,
            request,
            &scope,
            director,
            "human-management",
        )
    }
}
