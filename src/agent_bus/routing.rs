//! Agent bus target resolution, group visibility, and stable session helpers.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::anyhow;

use crate::agent_bus::model::AgentBusAgent;
use crate::agent_bus::model::AgentBusSendRequest;
use crate::agent_bus::store::AgentBusState;
use crate::session::model::CutexSessionRecord;
use crate::session::model::CutexSessionStore;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentTargetResolutionCode {
    NotFound,
    Ambiguous,
    TargetUnavailable,
}

impl AgentTargetResolutionCode {
    pub fn label(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::Ambiguous => "ambiguous",
            Self::TargetUnavailable => "target_unavailable",
        }
    }
}

#[derive(Debug)]
pub struct AgentTargetResolutionError {
    code: AgentTargetResolutionCode,
    detail: String,
}

impl AgentTargetResolutionError {
    fn new(code: AgentTargetResolutionCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    pub fn code(&self) -> AgentTargetResolutionCode {
        self.code
    }

    pub fn ambiguous(detail: impl Into<String>) -> Self {
        Self::new(AgentTargetResolutionCode::Ambiguous, detail)
    }
}

impl std::fmt::Display for AgentTargetResolutionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for AgentTargetResolutionError {}

pub fn agent_sender_label(agent: &AgentBusAgent) -> String {
    agent
        .base_name
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| (!agent.name.trim().is_empty()).then_some(agent.name.as_str()))
        .unwrap_or(agent.id.as_str())
        .to_string()
}

pub fn resolve_agent_message_sender_name(
    state: &Arc<Mutex<AgentBusState>>,
    payload: &AgentBusSendRequest,
) -> String {
    if let Some(from) = payload
        .from
        .as_ref()
        .filter(|value| !value.trim().is_empty())
    {
        return from.clone();
    }
    if let Some(agent_id) = payload
        .from_agent_id
        .as_ref()
        .filter(|value| !value.trim().is_empty())
    {
        return state
            .lock()
            .ok()
            .and_then(|state| state.agents.get(agent_id).map(|agent| agent.name.clone()))
            .unwrap_or_else(|| agent_id.clone());
    }
    "cutex".to_string()
}

pub fn groups_overlap(left: &[String], right: &[String]) -> bool {
    if left.is_empty() || right.is_empty() {
        return false;
    }
    let left = left.iter().map(String::as_str).collect::<HashSet<_>>();
    right.iter().any(|group| left.contains(group.as_str()))
}

pub fn visible_agents_for_request(
    state: &AgentBusState,
    requester: Option<&str>,
    all_groups: bool,
) -> Vec<AgentBusAgent> {
    if all_groups {
        return state.agents.values().cloned().collect();
    }
    let Some(requester) = requester else {
        return state.agents.values().cloned().collect();
    };
    let Some(requester_agent) = state.agents.get(requester) else {
        return Vec::new();
    };
    state
        .agents
        .values()
        .filter(|agent| {
            agent.id == requester || groups_overlap(&agent.groups, &requester_agent.groups)
        })
        .cloned()
        .collect()
}

pub fn resolve_agent_target(
    state: &Arc<Mutex<AgentBusState>>,
    target: &str,
) -> anyhow::Result<String> {
    resolve_agent_target_for_sender(state, target, None, true)
}

pub fn resolve_agent_target_for_sender(
    state: &Arc<Mutex<AgentBusState>>,
    target: &str,
    sender_agent_id: Option<&str>,
    all_groups: bool,
) -> anyhow::Result<String> {
    resolve_agent_target_for_sender_with_sessions(state, target, sender_agent_id, all_groups, None)
}

