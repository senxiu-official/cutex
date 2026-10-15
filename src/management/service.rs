//! Management API service address helpers.

use std::error::Error;
use std::fmt;
use std::net::IpAddr;

use crate::profiles::model::CodezConfig;

pub const DEFAULT_MANAGEMENT_PORT: u16 = 24270;
pub const DEFAULT_MANAGEMENT_REMOTE_TUNNEL_PORT: u16 = 24670;
pub const MANAGEMENT_BRIDGE_ID: &str = "cutex-management-api";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagementCredentialError {
    MissingRootToken,
    AgentBusTokenCollision,
}

impl fmt::Display for ManagementCredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingRootToken => formatter.write_str(
                "privileged Management administration requires a configured non-empty Management API root token",
            ),
            Self::AgentBusTokenCollision => formatter.write_str(
                "privileged Management administration requires a root token distinct from the Agent Bus token",
            ),
        }
    }
}

impl Error for ManagementCredentialError {}

/// Select the bearer for ordinary Management routes. An explicit CLI override
/// wins, followed by the dedicated Management credential, with the Agent Bus
/// bearer retained only as the legacy compatibility fallback.
pub fn management_api_token<'a>(
    config: &'a CodezConfig,
    explicit_override: Option<&'a str>,
) -> Option<&'a str> {
    nonempty_token(explicit_override)
        .or_else(|| {
            config
                .management_api_token
                .as_ref()
                .map(|token| token.as_str())
                .and_then(|token| nonempty_token(Some(token)))
        })
        .or_else(|| nonempty_token(config.agent_bus_token.as_deref()))
}

/// Resolve the dedicated Human/admin credential without falling back to the
/// ordinary Agent Bus bearer.
pub fn management_root_credential<'a>(
    config: &'a CodezConfig,
    explicit_override: Option<&'a str>,
) -> Result<&'a str, ManagementCredentialError> {
    let management_root_token = match explicit_override {
        Some(token) => nonempty_token(Some(token)),
        None => config
            .management_api_token
            .as_ref()
            .map(|token| token.as_str())
            .and_then(|token| nonempty_token(Some(token))),
    }
    .ok_or(ManagementCredentialError::MissingRootToken)?;
    let agent_bus_token = nonempty_token(config.agent_bus_token.as_deref());
    if agent_bus_token == Some(management_root_token) {
        return Err(ManagementCredentialError::AgentBusTokenCollision);
    }
    Ok(management_root_token)
}

fn nonempty_token(token: Option<&str>) -> Option<&str> {
    token.filter(|value| !value.trim().is_empty())
}

pub fn management_health_url(bind_addr: IpAddr, port: u16) -> String {
    let host = match bind_addr {
        IpAddr::V4(value) => value.to_string(),
        IpAddr::V6(value) => format!("[{value}]"),
    };
    format!("http://{host}:{port}/")
}

pub fn management_base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

pub fn management_health_local_url(port: u16) -> String {
    format!("{}/", management_base_url(port))
}

/// Port of this home's Management API (`management_port`, default 24270;
/// an instance without one gets 0, which fails closed).
pub fn management_port(config: &CodezConfig) -> u16 {
    config.management_port.unwrap_or(if crate::config::instance::is_isolated() {
        0
    } else {
        DEFAULT_MANAGEMENT_PORT
    })
}

pub fn validate_management_port(port: u16) -> anyhow::Result<()> {
    if crate::config::instance::is_isolated() {
        return crate::config::instance::validate_isolated_port(port, "Management API port");
    }
    if !(24000..=24999).contains(&port) {
        anyhow::bail!("Management API port must be in the Bridgeboard 24xxx range");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::model::ManagementApiToken;

    fn config(management: Option<&str>, agent_bus: Option<&str>) -> CodezConfig {
        CodezConfig {
            management_api_token: management.map(ManagementApiToken::new),
            agent_bus_token: agent_bus.map(str::to_string),
            ..CodezConfig::default()
        }
    }

    #[test]
    fn ordinary_management_prefers_override_then_dedicated_then_legacy() {
        let dedicated = config(Some("management-root"), Some("agent-bus-root"));
        assert_eq!(
            management_api_token(&dedicated, Some("manual-root")),
            Some("manual-root")
        );
        assert_eq!(
            management_api_token(&dedicated, None),
            Some("management-root")
        );
        assert_eq!(
            management_api_token(&config(None, Some("legacy-bus-root")), None),
            Some("legacy-bus-root")
        );
        assert_eq!(management_api_token(&config(None, None), None), None);
        assert_eq!(dedicated.agent_bus_token.as_deref(), Some("agent-bus-root"));
    }

    #[test]
    fn privileged_management_root_never_falls_back_to_agent_bus() {
        assert_eq!(
            management_root_credential(&config(Some("management-root"), Some("agent-bus")), None),
            Ok("management-root")
        );
        assert_eq!(
            management_root_credential(
                &config(None, Some("agent-bus-root")),
                Some("manual-management-root")
            ),
            Ok("manual-management-root"),
            "a manual override remains available"
        );
        assert_eq!(
            management_root_credential(&config(None, Some("agent-bus")), None),
            Err(ManagementCredentialError::MissingRootToken)
        );
        assert_eq!(
            management_root_credential(&config(Some("same"), Some("same")), None),
            Err(ManagementCredentialError::AgentBusTokenCollision)
        );
    }

    #[test]
    fn credential_errors_and_debug_output_are_secret_free() {
        let raw = "fixture-management-value-that-must-not-print";
        let collision = config(Some(raw), Some(raw));
        let error = management_root_credential(&collision, None)
            .expect_err("equal credentials must fail closed");
        let missing = management_root_credential(&config(None, Some(raw)), None)
            .expect_err("the Agent Bus bearer is never the root credential");
        let redacted = config(Some(raw), Some("different-agent-bus-value"));

        assert!(!error.to_string().contains(raw));
        assert!(!format!("{error:?}").contains(raw));
        assert!(!missing.to_string().contains(raw));
        assert!(!format!("{redacted:?}").contains(raw));
    }
}
