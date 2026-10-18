use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::Context;
#[cfg(unix)]
use std::os::unix::process::CommandExt;

use cutex::agent_bus::client::{agent_bus_healthy, agent_bus_update_agent_groups};
use cutex::agent_bus::model::AgentGroupUpdateMode;
use cutex::agent_bus::service::{
    agent_bus_health_url, agent_bus_port, validate_agent_bus_port, AGENT_BUS_BRIDGE_ID,
};
use cutex::config::paths::runtime_dir;
use cutex::config::store::load_codez_config;
use cutex::platform::command::command_exists_in_path;
use cutex::platform::host::current_host_name;
use cutex::profiles::model::CodezConfig;

pub(crate) fn ensure_agent_bus_running(
    config: &CodezConfig,
    register_handoff_if_healthy: bool,
) -> anyhow::Result<()> {
    let port = agent_bus_port(config);
    validate_agent_bus_port(port)?;
    cutex::config::instance::ensure_port_allowed(port)?;
    if agent_bus_healthy(port, config.agent_bus_token.as_deref()) {
        if register_handoff_if_healthy {
            register_agent_bus_handoff(port);
        }
        return Ok(());
    }
    // Under a service manager (`cutex service install` units / supervisor)
    // the bus is its own service: wait for it instead of starting an
    // unsupervised second one that would then hold its port.
    if std::env::var_os(cutex::upgrade::service_manager::SERVICE_MANAGER_ENV).is_some() {
        for _ in 0..600 {
            std::thread::sleep(Duration::from_millis(100));
            if agent_bus_healthy(port, config.agent_bus_token.as_deref()) {
                return Ok(());
            }
        }
        anyhow::bail!("cutex agent bus (service) did not become healthy on port {port}");
    }

    let exe = std::env::current_exe().context("Failed to resolve current cutex executable")?;
    let log_dir = runtime_dir()?;
    fs::create_dir_all(&log_dir)
        .with_context(|| format!("Failed to create runtime dir: {}", log_dir.display()))?;
    let log_path = log_dir.join("agent-bus.log");
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("Failed to open log file: {}", log_path.display()))?;
    let stderr = stdout.try_clone().context("Failed to clone bus log file")?;

    let mut child = Command::new(exe);
    child
        .arg("agent")
        .arg("serve")
        .arg("--port")
        .arg(port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    // Never on the command line (`ps` shows it to every local user): the
    // child reads it from its environment (owner-only) and clears it.
    if let Some(token) = config.agent_bus_token.as_ref() {
        child.env(cutex::agent_bus::service::AGENT_BUS_SERVE_TOKEN_ENV, token);
    } else {
        child.env_remove(cutex::agent_bus::service::AGENT_BUS_SERVE_TOKEN_ENV);
    }
    #[cfg(unix)]
    unsafe {
        child.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    child
        .spawn()
        .with_context(|| format!("Failed to start cutex agent bus on port {port}"))?;

    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(100));
        if agent_bus_healthy(port, config.agent_bus_token.as_deref()) {
            register_agent_bus_handoff(port);
            return Ok(());
        }
    }

    anyhow::bail!(
        "cutex agent bus did not become healthy on port {port}. See {}",
        log_path.display()
    )
}

pub(crate) fn maybe_patch_live_agent_groups(
    target: &str,
    groups: &[String],
    mode: AgentGroupUpdateMode,
) -> anyhow::Result<Option<String>> {
    let config = load_codez_config();
    let port = agent_bus_port(&config);
    if !agent_bus_healthy(port, config.agent_bus_token.as_deref()) {
        return Ok(None);
    }
    let response = agent_bus_update_agent_groups(&config, target, groups, mode)?;
    Ok(response.agent_id)
}

pub(crate) fn register_agent_bus_handoff(port: u16) {
    // Bridgeboard handoffs are machine-wide: never from an instance or a
    // test process (test port guard).
    if cutex::config::instance::uses_isolated_ports() {
        return;
    }
    if !command_exists_in_path("bridgeboard") {
        return;
    }
    let owner_host = bridgeboard_owner_host_name();
    register_agent_bus_handoff_id(AGENT_BUS_BRIDGE_ID, &owner_host, port);
}

fn register_agent_bus_handoff_id(id: &str, owner_host: &str, port: u16) {
    let id = id.to_string();
    let owner_host = owner_host.to_string();
    std::thread::spawn(move || {
        let _ = Command::new("bridgeboard")
            .arg("handoff")
            .arg("--id")
            .arg(id)
            .arg("--title")
            .arg("cutex agent bus")
            .arg("--port")
            .arg(port.to_string())
            .arg("--owner-host")
            .arg(owner_host)
            .arg("--pid-from-port")
            .arg("--local-url")
            .arg(agent_bus_health_url(port))
            .arg("--open-url")
            .arg(agent_bus_health_url(port))
            .arg("--health-url")
            .arg(agent_bus_health_url(port))
            .arg("--tunnel-mode")
            .arg("local_forward")
            .arg("--require-healthy")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
}

fn bridgeboard_owner_host_name() -> String {
    current_host_name().to_ascii_lowercase()
}
