//! Windows lifecycle through the per-user runtime-host (cutex-desktop
//! `services/runtime-host`), started from the HKCU Run key. It publishes a
//! loopback endpoint in `%LOCALAPPDATA%\runtime-host\endpoint.json` and a
//! bearer token in the owner-private file that endpoint names.
use super::{Action, Service};
use anyhow::{ensure, Context};
use std::path::PathBuf;
use std::time::Duration;

pub(super) fn service_id(service: Service) -> &'static str {
    match service {
        Service::Bus => "cutex-agent-serve",
        Service::Management => "cutex-management-serve",
    }
}

pub(super) struct Endpoint {
    url: String,
    token: String,
}

fn state_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("RUNTIME_HOST_STATE_DIR") {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join("runtime-host"))
}

/// The published host endpoint, or `None` when no runtime-host is installed/running.
pub(super) fn endpoint() -> anyhow::Result<Option<Endpoint>> {
    let Some(path) = state_dir().map(|dir| dir.join("endpoint.json")) else {
        return Ok(None);
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    let value: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;
    ensure!(value["schemaVersion"] == 1, "unsupported runtime-host endpoint schema");
    let url = value["url"].as_str().context("runtime-host endpoint url missing")?;
    ensure!(
        url.starts_with("http://127.0.0.1:"),
        "runtime-host endpoint must be loopback"
    );
    let token_file = value["tokenFile"].as_str().context("runtime-host token file missing")?;
    let token = std::fs::read_to_string(token_file)
        .context("cannot read runtime-host token")?
        .trim()
        .to_owned();
    Ok(Some(Endpoint { url: url.to_owned(), token }))
}

fn request(
    endpoint: &Endpoint,
    method: &str,
    path: &str,
    timeout: Duration,
) -> anyhow::Result<(u16, serde_json::Value)> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout + Duration::from_secs(15)))
        .max_redirects(0)
        .proxy(None)
        .http_status_as_error(false)
        .build()
        .new_agent();
    let url = format!("{}{path}", endpoint.url);
    let auth = format!("Bearer {}", endpoint.token);
    let response = match method {
        "POST" => agent.post(&url).header("Authorization", &auth).send_empty(),
        _ => agent.get(&url).header("Authorization", &auth).call(),
    };
    let mut response = response.map_err(|error| {
        anyhow::anyhow!("runtime-host at {} is not answering ({error}); start it from the Run key command", endpoint.url)
    })?;
    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .with_config()
        .limit(1 << 20)
        .read_json()
        .context("invalid runtime-host response")?;
    Ok((status, body))
}

pub(super) fn run(
    endpoint: &Endpoint,
    service: Service,
    action: Action,
    timeout: Duration,
) -> anyhow::Result<String> {
    let id = service_id(service);
    let (path, method) = match action {
        Action::Status => (format!("/v1/services/{id}"), "GET"),
        Action::Start => (format!("/v1/services/{id}/start?timeout={}", timeout.as_secs()), "POST"),
        Action::Stop => (format!("/v1/services/{id}/stop?timeout={}", timeout.as_secs()), "POST"),
        Action::Restart => (format!("/v1/services/{id}/restart?timeout={}", timeout.as_secs()), "POST"),
    };
    let (status, body) = request(endpoint, method, &path, timeout)?;
    summarize(service, status, &body)
}

fn summarize(service: Service, status: u16, body: &serde_json::Value) -> anyhow::Result<String> {
    let record = if body.get("service").is_some() { &body["service"] } else { body };
    if status != 200 {
        let error = body["error"].as_str().unwrap_or("runtime-host request failed");
        anyhow::bail!("{}: {error} (HTTP {status})", service.name());
    }
    ensure!(
        record["id"] == service_id(service),
        "runtime-host response identity mismatch"
    );
    let state = record["state"].as_str().context("missing service state")?;
    let health = record["health"].as_str().unwrap_or("unknown");
    let mut text = format!("{} · runtime-host\nState: {state} · Health: {health}", service.name());
    if let Some(pid) = record["pid"].as_u64() {
        text.push_str(&format!("\nPID: {pid}"));
        if record["adopted"] == true {
            text.push_str(" (adopted)");
        }
    }
    if let Some(started) = record["startedAt"].as_str() {
        text.push_str(&format!("\nStarted: {started}"));
    }
    if let Some(restarts) = record["restarts"].as_u64().filter(|r| *r > 0) {
        text.push_str(&format!("\nRestarts: {restarts}"));
    }
    if let Some(error) = record["lastError"].as_str() {
        text.push_str(&format!("\nLast error: {error}"));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> serde_json::Value {
        serde_json::json!({"ok": true, "service": {"id": "cutex-management-serve", "state": "running",
            "health": "healthy", "pid": 42, "adopted": false, "startedAt": "2026-10-06T00:00:00Z", "restarts": 1}})
    }

    #[test]
    fn summarizes_runtime_host_service() {
        let text = summarize(Service::Management, 200, &record()).unwrap();
        assert!(text.contains("State: running · Health: healthy"));
        assert!(text.contains("PID: 42"));
        assert!(text.contains("Restarts: 1"));
    }

    #[test]
    fn rejects_identity_mismatch_and_failures() {
        assert!(summarize(Service::Bus, 200, &record()).is_err());
        let failed = serde_json::json!({"ok": false, "error": "did not become healthy", "service": record()["service"]});
        let error = summarize(Service::Management, 504, &failed).unwrap_err().to_string();
        assert!(error.contains("did not become healthy"));
    }
}