pub fn resolve_agent_target_for_sender_with_sessions(
    state: &Arc<Mutex<AgentBusState>>,
    target: &str,
    sender_agent_id: Option<&str>,
    all_groups: bool,
    sessions: Option<&CutexSessionStore>,
) -> anyhow::Result<String> {
    let state = state
        .lock()
        .map_err(|_| anyhow!("agent bus state lock poisoned"))?;
    let visible_agents = visible_agents_for_request(&state, sender_agent_id, all_groups);
    let scope_label = if all_groups {
        "registered agent scope"
    } else {
        "visible group scope"
    };
    if durable_target_requested(target, sessions) {
        return resolve_durable_target(&state, &visible_agents, target, sessions, scope_label)
            .map(|agent| agent.id)
            .map_err(anyhow::Error::from);
    }
    if state.agents.contains_key(target) {
        return Ok(target.to_string());
    }
    resolve_visible_agent_target(&visible_agents, target, all_groups, scope_label)
        .map(|agent| agent.id)
        .map_err(anyhow::Error::from)
}

fn resolve_visible_agent_target(
    visible_agents: &[AgentBusAgent],
    target: &str,
    all_groups: bool,
    scope_label: &str,
) -> Result<AgentBusAgent, AgentTargetResolutionError> {
    let display_matches = visible_agents
        .iter()
        .filter(|agent| agent.name == target)
        .cloned()
        .collect::<Vec<_>>();
    match display_matches.as_slice() {
        [agent] => Ok(agent.clone()),
        [] => {
            let base_matches = visible_agents
                .iter()
                .filter(|agent| {
                    agent.base_name.as_deref() == Some(target)
                        || agent.thread_name.as_deref() == Some(target)
                        || agent.session_id.as_deref() == Some(target)
                })
                .cloned()
                .collect::<Vec<_>>();
            match base_matches.as_slice() {
                [agent] => Ok(agent.clone()),
                [] => Err(AgentTargetResolutionError::new(
                    AgentTargetResolutionCode::NotFound,
                    no_agent_match_error(target, all_groups),
                )),
                _ => Err(AgentTargetResolutionError::new(
                    AgentTargetResolutionCode::Ambiguous,
                    format!(
                        "Agent name/session `{target}` is ambiguous in the {scope_label}; use `cutex agent list --all-groups` and send to the display name or full id"
                    ),
                )),
            }
        }
        _ => Err(AgentTargetResolutionError::new(
            AgentTargetResolutionCode::Ambiguous,
            format!(
                "Agent name `{target}` is ambiguous in the {scope_label}; use `cutex agent list --all-groups` and send to a full id"
            ),
        )),
    }
}

pub fn is_full_durable_cutex_session_id(target: &str) -> bool {
    target
        .strip_prefix("cutex.")
        .is_some_and(|suffix| uuid::Uuid::parse_str(suffix).is_ok())
}

fn durable_target_requested(target: &str, sessions: Option<&CutexSessionStore>) -> bool {
    sessions.is_some_and(|store| {
        store
            .sessions
            .values()
            .any(|record| record.cutex_session_id == target)
    }) || is_full_durable_cutex_session_id(target)
}

fn roster_agent_matches_session(agent: &AgentBusAgent, record: &CutexSessionRecord) -> bool {
    if let Some(session) = &record.claude_session {
        return agent.session_id.as_deref() == Some(session.thread_id.as_str())
            && record.current_runtime_agent_id.as_deref() == Some(agent.id.as_str())
            && record.runtime_pid == Some(agent.pid);
    }
    agent.session_id.as_deref() == Some(record.cutex_session_id.as_str())
        || record.codex_session_id.as_deref() == agent.session_id.as_deref()
}

