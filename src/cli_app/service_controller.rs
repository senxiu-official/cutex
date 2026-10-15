//! One Windows lifecycle authority: delegate to TethysUNE's service controller.
use super::{Action, Service};
use anyhow::{ensure, Context};
use std::{path::Path, process::{Command, Stdio}, time::{Duration, Instant}};

fn service_id(service: Service) -> &'static str {
    match service { Service::Bus => "cutex-agent-serve", Service::Management => "cutex-management-serve" }
}

fn verb(action: Action) -> &'static str {
    match action { Action::Status => "status", Action::Start => "start", Action::Stop => "stop", Action::Restart => "restart" }
}

pub(super) fn run(executable: &str, logs: &Path, service: Service, action: Action, timeout: Duration) -> anyhow::Result<String> {
    // The host exposes Start only while stopped and Stop only while running.
    // Repeated user actions should return the current state without duplicating a runtime.
    if matches!(action, Action::Start | Action::Stop) {
        let current = invoke(executable, logs, service, Action::Status, timeout)?;
        let summary = summarize(&current, service, Action::Status)?;
        let value: serde_json::Value = serde_json::from_str(&current)?;
        if (action == Action::Stop && value["runtime_status"] == "stopped")
            || (action == Action::Start && value["runtime_status"] == "running"
                && value["health"] == "healthy" && value["terminal_session_id"].as_str().is_some()) {
            return Ok(summary);
        }
    }
    let result = invoke(executable, logs, service, action, timeout)?;
    summarize(&result, service, action)
}

fn invoke(executable: &str, logs: &Path, service: Service, action: Action, timeout: Duration) -> anyhow::Result<String> {
    std::fs::create_dir_all(logs)?;
    let prefix = format!("{}-control-{}", service_id(service), uuid::Uuid::new_v4());
    let stdout_path = logs.join(format!("{prefix}.json"));
    let stderr_path = logs.join(format!("{prefix}.log"));
    let mut command = Command::new(executable);
    command.args(["service", verb(action), service_id(service)])
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&stdout_path)?)
        .stderr(std::fs::File::create(&stderr_path)?);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command.spawn().with_context(|| format!(
        "Cannot run Windows service controller {executable}. Configure windows_service_controller in Cutex config.json and start the runtime host"))?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? { break status; }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("{} controller timed out; host action may still be running. Refresh status before retrying. Log: {}", service.name(), stderr_path.display());
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let errors = read_bounded(&stderr_path)?;
    ensure!(status.success(), "{} controller failed: {}. Start/check the runtime host. Log: {}", service.name(), errors.trim(), stderr_path.display());
    read_bounded(&stdout_path)
}

fn read_bounded(path: &Path) -> anyhow::Result<String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?.take(65537).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 65536, "Service controller output exceeds 64 KiB: {}", path.display());
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn summarize(text: &str, service: Service, action: Action) -> anyhow::Result<String> {
    let result: serde_json::Value = serde_json::from_str(text)?;
    ensure!(result["schema_version"] == 1 && result["service_id"] == service_id(service), "Service controller response identity mismatch");
    let expected_action = if action == Action::Status { serde_json::Value::Null } else { verb(action).into() };
    ensure!(result.get("action") == Some(&expected_action), "Service controller action mismatch");
    let state = result["runtime_status"].as_str().context("Missing runtime status")?;
    let health = result["health"].as_str().context("Missing health")?;
    if matches!(action, Action::Start | Action::Restart) {
        ensure!(state == "running" && health == "healthy" && result["terminal_session_id"].as_str().is_some(),
            "Runtime host has not confirmed a healthy hosted service: {state} / {health}");
    }
    if action == Action::Stop { ensure!(state == "stopped", "Service did not stop: {state}"); }
    let mut text = format!("{} · TethysUNE runtime host\nState: {state} · Health: {health}", service.name());
    if let Some(id) = result["terminal_session_id"].as_str() { text.push_str(&format!("\nHost session: {id}")); }
    if let Some(messages) = result["messages"].as_array() {
        for message in messages.iter().filter_map(|message| message.as_str()) { text.push('\n'); text.push_str(message); }
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn response() -> serde_json::Value {
        serde_json::json!({"schema_version":1,"service_id":"cutex-management-serve","action":"start","runtime_status":"running","health":"healthy","terminal_session_id":"host-terminal-2","messages":["started Agent Bus"]})
    }
    #[test]
    fn requires_confirmed_hosted_start() {
        let mut value = response();
        assert!(summarize(&value.to_string(), Service::Management, Action::Start).unwrap().contains("started Agent Bus"));
        value["terminal_session_id"] = serde_json::Value::Null;
        assert!(summarize(&value.to_string(), Service::Management, Action::Start).is_err());
        value = response(); value["health"] = "unhealthy".into();
        assert!(summarize(&value.to_string(), Service::Management, Action::Start).is_err());
    }
    #[test]
    fn rejects_wrong_service_action_and_failed_stop() {
        let mut value = response();
        assert!(summarize(&value.to_string(), Service::Bus, Action::Start).is_err());
        assert!(summarize(&value.to_string(), Service::Management, Action::Stop).is_err());
        value["action"] = "stop".into();
        assert!(summarize(&value.to_string(), Service::Management, Action::Stop).is_err());
        value["runtime_status"] = "stopped".into(); value["terminal_session_id"] = serde_json::Value::Null;
        assert!(summarize(&value.to_string(), Service::Management, Action::Stop).is_ok());
    }
    #[cfg(windows)]
    #[test]
    #[ignore = "explicit deployment validation: restarts installed Windows services"]
    fn installed_runtime_host_lifecycle() {
        for (service, action) in [(Service::Management, Action::Start), (Service::Management, Action::Restart),
            (Service::Bus, Action::Restart), (Service::Bus, Action::Stop), (Service::Management, Action::Start),
            (Service::Bus, Action::Status), (Service::Management, Action::Status)] {
            println!("{}", super::super::run(service, action).unwrap());
        }
    }
}
