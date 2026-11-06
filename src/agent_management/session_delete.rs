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
    ///
    /// r271 lock protocol (051 §2, same rule as decision A): a caller holding a
    /// session lane (session delete) only tries the execution lock and the host
    /// fence; busy is `HostBusy` and nothing is written (a lane-free retry
    /// finishes it).
    pub fn forget_deleted_session(&self, id: &CutexSessionId) -> Result<bool, AgentManagementError> {
        let busy = || AgentManagementError::HostBusy("hostBusy: roster cleanup waits for the host authority fence".into());
        let lane_held = crate::management::v2::session_coordination::thread_holds_lane();
        let _execution = if lane_held {
            match super::provider::provider_execution_lock().try_lock() {
                Ok(guard) => guard,
                Err(std::sync::TryLockError::WouldBlock) => return Err(busy()),
                Err(std::sync::TryLockError::Poisoned(_)) => return Err(AgentManagementError::PersistenceUnavailable),
            }
        } else {
            super::provider::provider_execution_lock()
                .lock()
                .map_err(|_| AgentManagementError::PersistenceUnavailable)?
        };
        let _mutation = if lane_held {
            self.store().try_lock_delivery_mutations()?.ok_or_else(busy)?
        } else {
            self.store().lock_mutations()?
        };
        let removed = self.store().with_state(true, |mut state| {
            if store_references(&state, id).blocks_delete() {
                return Err(AgentManagementError::Conflict("session_referenced"));
            }
            let removed_agent = state.agents.remove(id).is_some();
            let removed_membership = state.current_project_memberships.remove(id).is_some();
            let removed = removed_agent || removed_membership;
            Ok((state, removed, removed))
        })?;
        // The row is gone: readers no longer need the deletion marker.
        if let Err(error) = super::pending_forget::clear(id.as_str()) {
            eprintln!("session delete: pending-forget marker of {} kept: {error:#}", id.as_str());
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: &str) -> CutexSessionId {
        CutexSessionId::new(value.to_string()).unwrap()
    }

    /// r271 (051 §2): session delete with the host fence busy leaves the
    /// roster row for the lane-free sweep. Until it is gone the deleted id has
    /// no routing authority and no project membership; afterwards the row and
    /// the marker are gone.
    #[test]
    fn deleted_session_is_gone_for_authority_readers_before_the_sweep() {
        use crate::asks::routing_policy::{integration, RoutingActor, RoutingError, RoutingRef};
        let root = std::env::temp_dir().join(format!("cutex-delete-pending-{}", uuid::Uuid::new_v4()));
        let provider = AgentManagementProvider::open(root.join("provider")).unwrap();
        let deleted = id(&format!("cutex.deleted-{}", uuid::Uuid::new_v4()));
        let project = "project-pending-forget";
        provider.store().with_state(true, |mut state| {
            let mut roster = crate::test_support::waste_scale::roster_with_agents();
            let mut agent = roster.agents.pop_first().unwrap().1;
            agent.cutex_session_id = deleted.clone();
            state.agents.insert(deleted.clone(), agent);
            state.project_states.insert(ProjectId::new(project.to_string()).unwrap(), serde_json::from_value(serde_json::json!({
                "project_id": project, "lifecycle": "active", "revision": 1, "created_at": "2026-10-31T00:00:00Z",
                "updated_at": "2026-10-31T00:00:00Z", "archived_at": null, "workspace_root": null, "last_director_epoch": null
            })).unwrap());
            state.current_project_memberships.insert(deleted.clone(), serde_json::from_value(serde_json::json!({
                "cutex_session_id": deleted.as_str(), "project_id": project, "revision": 1, "updated_at": "2026-10-31T00:00:00Z"
            })).unwrap());
            Ok((state, (), true))
        }).unwrap();
        let host = integration::local_host_id();
        let actor = || RoutingActor::Agent { reference: RoutingRef::new(&host, deleted.as_str()).unwrap(), name: None };
        let snapshot = provider.store().snapshot().unwrap();
        assert!(integration::trusted_context(&snapshot, project, actor(), &host).is_ok(), "a live member routes");
        assert!(crate::management::v2::delegations::membership(&snapshot, project).members.contains(deleted.as_str()));
        // Delete marked it and removed the durable record; the fence is busy.
        super::super::pending_forget::mark(deleted.as_str()).unwrap();
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let fence_root = root.join("provider");
        let holder = std::thread::spawn(move || {
            let store = AgentManagementStore::open(&fence_root).unwrap();
            let _fence = store.lock_mutations().unwrap();
            held_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        held_rx.recv().unwrap();
        let lane = crate::management::v2::session_coordination::session_lane(deleted.as_str()).unwrap();
        {
            let _lane = lane.reserve().unwrap();
            assert!(matches!(provider.forget_deleted_session(&deleted), Err(AgentManagementError::HostBusy(_))));
        }
        let snapshot = provider.store().snapshot().unwrap();
        assert!(snapshot.agents.contains_key(&deleted), "row still present before the sweep");
        let refused = integration::trusted_context(&snapshot, project, actor(), &host).unwrap_err();
        assert_eq!(refused.code, RoutingError::NotAuthorized.code());
        assert!(!crate::management::v2::delegations::membership(&snapshot, project).members.contains(deleted.as_str()));
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        // The lane-free sweep path.
        assert!(provider.forget_deleted_session(&deleted).unwrap());
        assert!(!provider.store().snapshot().unwrap().agents.contains_key(&deleted), "row gone after the sweep");
        assert!(!super::super::pending_forget::contains(deleted.as_str()), "marker cleared");
        let _ = std::fs::remove_dir_all(root);
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
