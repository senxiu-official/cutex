//! Explicit old-test fixture facts. This module does not exist in production.
//! Synthetic projects deliberately use open routing because the old tests
//! exercise capability/queue behavior. Real seeded roster projects retain the
//! true default, as do all dedicated routing integration tests.
use super::*;
use crate::agent_management::{AgentManagementSnapshot, ProjectId};
use crate::asks::{
    directory::Directory,
    store::{AskError, AskState},
};
use chrono::{DateTime, Utc};
use serde_json::json;
use std::collections::BTreeSet;

pub struct AuthorityFixture {
    pub snapshot: AgentManagementSnapshot,
    pub synthetic_projects: BTreeSet<String>,
}

pub fn for_directory(directory: &Directory) -> AuthorityFixture {
    let mut snapshot = Directory::director_snapshot().expect("private-home test roster");
    let mut synthetic_projects = BTreeSet::new();
    let projects: BTreeSet<_> = directory
        .agents
        .values()
        .filter_map(|a| a.project.clone())
        .collect();
    for project in projects {
        let project_id = ProjectId::new(project.clone()).expect("fixture project id");
        if crate::agent_management::project_entry(&snapshot, &project_id).is_some() {
            continue;
        }
        synthetic_projects.insert(project.clone());
        snapshot.project_states.insert(project_id,serde_json::from_value(json!({
            "project_id":project,"lifecycle":"active","revision":1,"created_at":"2026-10-27T00:00:00Z",
            "updated_at":"2026-10-27T00:00:00Z","archived_at":null,"workspace_root":null,"last_director_epoch":null
        })).expect("explicit fixture project"));
    }
    for agent in directory.agents.values().filter(|a| {
        a.project
            .as_ref()
            .is_some_and(|p| synthetic_projects.contains(p))
    }) {
        let session = crate::role_revision::CutexSessionId::new(agent.cutex_session_id.clone())
            .expect("fixture canonical session");
        snapshot.current_project_memberships.insert(session,serde_json::from_value(json!({
            "cutex_session_id":agent.cutex_session_id,"project_id":agent.project,"revision":1,"updated_at":"2026-10-27T00:00:00Z"
        })).expect("explicit fixture membership"));
    }
    AuthorityFixture {
        snapshot,
        synthetic_projects,
    }
}

pub fn configure(
    state: &mut AskState,
    fixture: &AuthorityFixture,
    now: DateTime<Utc>,
) -> Result<(), AskError> {
    let host = integration::local_host_id();
    for project in &fixture.synthetic_projects {
        if state.routing.policies.contains_key(project) {
            continue;
        }
        let ctx =
            integration::trusted_context(&fixture.snapshot, project, RoutingActor::Owner, &host)?;
        state
            .routing
            .update_policy(
                &ctx,
                &UpdatePolicyRequest {
                    action_id: format!("fixture-routing-{project}"),
                    expected_revision: 0,
                    member_mode: MemberMode::Open,
                },
                now,
            )
            .map_err(integration::error)?;
    }
    Ok(())
}
