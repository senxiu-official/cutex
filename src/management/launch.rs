//! Management v2 connection checks. Clients never start the service.

use crate::management::remote::management_api_healthy;
use crate::management::service::{management_api_token, validate_management_port};
use crate::profiles::model::CodezConfig;

const RESET: &str = "\x1b[0m";
const YELLOW: &str = "\x1b[33m";

pub fn require_management_api_running(config: &CodezConfig, port: u16) -> anyhow::Result<()> {
    validate_management_port(port)?;
    if management_api_healthy(port, management_api_token(config, None)) {
        return Ok(());
    }
    anyhow::bail!(
        "Cutex management service is unavailable on port {port}. Start it in another terminal: cutex management serve --port {port} --bind 127.0.0.1. If installed as a service, start that service instead."
    )
}

pub fn warn_management_api_unavailable(err: &anyhow::Error) {
    eprintln!("{YELLOW}warning:{RESET} cutex management v2 service unavailable: {err:#}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_service_returns_manual_start_instruction_without_starting_service() {
        // Keep the port reserved without accepting requests: the check must return
        // an actionable error and must not try to launch a competing server.
        let listener = (24000..=24999).find_map(|port| std::net::TcpListener::bind(("127.0.0.1", port)).ok()).expect("free management test port");
        let port = listener.local_addr().unwrap().port();
        let error = require_management_api_running(&CodezConfig::default(), port).unwrap_err();
        assert!(error.to_string().contains(&format!("cutex management serve --port {port}")));
        assert!(!error.to_string().contains("--token"));
    }
}
