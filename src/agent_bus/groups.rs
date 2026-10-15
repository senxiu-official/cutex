//! Agent bus collaboration group normalization and mutation helpers.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::anyhow;

use crate::agent_bus::identity::default_agent_group_for;
use crate::agent_bus::identity::normalize_agent_groups;
use crate::agent_bus::model::AgentBusAgent;
use crate::agent_bus::model::AgentBusRegisterRequest;
use crate::agent_bus::model::AgentGroupUpdateMode;
use crate::agent_bus::routing::resolve_agent_target;
use crate::agent_bus::store::AgentBusState;

pub fn normalize_registered_agent_groups(
    groups: Vec<String>,
    path_key: Option<&str>,
    cwd: &str,
) -> Vec<String> {
    let mut groups = normalize_agent_groups(groups);
    if groups.is_empty() {
        groups.push(default_agent_group_for(path_key, cwd));
    } else {
        let default_group = default_agent_group_for(path_key, cwd);
        if !groups.iter().any(|group| group == &default_group) {
            groups.insert(0, default_group);
        }
    }
    groups
}

pub fn agent_from_register_request(
    payload: AgentBusRegisterRequest,
    now_epoch_secs: u64,
) -> AgentBusAgent {
    let path_key = payload.path_key.filter(|value| !value.trim().is_empty());
    let cwd = payload.cwd;
    let groups = normalize_registered_agent_groups(payload.groups, path_key.as_deref(), &cwd);
    AgentBusAgent {
        id: payload.id,
        name: payload.name,
        base_name: payload.base_name.filter(|value| !value.trim().is_empty()),
        thread_name: payload.thread_name.filter(|value| !value.trim().is_empty()),
        path_key,
        session_id: payload.session_id.filter(|value| !value.trim().is_empty()),
        cutex_session_id: None,
        profile: payload.profile,
        cwd,
        pid: payload.pid,
        host_id: payload
            .host_id
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
        groups,
        registration_class: payload.registration_class,
        last_seen_epoch_secs: now_epoch_secs,
    }
}

pub fn update_agent_groups(
    state: &Arc<Mutex<AgentBusState>>,
    target: &str,
    groups: &[String],
    mode: AgentGroupUpdateMode,
) -> anyhow::Result<(String, String, Vec<String>, AgentBusAgent)> {
    let agent_id = resolve_agent_target(state, target)?;
    let mut state = state
        .lock()
        .map_err(|_| anyhow!("agent bus state lock poisoned"))?;
    let agent = state
        .agents
        .get_mut(&agent_id)
        .ok_or_else(|| anyhow!("Agent disappeared before group update: {agent_id}"))?;
    let next_groups = apply_group_update(&agent.groups, groups, mode);
    agent.groups = next_groups.clone();
    Ok((
        agent.id.clone(),
        agent.name.clone(),
        next_groups,
        agent.clone(),
    ))
}

pub fn apply_group_update(
    existing: &[String],
    groups: &[String],
    mode: AgentGroupUpdateMode,
) -> Vec<String> {
    match mode {
        AgentGroupUpdateMode::Set => groups.to_vec(),
        AgentGroupUpdateMode::Add => {
            let mut merged = existing.to_vec();
            merged.extend(groups.iter().cloned());
            normalize_agent_groups(merged)
        }
        AgentGroupUpdateMode::Remove => {
            let remove = groups.iter().map(String::as_str).collect::<HashSet<_>>();
            existing
                .iter()
                .filter(|group| !remove.contains(group.as_str()))
                .cloned()
                .collect()
        }
    }
}

pub fn resolve_agent_display_name(
    state: &Arc<Mutex<AgentBusState>>,
    agent_id: &str,
) -> Option<String> {
    state
        .lock()
        .ok()
        .and_then(|state| state.agents.get(agent_id).map(|agent| agent.name.clone()))
}

pub fn agent_groups_for_id(
    state: &Arc<Mutex<AgentBusState>>,
    agent_id: &str,
) -> Option<Vec<String>> {
    state
        .lock()
        .ok()
        .and_then(|state| state.agents.get(agent_id).map(|agent| agent.groups.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_groups_include_default_project_group() {
        let groups = normalize_registered_agent_groups(vec!["waveline".to_string()], None, "/tmp");

        assert!(groups.iter().any(|group| group == "waveline"));
        assert!(groups.iter().any(|group| group.starts_with("project:")));
    }

    #[test]
    fn agent_from_register_request_trims_optional_identity_fields() {
        let agent = agent_from_register_request(
            AgentBusRegisterRequest {
                id: "agent-id".to_string(),
                name: "agent-name".to_string(),
                base_name: Some(" ".to_string()),
                thread_name: Some("thread".to_string()),
                path_key: Some("pathkey".to_string()),
                session_id: Some("session-id".to_string()),
                profile: "orchid".to_string(),
                cwd: "/tmp/work".to_string(),
                pid: 42,
                host_id: Some(" devbox ".to_string()),
                groups: vec!["waveline".to_string()],
                registration_class: crate::agent_bus::model::AgentRegistrationClass::Persistent,
            },
            123,
        );

        assert_eq!(agent.id, "agent-id");
        assert_eq!(agent.base_name, None);
        assert_eq!(agent.thread_name.as_deref(), Some("thread"));
        assert_eq!(agent.path_key.as_deref(), Some("pathkey"));
        assert_eq!(agent.session_id.as_deref(), Some("session-id"));
        assert_eq!(agent.host_id.as_deref(), Some("devbox"));
        assert_eq!(agent.last_seen_epoch_secs, 123);
        assert!(agent.groups.iter().any(|group| group == "waveline"));
        assert!(agent.groups.iter().any(|group| group == "project:pathkey"));
    }

    #[test]
    fn apply_group_update_adds_and_removes_normalized_groups() {
        let existing = vec!["project:abc".to_string(), "waveline".to_string()];
        let added = apply_group_update(
            &existing,
            &["aria".to_string(), "waveline".to_string()],
            AgentGroupUpdateMode::Add,
        );

        assert_eq!(
            added,
            vec![
                "project:abc".to_string(),
                "waveline".to_string(),
                "aria".to_string()
            ]
        );

        let removed = apply_group_update(
            &added,
            &["waveline".to_string()],
            AgentGroupUpdateMode::Remove,
        );
        assert_eq!(removed, vec!["project:abc".to_string(), "aria".to_string()]);
    }
}
