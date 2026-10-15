//! Stop-target planning for durable session runtimes.

use crate::agent_bus::model::AgentBusAgent;
use crate::agent_bus::store::agent_is_local_to_bus;
use crate::platform::process::process_is_running;
use crate::runtime::alden::CuteAldenSession;
use crate::session::model::CutexSessionRecord;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRuntimeStopTarget {
    pub had_runtime: bool,
    pub pid: Option<u32>,
    pub pids: Vec<u32>,
    pub alden_session_name: Option<String>,
    pub runtime_agent_id: Option<String>,
}

pub fn session_runtime_stop_target(
    record: &CutexSessionRecord,
    live_agents: &[AgentBusAgent],
    alden_session: Option<&CuteAldenSession>,
    local_host: &str,
) -> SessionRuntimeStopTarget {
    let mut pids = Vec::new();
    if let Some(session) = alden_session {
        pids.push(session.pid);
    }
    if let Some(pid) = record.alden_pid {
        pids.push(pid);
    }
    if let Some(pid) = record.runtime_pid {
        pids.push(pid);
    }
    if let Some(binding) = record.app_server_runtime.as_ref() {
        pids.push(binding.pid);
    }
    let codex_session_id = record.codex_session_id.as_deref();
    let current_runtime_agent_id = record.current_runtime_agent_id.as_deref();
    for agent in live_agents.iter().filter(|agent| {
        agent_is_local_to_bus(agent, local_host)
            && (agent.session_id.as_deref() == codex_session_id
                || current_runtime_agent_id == Some(agent.id.as_str()))
    }) {
        pids.push(agent.pid);
    }
    pids.sort_unstable();
    pids.dedup();
    pids.retain(|pid| *pid != 0);
    let pid = pids.first().copied();
    let pid_is_running = pids.iter().copied().any(process_is_running);
    let runtime_agent_id = live_agents
        .iter()
        .max_by(|left, right| {
            left.last_seen_epoch_secs
                .cmp(&right.last_seen_epoch_secs)
                .then_with(|| left.id.cmp(&right.id))
        })
        .map(|agent| agent.id.clone())
        .or_else(|| record.current_runtime_agent_id.clone());
    let app_server_launch_claim = record
        .app_server_launch_claim_id
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty());
    let had_runtime = !live_agents.is_empty()
        || alden_session.is_some()
        || pid_is_running
        || record.app_server_runtime.is_some()
        || app_server_launch_claim;

    SessionRuntimeStopTarget {
        had_runtime,
        pid,
        pids,
        alden_session_name: record.alden_session_name.clone(),
        runtime_agent_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offline_record() -> CutexSessionRecord {
        CutexSessionRecord::new_at(
            "cutex.runtime-stop".to_string(),
            Some("019f-runtime-stop".to_string()),
            "devbox".to_string(),
            "/tmp/runtime-stop".to_string(),
            None,
            "2026-08-08T00:00:00Z".to_string(),
        )
        .expect("session record")
    }

    #[test]
    fn legacy_pending_launch_id_is_not_an_app_server_stop_target() {
        let mut record = offline_record();
        record.pending_launch_id = Some("legacy-heartbeat-launch".to_string());

        let target = session_runtime_stop_target(&record, &[], None, "devbox");

        assert!(!target.had_runtime);
        assert!(target.pids.is_empty());
    }

    #[test]
    fn app_server_launch_claim_remains_a_stop_target_without_a_child_pid() {
        let mut record = offline_record();
        record.app_server_launch_claim_id = Some("app-server-launch-1".to_string());

        let target = session_runtime_stop_target(&record, &[], None, "devbox");

        assert!(target.had_runtime);
        assert!(target.pids.is_empty());
    }
}
