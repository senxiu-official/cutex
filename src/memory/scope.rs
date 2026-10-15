//! Who may see which project memory.
//!
//! Membership comes from the Agent Management snapshot: an Agent's current
//! project (`current_project_id`) or the project's authorised Director seat.
//! Independent Agents (no project) have no memory scope.

use std::path::PathBuf;

use crate::agent_management::AgentManagementSnapshot;
use crate::agent_management::AgentManagementStore;
use crate::agent_management::ProjectId;
use crate::agent_management::ProjectLifecycle;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectMemoryScope {
    pub project_id: ProjectId,
    pub dir: PathBuf,
}

pub fn load_agent_management_snapshot() -> anyhow::Result<AgentManagementSnapshot> {
    AgentManagementStore::open_default()?
        .snapshot()
        .map_err(anyhow::Error::new)
}

/// The project a session currently belongs to (member or Director seat).
pub fn project_for_session(
    snapshot: &AgentManagementSnapshot,
    cutex_session_id: &str,
) -> Option<ProjectId> {
    let member = snapshot
        .agents
        .values()
        .find(|agent| {
            agent.cutex_session_id.as_str() == cutex_session_id && agent.retired_at.is_none()
        })
        .and_then(|agent| crate::agent_management::current_project_id(snapshot, agent));
    let project = member.or_else(|| {
        snapshot
            .projects
            .values()
            .find(|authority| authority.authorized_director_session.as_str() == cutex_session_id)
            .map(|authority| authority.project_id.clone())
    })?;
    project_is_known(snapshot, &project).then_some(project)
}

/// A project exists for memory purposes while it is not removed, with or
/// without a Director (archived projects keep their memory readable and
/// writable).
pub fn project_is_known(snapshot: &AgentManagementSnapshot, project_id: &ProjectId) -> bool {
    crate::agent_management::project_exists(snapshot, project_id)
}

/// Current, non-retired members plus the Director seat, sorted and deduped.
pub fn project_member_session_ids(
    snapshot: &AgentManagementSnapshot,
    project_id: &ProjectId,
) -> Vec<String> {
    let mut ids: Vec<String> = snapshot
        .agents
        .values()
        .filter(|agent| agent.retired_at.is_none())
        .filter(|agent| {
            crate::agent_management::current_project_id(snapshot, agent).as_ref()
                == Some(project_id)
        })
        .map(|agent| agent.cutex_session_id.as_str().to_string())
        .collect();
    if let Some(authority) = snapshot.projects.get(project_id) {
        ids.push(authority.authorized_director_session.as_str().to_string());
    }
    ids.sort();
    ids.dedup();
    ids
}

/// Memory scope of one session, or `None` for independent Agents.
pub fn memory_scope_for_session(
    snapshot: &AgentManagementSnapshot,
    cutex_session_id: &str,
) -> anyhow::Result<Option<ProjectMemoryScope>> {
    let Some(project_id) = project_for_session(snapshot, cutex_session_id) else {
        return Ok(None);
    };
    let dir = super::project_memory_dir(&project_id)?;
    Ok(Some(ProjectMemoryScope { project_id, dir }))
}
