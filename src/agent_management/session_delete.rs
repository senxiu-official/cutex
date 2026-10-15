//! Management `cutex/session/delete`: which provider-owned references make a
//! permanent delete unsafe, and the roster cleanup after the session record
//! and its native thread are gone.
use super::*;
use crate::role_revision::CutexSessionId;
use serde::Serialize;

/// References that would dangle if the session were deleted. Empty means the
/// provider has nothing that still points at the session.
#[derive(Clone, Debug, Default, Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionDeleteReferences {
    /// Projects (not removed) whose Director is this session.
    pub director_projects: Vec<ProjectId>,
    /// Projects that grant this session the Agent Operator role.
    pub operator_grant_projects: Vec<ProjectId>,
    /// The (not removed) project this session is currently a member of.
    pub project_membership: Option<ProjectId>,
    /// r219: the Task Service was removed; always empty. Kept for one release
    /// so the Management v2 delete response still carries `openAssignments`.
    pub open_assignments: Vec<serde_json::Value>,
}

impl SessionDeleteReferences {
    pub fn is_empty(&self) -> bool {
        self.director_projects.is_empty()
            && self.operator_grant_projects.is_empty()
            && self.project_membership.is_none()
    }

    /// Whether a delete must be refused: Director authority or Operator grants.
    /// A plain project membership does not block; the delete drops it with the
    /// roster row (`forget_deleted_session`), e.g. an archived former member.
    pub fn blocks_delete(&self) -> bool {
        !self.director_projects.is_empty() || !self.operator_grant_projects.is_empty()
    }
}

fn project_not_removed(state: &AgentManagementSnapshot, project: &ProjectId) -> bool {
    crate::agent_management::project_exists(state, project)
}

/// Provider-store references (the Agent Management store is the only source).
pub(super) fn store_references(
    state: &AgentManagementSnapshot,
    id: &CutexSessionId,
) -> SessionDeleteReferences {
    let mut director_projects = state
        .projects
        .iter()
        .filter(|(project, authority)| {
            &authority.authorized_director_session == id && project_not_removed(state, project)
        })
        .map(|(project, _)| project.clone())
        .collect::<Vec<_>>();
    director_projects.sort();
    let mut operator_grant_projects = state
        .operator_grants
        .iter()
        .filter(|(project, grants)| grants.contains_key(id) && project_not_removed(state, project))
        .map(|(project, _)| project.clone())
        .collect::<Vec<_>>();
    operator_grant_projects.sort();
    let project_membership = match state.agents.get(id) {
        Some(agent) => current_project_id(state, agent),
        None => state
            .current_project_memberships
            .get(id)
            .and_then(|membership| membership.project_id.clone()),
    }
    .filter(|project| project_not_removed(state, project));
    SessionDeleteReferences {
        director_projects,
        operator_grant_projects,
        project_membership,
        open_assignments: Vec::new(),
    }
}

impl AgentManagementProvider {
    /// Everything that still references `id`: Director authority, Operator
    /// grants and current project membership (all Agent Management).
    pub fn session_delete_references(
        &self,
        id: &CutexSessionId,
    ) -> Result<SessionDeleteReferences, AgentManagementError> {
        Ok(store_references(&self.store().snapshot()?, id))
    }

    /// Drop the roster row and membership record of a session whose durable
    /// record was deleted. Refuses (without writing) when a provider-store
    /// reference appeared since the caller's check. Returns whether anything
    /// was removed.
    pub fn forget_deleted_session(&self, id: &CutexSessionId) -> Result<bool, AgentManagementError> {
        let _execution = super::provider::provider_execution_lock()
            .lock()
            .map_err(|_| AgentManagementError::PersistenceUnavailable)?;
        let _mutation = self.store().lock_mutations()?;
        self.store().with_state(true, |mut state| {
            if store_references(&state, id).blocks_delete() {
                return Err(AgentManagementError::Conflict("session_referenced"));
            }
            let removed_agent = state.agents.remove(id).is_some();
            let removed_membership = state.current_project_memberships.remove(id).is_some();
            let removed = removed_agent || removed_membership;
            Ok((state, removed, removed))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: &str) -> CutexSessionId {
        CutexSessionId::new(value.to_string()).unwrap()
    }

    #[test]
    fn only_director_or_operator_references_block_a_delete() {
        let project = ProjectId::new("project-7").unwrap();
        let member = SessionDeleteReferences { project_membership: Some(project.clone()), ..Default::default() };
        assert!(!member.is_empty());
        assert!(!member.blocks_delete());
        let director = SessionDeleteReferences { director_projects: vec![project.clone()], ..Default::default() };
        assert!(director.blocks_delete());
        let operator = SessionDeleteReferences { operator_grant_projects: vec![project], ..Default::default() };
        assert!(operator.blocks_delete());
        assert!(!SessionDeleteReferences::default().blocks_delete());
    }

    #[test]
    fn empty_store_has_no_references_and_forget_is_a_no_op() {
        let root = std::env::temp_dir().join(format!("cutex-delete-refs-{}", uuid::Uuid::new_v4()));
        let provider = AgentManagementProvider::open(root.join("provider")).unwrap();
        let references = provider
            .session_delete_references(&id("cutex.lonely"))
            .unwrap();
        assert!(references.is_empty());
        assert_eq!(
            serde_json::to_value(&references).unwrap(),
            serde_json::json!({
                "directorProjects": [],
                "operatorGrantProjects": [],
                "projectMembership": null,
                "openAssignments": [],
            })
        );
        assert!(!provider.forget_deleted_session(&id("cutex.lonely")).unwrap());
        let _ = std::fs::remove_dir_all(root);
    }
}
