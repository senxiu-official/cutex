use anyhow::anyhow;

use super::agent_bus_config;
use super::agent_bus_runtime;

use cutex::agent_bus::client::agent_bus_fetch_agents;
use cutex::agent_bus::model::AgentBusAgent;
use cutex::config::env::CUTEX_AGENT_ID_ENV_VAR;
use cutex::profiles::model::CodezConfig;

pub(crate) fn current_live_agent() -> anyhow::Result<AgentBusAgent> {
    let (_, agent, _) = current_live_agent_context()?;
    Ok(agent)
}

pub(crate) fn current_live_agent_context(
) -> anyhow::Result<(CodezConfig, AgentBusAgent, Vec<AgentBusAgent>)> {
    let config = agent_bus_config::ensure_agent_bus_config(true, None)?;
    agent_bus_runtime::ensure_agent_bus_running(&config, true)?;
    let current_agent_id = std::env::var(CUTEX_AGENT_ID_ENV_VAR)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "No current cutex agent is visible. Start cute-codex through `cutex --agent` or `cutex run <profile> --agent`."
            )
        })?;
    let agents = agent_bus_fetch_agents(&config)?;
    let agent = agents
        .iter()
        .find(|agent| agent.id == current_agent_id)
        .cloned()
        .ok_or_else(|| {
            anyhow!("Current cutex agent is not registered on the bus: {current_agent_id}")
        })?;
    Ok((config, agent, agents))
}

pub(crate) fn normalize_session_id(session_id: &str) -> anyhow::Result<String> {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        anyhow::bail!("Session id cannot be empty");
    }
    Ok(session_id.to_string())
}
