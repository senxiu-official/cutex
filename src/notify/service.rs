//! Desktop notification bridge service address and port helpers.

use anyhow::bail;

use crate::profiles::model::CodezConfig;

pub const DEFAULT_DESKTOP_NOTIFY_PORT: u16 = 24250;
pub const DESKTOP_NOTIFY_BRIDGE_ID: &str = "cutex-desktop-notify";

pub fn validate_desktop_notify_port(port: u16) -> anyhow::Result<()> {
    if !(24000..=24999).contains(&port) {
        bail!("Desktop notify bridge port must be in the Bridgeboard 24xxx range");
    }
    Ok(())
}

pub fn desktop_notify_port(config: &CodezConfig) -> u16 {
    config
        .desktop_notify_port
        .unwrap_or(DEFAULT_DESKTOP_NOTIFY_PORT)
}

pub fn desktop_notify_bridge_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/api/agent-notify/push")
}

pub fn desktop_notify_health_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/")
}
