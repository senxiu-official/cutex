//! Explicit Human service operations. Never invoked by runtime discovery.
use anyhow::{ensure, Context};
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action { Status, Start, Stop, Restart }

#[derive(Clone, Copy, Debug)]
pub(super) enum Service { Bus, Management }

impl Service {
    pub(super) fn name(self) -> &'static str {
        match self { Self::Bus => "Agent Bus", Self::Management => "Management API" }
    }
    fn unit(self) -> &'static str {
        match self { Self::Bus => "cutex-agent-bus.service", Self::Management => "cutex-management-api.service" }
    }
}

pub(super) fn run(service: Service, action: Action) -> anyhow::Result<String> {
    // These controls operate the default instance's machine-wide services.
    cutex::config::instance::refuse_in_instance("Service controls")?;
    if action == Action::Status { return run_inner(service, action); }
    let root = cutex::config::paths::runtime_dir()?.join("services");
    std::fs::create_dir_all(&root)?;
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true)
        .open(root.join(format!("{}.lock", service.unit())))?;
    fs2::FileExt::try_lock_exclusive(&lock).context("A service action is already running")?;
    run_inner(service, action)
}

fn run_inner(service: Service, action: Action) -> anyhow::Result<String> {
    #[cfg(target_os = "linux")]
    {
        let verb = match action { Action::Status => "show", Action::Start => "start", Action::Stop => "stop", Action::Restart => "restart" };
        let mut command = Command::new("systemctl");
        command.args(["--user", verb, service.unit()]);
        if action == Action::Status {
            command.args(["--property=LoadState,ActiveState,SubState,MainPID", "--no-pager"]);
        }
        let output = command.output().context("Cannot run systemctl --user")?;
        ensure!(output.status.success(), "{}: {}", service.name(), String::from_utf8_lossy(&output.stderr));
        if action != Action::Status { return run_inner(service, Action::Status); }
        Ok(format!("{}\n{}", service.name(), String::from_utf8_lossy(&output.stdout).trim()))
    }
    #[cfg(windows)]
    {
        let timeout = std::time::Duration::from_secs(120);
        let config = cutex::config::store::load_codez_config_checked()?;
        let configured = config.windows_service_controller
            .as_deref().filter(|value| !value.trim().is_empty());
        // The per-user runtime-host (HKCU Run key) is the Windows lifecycle
        // authority. TethysUNE's controller stays a fallback only when it is
        // configured explicitly and no runtime-host is published.
        if std::env::var_os("CUTEX_TEST_PRIVATE_HOME").is_none() {
            if let Some(endpoint) = runtime_host::endpoint()? {
                return runtime_host::run(&endpoint, service, action, timeout);
            }
        }
        let executable = configured.context(
            r"runtime-host is not running: start it with the command in HKCU\Software\Microsoft\Windows\CurrentVersion\Run (RuntimeHost)")?;
        let logs = cutex::config::paths::runtime_dir()?.join("services");
        controller::run(executable, &logs, service, action, timeout)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    anyhow::bail!("Service controls currently support Linux and Windows")
}


#[cfg(any(windows, test))]
#[path = "service_controller.rs"]
mod controller;

#[cfg(any(windows, test))]
#[path = "runtime_host_controller.rs"]
mod runtime_host;