fn resolve_durable_target(
    state: &AgentBusState,
    visible_agents: &[AgentBusAgent],
    target: &str,
    sessions: Option<&CutexSessionStore>,
    scope_label: &str,
) -> Result<AgentBusAgent, AgentTargetResolutionError> {
    if let Some(sessions) = sessions {
        let records = sessions
            .sessions
            .values()
            .filter(|record| record.cutex_session_id == target)
            .collect::<Vec<_>>();
        match records.as_slice() {
            [record] => {
                let runtime_id = record
                    .current_runtime_agent_id
                    .as_deref()
                    .filter(|_| record.is_active() && record.runtime_generation > 0);
                let Some(runtime_id) = runtime_id else {
                    return Err(AgentTargetResolutionError::new(
                        AgentTargetResolutionCode::TargetUnavailable,
                        format!("Durable cutex session `{target}` has no current online endpoint"),
                    ));
                };
                let Some(agent) = state.agents.get(runtime_id) else {
                    return Err(AgentTargetResolutionError::new(
                        AgentTargetResolutionCode::TargetUnavailable,
                        format!("Durable cutex session `{target}` has no current online endpoint"),
                    ));
                };
                if !roster_agent_matches_session(agent, record) {
                    return Err(AgentTargetResolutionError::new(
                        AgentTargetResolutionCode::TargetUnavailable,
                        format!("Durable cutex session `{target}` has a stale runtime endpoint"),
                    ));
                }
                if !visible_agents.iter().any(|visible| visible.id == agent.id) {
                    return Err(AgentTargetResolutionError::new(
                        AgentTargetResolutionCode::NotFound,
                        format!(
                            "No visible registered cutex agent matches `{target}` in the {scope_label}"
                        ),
                    ));
                }
                return Ok(agent.clone());
            }
            [] => {}
            _ => {
                return Err(AgentTargetResolutionError::new(
                    AgentTargetResolutionCode::Ambiguous,
                    format!("Durable cutex session `{target}` has multiple current records"),
                ));
            }
        }
    }

    let matches = visible_agents
        .iter()
        .filter(|agent| agent.cutex_session_id.as_deref() == Some(target))
        .cloned()
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [agent] => Ok(agent.clone()),
        [] => Err(AgentTargetResolutionError::new(
            AgentTargetResolutionCode::NotFound,
            no_agent_match_error(target, false),
        )),
        _ => Err(AgentTargetResolutionError::new(
            AgentTargetResolutionCode::Ambiguous,
            format!("Durable cutex session `{target}` has multiple current endpoints"),
        )),
    }
}

pub fn project_current_durable_session_ids(
    agents: &mut [AgentBusAgent],
    sessions: &CutexSessionStore,
) {
    for agent in agents {
        agent.cutex_session_id = None;
        let records = sessions
            .sessions
            .values()
            .filter(|record| {
                record.is_active()
                    && record.runtime_generation > 0
                    && record.current_runtime_agent_id.as_deref() == Some(agent.id.as_str())
                    && roster_agent_matches_session(agent, record)
            })
            .collect::<Vec<_>>();
        let [record] = records.as_slice() else {
            continue;
        };
        let durable_record_count = sessions
            .sessions
            .values()
            .filter(|candidate| candidate.cutex_session_id == record.cutex_session_id)
            .count();
        if durable_record_count == 1 {
            agent.cutex_session_id = Some(record.cutex_session_id.clone());
        }
    }
}

pub fn no_agent_match_error(target: &str, all_groups: bool) -> String {
    if all_groups {
        format!("No registered cutex agent matches `{target}`")
    } else {
        format!(
            "No visible registered cutex agent matches `{target}`; use a full runtime id or pass --all-groups to search every registered group"
        )
    }
}

pub fn resolve_agent_target_from_agent_list(
    agents: &[AgentBusAgent],
    target: &str,
    sender_groups: Option<&[String]>,
    all_groups: bool,
) -> anyhow::Result<AgentBusAgent> {
    if let Some(agent) = agents.iter().find(|agent| agent.id == target) {
        return Ok(agent.clone());
    }
    let visible_agents = if all_groups {
        agents.to_vec()
    } else if let Some(sender_groups) = sender_groups {
        agents
            .iter()
            .filter(|agent| groups_overlap(&agent.groups, sender_groups))
            .cloned()
            .collect::<Vec<_>>()
    } else {
        agents.to_vec()
    };
    if durable_target_requested(target, None) {
        return resolve_durable_target(
            &AgentBusState::default(),
            &visible_agents,
            target,
            None,
            "peer bus scope",
        )
        .map_err(anyhow::Error::from);
    }
    resolve_visible_agent_target(&visible_agents, target, all_groups, "peer bus scope")
        .map_err(anyhow::Error::from)
}

