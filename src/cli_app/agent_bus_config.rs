use uuid::Uuid;

use cutex::agent_bus::service::{validate_agent_bus_port, DEFAULT_AGENT_BUS_PORT};
use cutex::config::store::{load_codez_config, save_codez_config};
use cutex::profiles::model::CodezConfig;

pub(crate) fn ensure_agent_bus_config(
    enabled: bool,
    port: Option<u16>,
) -> anyhow::Result<CodezConfig> {
    let mut config = load_codez_config();
    config.agent_bus_enabled = enabled;
    if let Some(port) = port {
        validate_agent_bus_port(port)?;
        config.agent_bus_port = Some(port);
    } else if cutex::config::instance::is_isolated() {
        // An instance keeps its own configured bus port (required by the
        // instance preflight); never normalize it to the default bus.
        validate_agent_bus_port(cutex::agent_bus::service::agent_bus_port(&config))?;
    } else {
        // Federation keeps every host on its own local bus. Older configs could
        // point this value at an SSH tunnel; normalize back to the canonical
        // local bus unless the caller explicitly asks for a port.
        config.agent_bus_port = Some(DEFAULT_AGENT_BUS_PORT);
    }
    if config
        .agent_bus_token
        .as_ref()
        .is_none_or(|token| token.trim().is_empty())
    {
        config.agent_bus_token = Some(format!("cutex-agent-{}", Uuid::new_v4()));
    }
    save_codez_config(&config)?;
    Ok(config)
}
