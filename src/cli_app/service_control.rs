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
    /// The unit a managed install recorded (`install.json`; drill names in
    /// debug end-to-end tests), else the standard name.
    #[cfg(target_os = "linux")]
    fn managed_unit(self) -> String {
        let names = supervisor::managed_layout()
            .and_then(|layout| layout.read_install().ok())
            .map(|record| cutex::upgrade::service::ServiceNames::of(&record));
        match (self, names) {
            (Self::Bus, Some(names)) => names.bus_unit,
            (Self::Management, Some(names)) => names.management_unit,
            _ => self.unit().to_string(),
        }
    }
}

/// A managed install's bus unit written before r248 has no `KillMode`, so
/// stopping it kills the agent runtimes in its cgroup: patch it first
/// (`upgrade::service::keep_bus_children`) and reload, as the upgrade runner does.
#[cfg(target_os = "linux")]
fn keep_bus_children_before_stop(unit: &str) -> anyhow::Result<()> {
    let Some(record) = supervisor::managed_layout().and_then(|layout| layout.read_install().ok()) else { return Ok(()) };
    let Some(dir) = record.unit_dir.map(std::path::PathBuf::from).or_else(cutex::upgrade::service::default_unit_dir) else {
        return Ok(());
    };
    if cutex::upgrade::service::keep_bus_children(&dir, unit)? {
        let status = Command::new("systemctl").args(["--user", "daemon-reload"]).status().context("Cannot run systemctl --user")?;
        ensure!(status.success(), "systemctl --user daemon-reload failed after patching {unit}");
    }
    Ok(())
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
        if matches!(service, Service::Bus) && matches!(action, Action::Stop | Action::Restart) {
            keep_bus_children_before_stop(&service.managed_unit())?;
        }
        let mut command = Command::new("systemctl");
        command.args(["--user", verb, &service.managed_unit()]);
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
        if let Some(executable) = configured {
            let logs = cutex::config::paths::runtime_dir()?.join("services");
            return controller::run(executable, &logs, service, action, timeout);
        }
        // A managed install (`cutex service install`): its supervisor task.
        if let Some(layout) = supervisor::managed_layout() {
            return supervisor::run(&layout, service, action, timeout);
        }
        anyhow::bail!(
            "No Windows service manager found: run `cutex service install` (managed install), or start the runtime-host of a hand-deployed host"
        )
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    anyhow::bail!("Service controls currently support Linux and Windows")
}


#[cfg(any(windows, test))]
#[path = "service_controller.rs"]
mod controller;

#[cfg(any(windows, target_os = "linux", test))]
#[path = "service_supervisor.rs"]
mod supervisor;

#[cfg(any(windows, test))]
#[path = "runtime_host_controller.rs"]
mod runtime_host;
