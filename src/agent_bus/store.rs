//! Agent bus in-memory state and registry persistence.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::anyhow;
use anyhow::Context;

use crate::agent_bus::model::AgentBusAgent;
use crate::agent_bus::model::AgentBusMessage;
use crate::agent_bus::model::AgentBusRecentSend;
use crate::agent_bus::model::AgentBusRegistry;
use crate::config::atomic::write_pretty_json_atomic;
use crate::config::paths::runtime_dir;
use crate::platform::host::current_host_name;
use crate::platform::now_epoch_secs;
use crate::platform::process::process_is_running;

pub const AGENT_BUS_DEDUPE_WINDOW_SECS: u64 = 30;
pub const AGENT_BUS_STALE_HEARTBEAT_SECS: u64 = 120;
pub const AGENT_BUS_PID_PRUNE_GRACE_SECS: u64 = 15;

#[derive(Debug, Default)]
pub struct AgentBusState {
    pub agents: HashMap<String, AgentBusAgent>,
    pub messages: HashMap<String, VecDeque<AgentBusMessage>>,
    pub recent_sends: HashMap<String, AgentBusRecentSend>,
    /// In-flight durable ordinary sends keyed by the ordinary dedupe key.
    ///
    /// Reserving the identity before the durable ledger write lets concurrent
    /// identical requests converge on one message id without making the
    /// in-memory queue authoritative.
    pub send_reservations: HashMap<String, (String, u64)>,
}

fn agent_bus_registry_path() -> anyhow::Result<PathBuf> {
    Ok(runtime_dir()?.join("agent-bus-registry.json"))
}

pub fn load_agent_bus_state_from_registry() -> anyhow::Result<AgentBusState> {
    let path = agent_bus_registry_path()?;
    if !path.exists() {
        return Ok(AgentBusState::default());
    }
    let data = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read agent bus registry: {}", path.display()))?;
    let mut registry: AgentBusRegistry = serde_json::from_str(&data)
        .with_context(|| format!("Failed to parse agent bus registry: {}", path.display()))?;
    let now = now_epoch_secs();
    for agent in registry.agents.values_mut() {
        agent.last_seen_epoch_secs = now;
    }
    Ok(AgentBusState {
        agents: registry.agents,
        messages: HashMap::new(),
        recent_sends: HashMap::new(),
        send_reservations: HashMap::new(),
    })
}

pub fn save_agent_bus_registry_locked(state: &AgentBusState) -> anyhow::Result<()> {
    let path = agent_bus_registry_path()?;
    let registry = AgentBusRegistry {
        agents: state.agents.clone(),
    };
    write_pretty_json_atomic(&path, &registry, "agent bus registry")
}

pub fn persist_agent_bus_registry(state: &Arc<Mutex<AgentBusState>>) -> anyhow::Result<()> {
    let state = state
        .lock()
        .map_err(|_| anyhow!("agent bus state lock poisoned"))?;
    save_agent_bus_registry_locked(&state)
}

pub fn prune_recent_agent_sends(state: &mut AgentBusState, now: u64) {
    state.recent_sends.retain(|_, recent| {
        now.saturating_sub(recent.created_at_epoch_secs) <= AGENT_BUS_DEDUPE_WINDOW_SECS
    });
    state.send_reservations.retain(|_, (_, created_at)| {
        now.saturating_sub(*created_at) <= AGENT_BUS_DEDUPE_WINDOW_SECS
    });
}

pub fn prune_stale_agents_with_checker(
    state: &Arc<Mutex<AgentBusState>>,
    now: u64,
    local_host: &str,
    process_is_running: impl Fn(u32) -> bool,
) -> anyhow::Result<bool> {
    let heartbeat_cutoff = now.saturating_sub(AGENT_BUS_STALE_HEARTBEAT_SECS);
    let pid_prune_cutoff = now.saturating_sub(AGENT_BUS_PID_PRUNE_GRACE_SECS);
    let mut state = state
        .lock()
        .map_err(|_| anyhow!("agent bus state lock poisoned"))?;
    let stale = state
        .agents
        .iter()
        .filter_map(|(id, agent)| {
            // Each host owns its local registry. Old
            // clients did not send `host_id`, so missing host metadata is
            // treated as a legacy local registration and may be pruned by pid.
            // Explicit foreign `host_id` records are preserved because their
            // pids are meaningful only on the peer host.
            let stale_by_heartbeat = agent.last_seen_epoch_secs < heartbeat_cutoff;
            let stale_by_pid = agent.last_seen_epoch_secs < pid_prune_cutoff
                && agent_is_local_to_bus(agent, local_host)
                && !process_is_running(agent.pid);
            (stale_by_heartbeat || stale_by_pid).then(|| id.clone())
        })
        .collect::<Vec<_>>();
    let changed = !stale.is_empty();
    for id in stale {
        state.agents.remove(&id);
        state.messages.remove(&id);
    }
    Ok(changed)
}

