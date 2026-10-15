//! Management API remote tunnel and blocking HTTP client helpers.

use std::process::Command;
use std::time::Duration;

use anyhow::Context;
use serde_json::Value;

use crate::http::client::http_json_request;
use crate::http::client::HttpJsonRequest;
use crate::management::service::management_base_url;
use crate::management::service::management_health_local_url;
use crate::management::service::validate_management_port;
use crate::management::service::MANAGEMENT_BRIDGE_ID;
use crate::platform::command::command_exists_in_path;

pub fn ensure_management_remote_tunnel(
    host: &str,
    service_id: &str,
    local_port: u16,
    remote_port: u16,
    token: Option<&str>,
) -> anyhow::Result<()> {
    crate::config::instance::refuse_in_instance("A Bridgeboard management tunnel")?;
    let host = host.trim();
    if host.is_empty() {
        anyhow::bail!("Remote host cannot be empty");
    }
    validate_management_port(local_port)?;
    validate_management_port(remote_port)?;
    let service_id = if service_id.trim().is_empty() {
        MANAGEMENT_BRIDGE_ID
    } else {
        service_id.trim()
    };
    if management_api_healthy(local_port, token) {
        return Ok(());
    }
    let fallback = raw_management_ssh_tunnel_command(host, local_port, remote_port);
    if !command_exists_in_path("bridgeboard") {
        anyhow::bail!("bridgeboard not found; start the management tunnel manually: {fallback}");
    }
    let status = Command::new("bridgeboard")
        .arg("up")
        .arg("--peer")
        .arg(host)
        .arg("--local-port")
        .arg(local_port.to_string())
        .arg(service_id)
        .status()
        .with_context(|| {
            format!(
                "Failed to run bridgeboard up --peer {host} --local-port {local_port} {service_id}"
            )
        })?;
    if !status.success() {
        anyhow::bail!(
            "bridgeboard could not bring up the management tunnel: {status}; SSH fallback: {fallback}"
        );
    }
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(250));
        if management_api_healthy(local_port, token) {
            return Ok(());
        }
    }
    anyhow::bail!(
        "management tunnel is not healthy yet: {}; SSH fallback: {fallback}",
        management_health_local_url(local_port)
    )
}

pub fn management_api_healthy(port: u16, token: Option<&str>) -> bool {
    management_http_json(&management_base_url(port), "GET", "/", token, None).is_ok()
}

pub fn raw_management_ssh_tunnel_command(host: &str, local_port: u16, remote_port: u16) -> String {
    format!("ssh -N -L 127.0.0.1:{local_port}:127.0.0.1:{remote_port} {host}")
}

pub fn management_http_json(
    base_url: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&[u8]>,
) -> anyhow::Result<Value> {
    management_http_json_with_timeout(base_url, method, path, token, body, Duration::from_secs(5))
}

pub fn management_http_json_with_timeout(
    base_url: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&[u8]>,
    timeout: Duration,
) -> anyhow::Result<Value> {
    let url = management_request_url(base_url, path);
    http_json_request(HttpJsonRequest {
        url: &url,
        method,
        token,
        body,
        timeout,
        invalid_url_context: &format!("Invalid management API URL: {url}"),
        only_http_message: "Only http:// management API URLs are supported",
        missing_host_message: &format!("Management API URL has no host: {url}"),
        connect_context: "Failed to connect cutex management API",
        read_context: "Failed to read management API response",
        non_success_prefix: "cutex management API returned non-success",
        parse_context: "Failed to parse management API JSON response",
        ok_text_as_null: true,
    })
}

// Join the configured base path, rather than URL::join (whose leading slash
// replaces a configured prefix, and whose // form can replace the authority).
// Only the separator is canonicalized; endpoint/query text is left intact.
fn management_request_url(base_url: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base_url.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

#[cfg(test)]
mod tests {
    use super::management_request_url;

    #[test]
    fn management_url_join_preserves_base_and_single_boundary_separator() {
        for base in ["http://127.0.0.1:12345", "http://127.0.0.1:12345/"] {
            for path in [
                "v2/agent-management/explicit-launch",
                "/v2/agent-management/explicit-launch",
            ] {
                assert_eq!(
                    management_request_url(base, path),
                    "http://127.0.0.1:12345/v2/agent-management/explicit-launch"
                );
            }
            assert_eq!(management_request_url(base, "/"), "http://127.0.0.1:12345/");
        }
        assert_eq!(
            management_request_url("http://127.0.0.1:12345/prefix/", "/v2/items?limit=1"),
            "http://127.0.0.1:12345/prefix/v2/items?limit=1"
        );
        assert_eq!(
            management_request_url("http://127.0.0.1:12345/", "//foreign.invalid/path"),
            "http://127.0.0.1:12345/foreign.invalid/path"
        );
    }
}
