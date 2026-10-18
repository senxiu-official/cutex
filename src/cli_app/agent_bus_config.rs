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
    } else if cutex::config::instance::default_ports_forbidden() {
        // Test port guard: a test home gets its own bus port (kept once
        // stored), never the default bus of this machine.
        let configured = config
            .agent_bus_port
            .filter(|port| validate_agent_bus_port(*port).is_ok());
        config.agent_bus_port = Some(match configured {
            Some(port) => port,
            None => cutex::config::instance::allocate_isolated_port()?,
        });
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

#[cfg(test)]
mod tests {
    use super::*;
    use cutex::config::instance::RESERVED_DEFAULT_PORTS;

    #[test]
    fn test_home_bus_config_gets_its_own_port_never_the_default_bus() {
        let _home = super::super::test_home::IsolatedTestHome::new("bus-port-guard").unwrap();
        assert!(cutex::config::instance::default_ports_forbidden());

        // A stored default port (an old fixture) is replaced, not normalized to 24260.
        let mut fixture = load_codez_config();
        fixture.agent_bus_port = Some(DEFAULT_AGENT_BUS_PORT);
        save_codez_config(&fixture).unwrap();
        let config = ensure_agent_bus_config(true, None).unwrap();
        let port = config.agent_bus_port.expect("allocated bus port");
        assert!(!RESERVED_DEFAULT_PORTS.contains(&port), "{port}");
        assert_eq!(cutex::agent_bus::service::agent_bus_port(&config), port);

        // Kept once stored; the enabled switch is the caller's.
        let again = ensure_agent_bus_config(false, None).unwrap();
        assert_eq!(again.agent_bus_port, Some(port));
        assert!(!again.agent_bus_enabled);
        assert_eq!(load_codez_config().agent_bus_port, Some(port));

        // An explicit default port is refused.
        assert!(ensure_agent_bus_config(true, Some(DEFAULT_AGENT_BUS_PORT)).is_err());
        // The runtime refuses a default port before any probe or spawn.
        let mut live = config.clone();
        live.agent_bus_port = Some(DEFAULT_AGENT_BUS_PORT);
        assert!(super::super::agent_bus_runtime::ensure_agent_bus_running(&live, true).is_err());
    }
}
