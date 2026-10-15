//! Status, attachability, and runtime-label projection for durable sessions.

use crate::agent_bus::model::AgentBusAgent;
use crate::platform::host::current_host_name;
use crate::platform::process::process_is_running;
use crate::runtime::alden::CuteAldenSession;
use crate::session::model::CutexSessionRecord;
use crate::session::model::CutexSessionRuntimeBackend;
use crate::session::service::cutex_session_is_managed;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CutexSessionLifecycleState {
    Online,
    Stale,
    Offline,
}

impl CutexSessionLifecycleState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Stale => "stale",
            Self::Offline => "offline",
        }
    }
}

pub fn cutex_session_scope_label(record: &CutexSessionRecord) -> &'static str {
    if record.exposed_to_backend {
        "im"
    } else if cutex_session_is_managed(record) {
        "managed"
    } else {
        "local"
    }
}

pub fn cutex_session_status_label(
    record: &CutexSessionRecord,
    alden_sessions: &[CuteAldenSession],
) -> &'static str {
    cutex_session_status_label_with_agents(record, alden_sessions, &[])
}

pub fn cutex_session_status_label_with_agents(
    record: &CutexSessionRecord,
    alden_sessions: &[CuteAldenSession],
    live_agents: &[AgentBusAgent],
) -> &'static str {
    if cutex_session_is_attachable(record, alden_sessions) {
        "attachable"
    } else if cutex_session_has_live_managed_core(record, live_agents)
        || cutex_session_has_live_native_agent(record, live_agents)
    {
        "online"
    } else if record.current_runtime_agent_id.is_some() {
        "stale"
    } else {
        "offline"
    }
}

pub fn cutex_session_lifecycle_state_with_agents(
    record: &CutexSessionRecord,
    alden_sessions: &[CuteAldenSession],
    live_agents: &[AgentBusAgent],
) -> CutexSessionLifecycleState {
    if cutex_session_is_attachable(record, alden_sessions)
        || cutex_session_has_live_managed_core(record, live_agents)
        || cutex_session_has_live_native_agent(record, live_agents)
    {
        CutexSessionLifecycleState::Online
    } else if record.current_runtime_agent_id.is_some() {
        CutexSessionLifecycleState::Stale
    } else {
        CutexSessionLifecycleState::Offline
    }
}

pub fn cutex_session_has_live_managed_core(
    record: &CutexSessionRecord,
    live_agents: &[AgentBusAgent],
) -> bool {
    let Some(binding) = record.app_server_runtime.as_ref() else {
        return false;
    };
    live_agents.iter().any(|agent| {
        agent.pid == binding.pid
            && managed_runtime_agent_matches_record(record, agent)
            && agent_endpoint_is_live(agent)
    })
}

fn managed_runtime_agent_matches_record(
    record: &CutexSessionRecord,
    agent: &AgentBusAgent,
) -> bool {
    match record.current_runtime_agent_id.as_deref() {
        Some(runtime_agent_id) => agent.id == runtime_agent_id,
        None => record
            .codex_session_id
            .as_deref()
            .is_some_and(|session_id| agent.session_id.as_deref() == Some(session_id)),
    }
}

pub fn cutex_session_has_live_native_agent(
    record: &CutexSessionRecord,
    live_agents: &[AgentBusAgent],
) -> bool {
    if record.runtime_backend == CutexSessionRuntimeBackend::ClaudeSubscription {
        return record.runtime_pid.is_some() && live_agents.iter().any(|agent| {
            record.current_runtime_agent_id.as_deref() == Some(agent.id.as_str())
                && agent.cutex_session_id.as_deref() == Some(record.cutex_session_id.as_str())
                && record.app_server_thread_id() == agent.session_id.as_deref()
                && record.runtime_pid == Some(agent.pid) && agent_endpoint_is_live(agent)
        });
    }
    record.runtime_backend == CutexSessionRuntimeBackend::HostForeground
        && live_agents.iter().any(|agent| {
            native_agent_matches_record(record, agent) && agent_endpoint_is_live(agent)
        })
}

fn native_agent_matches_record(record: &CutexSessionRecord, agent: &AgentBusAgent) -> bool {
    record
        .codex_session_id
        .as_deref()
        .is_some_and(|session_id| agent.session_id.as_deref() == Some(session_id))
        || record
            .current_runtime_agent_id
            .as_deref()
            .is_some_and(|id| agent.id == id)
}

fn agent_endpoint_is_live(agent: &AgentBusAgent) -> bool {
    let local_host = current_host_name();
    let agent_host = agent.host_id.as_deref().unwrap_or("");
    let local_or_unknown = agent_host.trim().is_empty()
        || agent_host.eq_ignore_ascii_case("unknown")
        || agent_host.eq_ignore_ascii_case(local_host.trim());
    if local_or_unknown {
        process_is_running(agent.pid)
    } else {
        true
    }
}

