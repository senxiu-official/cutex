//! Final agent writes share the roster mutation fence with membership and
//! Director changes. Approval waits must finish before entering this scope.
use super::store::{Caller, ServicesError, ServicesResult};
use crate::agent_management::AgentManagementStore;

pub(super) fn commit<T>(
    caller: &Caller,
    revalidate: bool,
    mutation: impl FnOnce(&Caller) -> ServicesResult<T>,
) -> ServicesResult<T> {
    if !revalidate {
        // Internal test adapters explicitly provide their trusted caller.
        return mutation(caller);
    }
    let roster = AgentManagementStore::open_default().map_err(ServicesError::unavailable)?;
    commit_in(&roster, caller, mutation)
}

fn commit_in<T>(
    roster: &AgentManagementStore,
    caller: &Caller,
    mutation: impl FnOnce(&Caller) -> ServicesResult<T>,
) -> ServicesResult<T> {
    let _fence = roster.lock_mutations().map_err(ServicesError::unavailable)?;
    let snapshot = roster.snapshot().map_err(ServicesError::unavailable)?;
    let fresh = match caller {
        Caller::Owner => Caller::Owner,
        Caller::Agent { cutex_session_id, .. } => {
            let projects = crate::asks::directory::Directory::director_projects(&snapshot, cutex_session_id);
            Caller::Agent {
                cutex_session_id: cutex_session_id.clone(),
                project_id: (projects.len() == 1).then(|| projects[0].clone()),
            }
        }
    };
    if let (
        Caller::Agent { project_id: before, .. },
        Caller::Agent { project_id: after, .. },
    ) = (caller, &fresh) {
        if before != after {
            return Err(ServicesError::new(
                403,
                "not_service_owner",
                "your project context changed while the request was pending; retry in the current project",
            ));
        }
    }
    crate::director_address::with_authority_snapshot(&snapshot, || mutation(&fresh))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::store::{RegisterRequest, ServiceStore, UpdateRequest};

    #[test]
    fn final_write_refreshes_membership_and_serializes_director_changes() {
        let root = std::env::temp_dir().join(format!("services-authority-{}", uuid::Uuid::new_v4()));
        let roster = AgentManagementStore::open(root.join("roster")).unwrap();
        let services = ServiceStore::new(root.join("services"), "test").with_port_probe(|_| true);
        let registrant = "cutex.00000000-0000-0000-0000-000000000001";
        let old = "cutex.00000000-0000-0000-0000-000000000002";
        let new = "cutex.00000000-0000-0000-0000-000000000003";
        let caller = |id: &str| Caller::Agent { cutex_session_id: id.into(), project_id: Some("proj".into()) };
        services.register(serde_json::from_value::<RegisterRequest>(serde_json::json!({
            "slug":"board", "projectId":"proj", "ownerScope":"project", "ownerCutexSessionId":registrant
        })).unwrap(), &Caller::Owner).unwrap();
        let set_director = |director: &str| {
            let _fence = roster.lock_mutations().unwrap();
            roster.with_state(true, |mut snapshot| {
                snapshot.projects.insert(crate::agent_management::ProjectId::new("proj").unwrap(),
                    serde_json::from_value(serde_json::json!({"project_id":"proj", "authorized_director_session":director,
                        "authority_epoch":1, "updated_at":"2026-10-28T00:00:00Z"})).unwrap());
                Ok((snapshot, (), true))
            }).unwrap();
        };
        set_director(old);
        let rename = |name: &str| UpdateRequest { display_name: Some(name.into()), ..Default::default() };
        // The original caller's project hint cannot revive removed membership.
        assert_eq!(commit_in(&roster, &caller(registrant), |fresh| services.update("board", rename("stale"), fresh))
            .unwrap_err().code, "not_service_owner");
        commit_in(&roster, &caller(old), |fresh| {
            assert!(roster.try_lock_delivery_mutations().unwrap().is_none(), "handoff fence must span write");
            services.update("board", rename("before handoff"), fresh)
        }).unwrap();
        set_director(new);
        assert_eq!(commit_in(&roster, &caller(old), |fresh| services.update("board", rename("stale director"), fresh))
            .unwrap_err().code, "not_service_owner");
        commit_in(&roster, &caller(new), |fresh| services.update("board", rename("new director"), fresh)).unwrap();
        assert_eq!(services.get("board").unwrap().display_name, "new director");
    }
}