pub fn agent_bus_agent_session_id(agent: &AgentBusAgent) -> Option<String> {
    normalize_agent_bus_session_id(agent.session_id.as_deref())
}

pub fn agent_bus_agent_snapshot_by_id(
    state: &Arc<Mutex<AgentBusState>>,
    agent_id: &str,
) -> Option<AgentBusAgent> {
    state
        .lock()
        .ok()
        .and_then(|state| state.agents.get(agent_id).cloned())
}

pub fn agent_bus_agent_session_id_by_id(
    state: &Arc<Mutex<AgentBusState>>,
    agent_id: &str,
) -> Option<String> {
    agent_bus_agent_snapshot_by_id(state, agent_id)
        .as_ref()
        .and_then(agent_bus_agent_session_id)
}

pub fn normalize_agent_bus_session_id(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() || value.contains('/') || value.contains('\\') {
        return None;
    }
    Some(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_bus::delivery::AgentDeliveryMode;
    use crate::agent_bus::model::AgentBusEnvelopeKind;
    use crate::agent_bus::model::AgentMessageKind;
    use crate::agent_bus::model::AgentRegistrationClass;
    use crate::session::model::CutexSessionRecord;

    fn sample_agent(
        id: &str,
        name: &str,
        base_name: Option<&str>,
        session_id: Option<&str>,
        groups: &[&str],
    ) -> AgentBusAgent {
        AgentBusAgent {
            id: id.to_string(),
            name: name.to_string(),
            base_name: base_name.map(str::to_string),
            thread_name: base_name.map(str::to_string),
            path_key: None,
            session_id: session_id.map(str::to_string),
            cutex_session_id: None,
            profile: "orchid".to_string(),
            cwd: "/tmp".to_string(),
            pid: 1,
            host_id: None,
            groups: groups.iter().map(|group| group.to_string()).collect(),
            registration_class: AgentRegistrationClass::LocalOnly,
            last_seen_epoch_secs: 1,
        }
    }

    #[test]
    fn visible_agents_follow_sender_group_scope() {
        let mut state = AgentBusState::default();
        state.agents.insert(
            "leader".to_string(),
            sample_agent("leader", "leader.111", Some("leader"), None, &["alpha"]),
        );
        state.agents.insert(
            "worker".to_string(),
            sample_agent("worker", "worker.222", Some("worker"), None, &["alpha"]),
        );
        state.agents.insert(
            "hidden".to_string(),
            sample_agent("hidden", "hidden.333", Some("hidden"), None, &["beta"]),
        );

        let mut visible = visible_agents_for_request(&state, Some("leader"), false)
            .into_iter()
            .map(|agent| agent.id)
            .collect::<Vec<_>>();
        visible.sort();

        assert_eq!(visible, vec!["leader".to_string(), "worker".to_string()]);
    }

    #[test]
    fn peer_target_resolution_respects_sender_groups() {
        let agents = vec![
            sample_agent(
                "remote-worker",
                "worker.222",
                Some("worker"),
                Some("session-worker"),
                &["waveline", "project:alpha"],
            ),
            sample_agent(
                "remote-hidden",
                "hidden.333",
                Some("hidden"),
                None,
                &["project:beta"],
            ),
        ];
        let sender_groups = vec!["waveline".to_string()];

        assert_eq!(
            resolve_agent_target_from_agent_list(&agents, "worker", Some(&sender_groups), false)
                .expect("worker should resolve")
                .id,
            "remote-worker"
        );
        assert!(resolve_agent_target_from_agent_list(
            &agents,
            "hidden",
            Some(&sender_groups),
            false
        )
        .is_err());
        assert_eq!(
            resolve_agent_target_from_agent_list(&agents, "hidden", Some(&sender_groups), true)
                .expect("all_groups should resolve hidden")
                .id,
            "remote-hidden"
        );
    }

    fn active_session(
        durable_id: &str,
        native_session_id: &str,
        runtime_id: Option<&str>,
        generation: u64,
    ) -> CutexSessionRecord {
        let mut record = CutexSessionRecord::new_at(
            durable_id.to_string(),
            Some(native_session_id.to_string()),
            "host-a".to_string(),
            "/tmp".to_string(),
            None,
            "2026-08-29T01:00:00Z".to_string(),
        )
        .expect("session record");
        record.current_runtime_agent_id = runtime_id.map(str::to_string);
        record.runtime_generation = generation;
        record.agent_enabled = true;
        record
    }

    fn target_code(error: anyhow::Error) -> AgentTargetResolutionCode {
        error
            .downcast_ref::<AgentTargetResolutionError>()
            .expect("typed target resolution error")
            .code()
    }

    #[test]
    fn durable_session_resolves_only_current_visible_restart_generation() {
        let durable_id = "cutex.018e0000-0000-7000-8000-0000a000037e";
        let native_session_id = "018e0000-0000-7000-8000-0000a000037e";
        let mut state = AgentBusState::default();
        state.agents.insert(
            "sender".to_string(),
            sample_agent("sender", "sender", Some("sender"), None, &["alpha"]),
        );
        state.agents.insert(
            "runtime-old".to_string(),
            sample_agent(
                "runtime-old",
                "worker-old",
                Some("worker"),
                Some(native_session_id),
                &["alpha"],
            ),
        );
        state.agents.insert(
            "runtime-new".to_string(),
            sample_agent(
                "runtime-new",
                "worker-new",
                Some("worker-new"),
                Some(native_session_id),
                &["alpha"],
            ),
        );
        let state = Arc::new(Mutex::new(state));
        let mut sessions = CutexSessionStore::default();
        sessions.sessions.insert(
            durable_id.to_string(),
            active_session(durable_id, native_session_id, Some("runtime-new"), 2),
        );

        assert_eq!(
            resolve_agent_target_for_sender_with_sessions(
                &state,
                durable_id,
                Some("sender"),
                false,
                Some(&sessions),
            )
            .expect("durable target"),
            "runtime-new"
        );
        assert_eq!(
            resolve_agent_target_for_sender(&state, "runtime-old", Some("sender"), false)
                .expect("runtime endpoint compatibility"),
            "runtime-old"
        );
        assert_eq!(
            resolve_agent_target_for_sender(&state, "worker-new", Some("sender"), false)
                .expect("display-name compatibility"),
            "runtime-new"
        );
        let mut projected = state
            .lock()
            .expect("state")
            .agents
            .values()
            .cloned()
            .collect::<Vec<_>>();
        project_current_durable_session_ids(&mut projected, &sessions);
        assert_eq!(
            projected
                .iter()
                .find(|agent| agent.id == "runtime-new")
                .and_then(|agent| agent.cutex_session_id.as_deref()),
            Some(durable_id)
        );
        assert!(projected
            .iter()
            .find(|agent| agent.id == "runtime-old")
            .expect("old endpoint")
            .cutex_session_id
            .is_none());
        let queued = crate::agent_bus::queue::enqueue_agent_bus_message_once(
            &state,
            "sender",
            "runtime-new",
            "worker-new",
            "one durable-target message",
            AgentBusEnvelopeKind::Message,
            AgentDeliveryMode::AfterTurn,
            AgentMessageKind::Agent,
            None,
            None,
            None,
            None,
            None,
            Some("durable-target-send-1".to_string()),
            1,
        )
        .expect("queue exactly one resolved send");
        assert!(!queued.deduplicated);
        assert_eq!(
            state
                .lock()
                .expect("state")
                .messages
                .get("runtime-new")
                .expect("target queue")
                .len(),
            1
        );
    }

    #[test]
    fn durable_session_offline_unknown_hidden_and_ambiguous_fail_closed() {
        let durable_id = "cutex.018e0000-0000-7000-8000-0000a000037e";
        let unknown_id = "cutex.018e0000-0000-7000-8000-0000a000037f";
        let native_session_id = "018e0000-0000-7000-8000-0000a000037e";
        let mut state = AgentBusState::default();
        state.agents.insert(
            "sender".to_string(),
            sample_agent("sender", "sender", Some("sender"), None, &["alpha"]),
        );
        state.agents.insert(
            "runtime-current".to_string(),
            sample_agent(
                "runtime-current",
                "worker",
                Some("worker"),
                Some(native_session_id),
                &["beta"],
            ),
        );
        let state = Arc::new(Mutex::new(state));
        let mut sessions = CutexSessionStore::default();
        sessions.sessions.insert(
            "primary".to_string(),
            active_session(durable_id, native_session_id, Some("runtime-current"), 1),
        );

        assert_eq!(
            target_code(
                resolve_agent_target_for_sender_with_sessions(
                    &state,
                    durable_id,
                    Some("sender"),
                    false,
                    Some(&sessions),
                )
                .expect_err("hidden target must not resolve"),
            ),
            AgentTargetResolutionCode::NotFound
        );
        assert_eq!(
            resolve_agent_target_for_sender_with_sessions(
                &state,
                durable_id,
                Some("sender"),
                true,
                Some(&sessions),
            )
            .expect("all-groups target"),
            "runtime-current"
        );

        sessions
            .sessions
            .get_mut("primary")
            .expect("primary")
            .current_runtime_agent_id = None;
        assert_eq!(
            target_code(
                resolve_agent_target_for_sender_with_sessions(
                    &state,
                    durable_id,
                    Some("sender"),
                    true,
                    Some(&sessions),
                )
                .expect_err("offline target must not resolve"),
            ),
            AgentTargetResolutionCode::TargetUnavailable
        );
        assert_eq!(
            target_code(
                resolve_agent_target_for_sender_with_sessions(
                    &state,
                    unknown_id,
                    Some("sender"),
                    true,
                    Some(&sessions),
                )
                .expect_err("unknown target must not resolve"),
            ),
            AgentTargetResolutionCode::NotFound
        );

        sessions.sessions.insert(
            "collision".to_string(),
            active_session(durable_id, native_session_id, Some("runtime-current"), 2),
        );
        assert_eq!(
            target_code(
                resolve_agent_target_for_sender_with_sessions(
                    &state,
                    durable_id,
                    Some("sender"),
                    true,
                    Some(&sessions),
                )
                .expect_err("ambiguous durable records must not resolve"),
            ),
            AgentTargetResolutionCode::Ambiguous
        );
        assert!(state.lock().expect("state").messages.is_empty());
    }

    #[test]
    fn projected_peer_durable_identity_preserves_group_scope_and_ambiguity() {
        let durable_id = "cutex.018e0000-0000-7000-8000-0000a000037e";
        let mut visible = sample_agent(
            "remote-current",
            "worker",
            Some("worker"),
            Some("native-session"),
            &["alpha"],
        );
        visible.cutex_session_id = Some(durable_id.to_string());
        let mut hidden = sample_agent(
            "remote-hidden",
            "hidden",
            Some("hidden"),
            Some("native-hidden"),
            &["beta"],
        );
        hidden.cutex_session_id = Some(durable_id.to_string());
        let sender_groups = vec!["alpha".to_string()];

        assert_eq!(
            resolve_agent_target_from_agent_list(
                &[visible.clone(), hidden.clone()],
                durable_id,
                Some(&sender_groups),
                false,
            )
            .expect("same-group peer durable target")
            .id,
            "remote-current"
        );
        assert_eq!(
            target_code(
                resolve_agent_target_from_agent_list(
                    &[visible, hidden],
                    durable_id,
                    Some(&sender_groups),
                    true,
                )
                .expect_err("multiple peer endpoints must be ambiguous"),
            ),
            AgentTargetResolutionCode::Ambiguous
        );
    }
}