pub fn prune_stale_agents(state: &Arc<Mutex<AgentBusState>>) -> anyhow::Result<bool> {
    let now = now_epoch_secs();
    let local_host = current_host_name();
    prune_stale_agents_with_checker(state, now, &local_host, process_is_running)
}

pub fn agent_is_local_to_bus(agent: &AgentBusAgent, local_host: &str) -> bool {
    agent
        .host_id
        .as_deref()
        .map(|host_id| host_id.eq_ignore_ascii_case(local_host))
        .unwrap_or(true)
}

pub fn agent_endpoint_is_usable_for_this_host(agent: &AgentBusAgent) -> bool {
    let local_host = current_host_name();
    !agent_is_local_to_bus(agent, &local_host) || process_is_running(agent.pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_bus::model::AgentRegistrationClass;

    fn sample_agent(id: &str, host_id: Option<&str>, pid: u32, last_seen: u64) -> AgentBusAgent {
        AgentBusAgent {
            id: id.to_string(),
            name: id.to_string(),
            base_name: None,
            thread_name: None,
            path_key: None,
            session_id: None,
            cutex_session_id: None,
            profile: "orchid".to_string(),
            cwd: "/tmp".to_string(),
            pid,
            host_id: host_id.map(str::to_string),
            groups: Vec::new(),
            registration_class: AgentRegistrationClass::LocalOnly,
            last_seen_epoch_secs: last_seen,
        }
    }

    #[test]
    fn stale_pruning_preserves_foreign_host_pid_state() {
        let state = Arc::new(Mutex::new(AgentBusState::default()));
        {
            let mut locked = state.lock().expect("state lock should not be poisoned");
            locked.agents.insert(
                "remote".to_string(),
                sample_agent("remote", Some("winbox"), 4242, 1_000),
            );
        }

        let changed = prune_stale_agents_with_checker(&state, 1_010, "devbox", |_| false).unwrap();

        assert!(!changed);
        assert!(state
            .lock()
            .expect("state lock should not be poisoned")
            .agents
            .contains_key("remote"));
    }

    #[test]
    fn stale_pruning_removes_local_dead_pid() {
        let state = Arc::new(Mutex::new(AgentBusState::default()));
        {
            let mut locked = state.lock().expect("state lock should not be poisoned");
            locked.agents.insert(
                "local".to_string(),
                sample_agent("local", Some("devbox"), 4242, 1_000),
            );
        }

        let changed = prune_stale_agents_with_checker(
            &state,
            1_000 + AGENT_BUS_PID_PRUNE_GRACE_SECS + 1,
            "devbox",
            |_| false,
        )
        .unwrap();

        assert!(changed);
        assert!(!state
            .lock()
            .expect("state lock should not be poisoned")
            .agents
            .contains_key("local"));
    }

    #[test]
    fn stale_pruning_preserves_fresh_local_agent_when_pid_probe_fails() {
        let state = Arc::new(Mutex::new(AgentBusState::default()));
        {
            let mut locked = state.lock().expect("state lock should not be poisoned");
            locked.agents.insert(
                "local".to_string(),
                sample_agent("local", Some("devbox"), 4242, 1_000),
            );
        }

        let changed = prune_stale_agents_with_checker(&state, 1_001, "devbox", |_| false).unwrap();

        assert!(!changed);
        assert!(state
            .lock()
            .expect("state lock should not be poisoned")
            .agents
            .contains_key("local"));
    }
}