pub fn cutex_session_is_attachable(
    record: &CutexSessionRecord,
    alden_sessions: &[CuteAldenSession],
) -> bool {
    let alden_live = record.alden_session_name.as_deref().is_some_and(|name| {
        alden_sessions
            .iter()
            .any(|session| session.name.as_deref() == Some(name))
    });
    let pid_live = record.alden_pid.is_some_and(process_is_running);
    record.runtime_backend == CutexSessionRuntimeBackend::CuteAlden && alden_live && pid_live
}

pub fn runtime_backend_short_label(backend: CutexSessionRuntimeBackend) -> &'static str {
    match backend {
        CutexSessionRuntimeBackend::Host => "host",
        CutexSessionRuntimeBackend::HostForeground => "native",
        CutexSessionRuntimeBackend::Docker => "docker",
        CutexSessionRuntimeBackend::CuteAlden => "alden",
        CutexSessionRuntimeBackend::ClaudeSubscription => "claude",
        CutexSessionRuntimeBackend::Future => "future",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::agent_bus::model::AgentRegistrationClass;
    use crate::session::model::{CutexAppServerRuntimeBinding, CutexAppServerTransport};

    fn record(backend: CutexSessionRuntimeBackend) -> CutexSessionRecord {
        let mut record = CutexSessionRecord::new_at(
            "cutex.lifecycle".to_string(),
            Some("019e-lifecycle".to_string()),
            "devbox".to_string(),
            "/tmp/lifecycle".to_string(),
            Some("orchid".to_string()),
            "2026-08-05T00:00:00Z".to_string(),
        )
        .expect("record");
        record.runtime_backend = backend;
        record
    }

    #[test]
    fn lifecycle_status_keeps_attachability_as_a_separate_capability() {
        let mut record = record(CutexSessionRuntimeBackend::CuteAlden);
        record.alden_session_name = Some("cutex.lifecycle.runtime".to_string());
        record.alden_pid = Some(std::process::id());
        let alden_sessions = vec![CuteAldenSession {
            pid: std::process::id(),
            name: record.alden_session_name.clone(),
        }];

        assert_eq!(
            cutex_session_status_label(&record, &alden_sessions),
            "attachable"
        );
        assert_eq!(
            cutex_session_lifecycle_state_with_agents(&record, &alden_sessions, &[]),
            CutexSessionLifecycleState::Online
        );
    }

    #[test]
    fn lifecycle_status_distinguishes_stale_and_offline_records() {
        let mut record = record(CutexSessionRuntimeBackend::Host);
        assert_eq!(
            cutex_session_lifecycle_state_with_agents(&record, &[], &[]),
            CutexSessionLifecycleState::Offline
        );

        record.current_runtime_agent_id = Some("cutex.lifecycle.runtime".to_string());
        assert_eq!(
            cutex_session_lifecycle_state_with_agents(&record, &[], &[]),
            CutexSessionLifecycleState::Stale
        );
    }

    #[test]
    fn live_managed_alden_core_is_online_when_its_tui_peer_is_detached() {
        let mut record = record(CutexSessionRuntimeBackend::CuteAlden);
        record.current_runtime_agent_id = Some("cutex.lifecycle.runtime".to_string());
        record.app_server_runtime = Some(CutexAppServerRuntimeBinding {
            transport: CutexAppServerTransport::UnixSocket,
            endpoint: "unix:///tmp/runtime/app.sock".to_string(),
            pid: std::process::id(),
            runtime_dir: "/tmp/runtime".to_string(),
            launched_profile: Some("orchid".to_string()),
            launched_profile_id: None,
            launch_profile_source: None,
            auth_token_path: None,
            diagnostic_journal_path: "/tmp/runtime/events.jsonl".to_string(),
            schema_version: "test".to_string(),
            schema_sha256: "hash".to_string(),
            started_at: "2026-08-08T00:00:00Z".to_string(),
        });
        let live_agents = vec![AgentBusAgent {
            id: "cutex.lifecycle.runtime".to_string(),
            name: "lifecycle.runtime".to_string(),
            base_name: Some("lifecycle".to_string()),
            thread_name: None,
            path_key: None,
            session_id: record.codex_session_id.clone(),
            cutex_session_id: None,
            profile: "orchid".to_string(),
            cwd: record.cwd.clone(),
            pid: std::process::id(),
            host_id: Some(crate::platform::host::current_host_name()),
            groups: Vec::new(),
            registration_class: AgentRegistrationClass::Persistent,
            last_seen_epoch_secs: 42,
        }];

        assert!(!cutex_session_is_attachable(&record, &[]));
        assert!(cutex_session_has_live_managed_core(&record, &live_agents));
        assert_eq!(
            cutex_session_lifecycle_state_with_agents(&record, &[], &live_agents),
            CutexSessionLifecycleState::Online
        );
        assert_eq!(
            cutex_session_status_label_with_agents(&record, &[], &live_agents),
            "online"
        );
    }
}
