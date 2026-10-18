//! `cutex service install | uninstall | status` for the new-install layout
//! (RFC §4.1, §4.3, §4.4) and the service control the upgrade runner uses.
//!
//! Linux: two systemd `--user` units, `cutex-agent-bus.service` and
//! `cutex-management-api.service` (the names `cutex human service` already
//! controls), `ExecStart=<base>/current/cutex …`, `KillMode=process` on the
//! Management API (native app-servers survive a restart: reload upgrades),
//! `Restart=on-failure`. Run `loginctl enable-linger` to keep them after
//! logout.
//!
//! Windows: no runtime-host for colleague installs (lead decision,
//! 2026-10-15). A per-user Task Scheduler task `\Cutex\cutex-supervisor`
//! (logon trigger for this user, least privilege, no time limit, restart
//! on failure) runs `<base>\bin\cutex.exe service run`, the supervisor in
//! [`super::supervisor`]. The desktop may start the same command when the
//! Management API does not answer (single instance by lock).
//!
//! Notifications: neither manager runs a separate `cutex notify run`. The
//! Management API of a managed install embeds the notification worker
//! (state collector + webhook outbox, `notify::outbound::spawn_embedded_worker`),
//! guarded by the same worker lock as `cutex notify run`, so a host that
//! also runs `cutex notify run` still delivers each notification once. An
//! existing managed install gets it with the next backend update (new
//! binary, same units / task); `status` reports its heartbeat.
//!
//! Safety: install refuses to overwrite a unit (or a task) it did not write
//! (marker line), and refuses on a host with a published runtime-host, so
//! the owner's hand-deployed hosts are never changed. With `--unit-dir`
//! (tests, staging) nothing is started and `systemctl` is never called.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, ensure, Context};
use serde_json::{json, Value};

use super::layout::{InstallRecord, Layout, CUTEX_EXE, FACADE_EXE, INSTALL_SCHEMA};

pub const BUS_UNIT: &str = "cutex-agent-bus.service";
pub const MANAGEMENT_UNIT: &str = "cutex-management-api.service";
pub const UNIT_MARKER: &str = "# Managed by `cutex service install`; edits are overwritten.";
pub const TASK_NAME: &str = "\\Cutex\\cutex-supervisor";
/// Debug builds only: install under drill names (`cutex-<label>-agent-bus.service`,
/// `cutex-<label>-management-api.service`, task `\\Cutex-<label>\\cutex-supervisor`)
/// so an end-to-end test on a developer machine never registers, starts or
/// stops the real service names. Release builds ignore it.
pub const TEST_SERVICE_NAME_ENV: &str = "CUTEX_SERVICE_TEST_NAME";

/// The service names an install uses (recorded in `install.json`; the
/// runner, `status` and `uninstall` read them from there).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceNames {
    pub bus_unit: String,
    pub management_unit: String,
    pub task_name: String,
}

impl Default for ServiceNames {
    fn default() -> Self {
        Self { bus_unit: BUS_UNIT.into(), management_unit: MANAGEMENT_UNIT.into(), task_name: TASK_NAME.into() }
    }
}

impl ServiceNames {
    pub fn for_test_label(label: &str) -> Self {
        Self {
            bus_unit: format!("cutex-{label}-agent-bus.service"),
            management_unit: format!("cutex-{label}-management-api.service"),
            task_name: format!("\\Cutex-{label}\\cutex-supervisor"),
        }
    }

    /// The names a new install gets: the real ones, or (debug builds only)
    /// the drill names of `CUTEX_SERVICE_TEST_NAME`.
    pub fn for_install() -> anyhow::Result<Self> {
        if cfg!(debug_assertions) {
            if let Some(label) = std::env::var(TEST_SERVICE_NAME_ENV).ok().filter(|v| !v.is_empty()) {
                ensure!(
                    super::layout::valid_label(&label) && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
                    "{TEST_SERVICE_NAME_ENV} must be [A-Za-z0-9-]"
                );
                return Ok(Self::for_test_label(&label));
            }
        }
        Ok(Self::default())
    }

    /// The names an existing install recorded (older records: the real ones).
    pub fn of(record: &InstallRecord) -> Self {
        let defaults = Self::default();
        let field = |key: &str, default: String| {
            record.extra.get(key).and_then(serde_json::Value::as_str).map(str::to_string).unwrap_or(default)
        };
        Self {
            bus_unit: field("busUnit", defaults.bus_unit),
            management_unit: field("managementUnit", defaults.management_unit),
            task_name: record.task_name.clone().unwrap_or(defaults.task_name),
        }
    }
}

/// Runs external commands (systemctl / schtasks); a fake in tests.
pub trait Exec {
    fn run(&self, program: &str, args: &[String]) -> anyhow::Result<String>;
}

pub struct RealExec;

impl Exec for RealExec {
    fn run(&self, program: &str, args: &[String]) -> anyhow::Result<String> {
        let output = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .with_context(|| format!("cannot run {program}"))?;
        ensure!(
            output.status.success(),
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

/// systemd quoting for ExecStart / Environment values.
pub(crate) fn systemd_quote(value: &str) -> String {
    if value.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+=:@%".contains(c)) {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%"))
    }
}

/// `Environment=` lines every unit carries: the service manager, the
/// install base and, for an independent instance, its `CUTEX_HOME` (else the
/// services would run the default instance).
fn unit_environment(base: &Path, cutex_home: Option<&Path>) -> String {
    let mut lines = vec![
        format!("{}={}", super::service_manager::SERVICE_MANAGER_ENV, super::service_manager::SYSTEMD),
        format!("{}={}", super::layout::INSTALL_BASE_ENV, base.display()),
    ];
    if let Some(home) = cutex_home {
        lines.push(format!("{}={}", crate::config::instance::HOME_ENV, home.display()));
    }
    lines.iter().map(|line| format!("Environment={}\n", systemd_quote(line))).collect()
}

pub fn bus_unit(base: &Path) -> String {
    bus_unit_with(base, None)
}

pub fn bus_unit_with(base: &Path, cutex_home: Option<&Path>) -> String {
    let exe = base.join("current").join(CUTEX_EXE);
    format!(
        "{UNIT_MARKER}\n[Unit]\nDescription=cutex Agent Bus\n\n[Service]\nType=simple\n\
         {}ExecStart={} agent serve\nRestart=on-failure\nRestartSec=3\n\n\
         [Install]\nWantedBy=default.target\n",
        unit_environment(base, cutex_home),
        systemd_quote(&exe.display().to_string()),
    )
}

pub fn management_unit(base: &Path) -> String {
    management_unit_with(base, BUS_UNIT, None)
}

pub fn management_unit_with(base: &Path, bus: &str, cutex_home: Option<&Path>) -> String {
    let exe = base.join("current").join(CUTEX_EXE);
    format!(
        "{UNIT_MARKER}\n[Unit]\nDescription=cutex Management API\nWants={bus}\nAfter={bus}\n\n[Service]\nType=simple\n\
         {}ExecStart={} management serve\nRestart=on-failure\nRestartSec=3\n\
         # Native app-servers are children of the Management API and must\n\
         # survive its restart (reload upgrades re-adopt them).\nKillMode=process\nTimeoutStopSec=30\n\n\
         [Install]\nWantedBy=default.target\n",
        unit_environment(base, cutex_home),
        systemd_quote(&exe.display().to_string()),
    )
}

/// `CUTEX_HOME` of this process (an independent instance), if any.
fn instance_home() -> Option<PathBuf> {
    std::env::var_os(crate::config::instance::HOME_ENV).filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn xml_escape(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// Task Scheduler definition (UTF-16 when written; `schtasks /Create /XML`).
/// `user` = `DOMAIN\name` of the installing user: a logon trigger limited to
/// that user and an interactive token need no administrator rights.
pub fn task_xml(base: &Path, user: &str) -> String {
    let exe = base.join("bin").join(CUTEX_EXE);
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>cutex supervisor: keeps the cutex Agent Bus and Management API running ({marker})</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>5</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>999</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <Arguments>service run</Arguments>
      <WorkingDirectory>{base}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#,
        marker = xml_escape(UNIT_MARKER),
        user = xml_escape(user),
        exe = xml_escape(&exe.display().to_string()),
        base = xml_escape(&base.display().to_string()),
    )
}

fn utf16_with_bom(text: &str) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

/// Default systemd user unit directory.
pub fn default_unit_dir() -> Option<PathBuf> {
    let test = std::env::var_os("CUTEX_TEST_PRIVATE_HOME").is_some();
    if !test {
        if let Some(config) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()).map(PathBuf::from) {
            if config.is_absolute() {
                return Some(config.join("systemd/user"));
            }
        }
    }
    crate::config::paths::home_dir().map(|home| home.join(".config/systemd/user"))
}

#[derive(Clone, Debug)]
pub struct InstallOptions {
    pub base: PathBuf,
    /// Linux: where units go; `None` = the default user unit dir (then the
    /// units are enabled and started unless `no_start`).
    pub unit_dir: Option<PathBuf>,
    pub channel: String,
    pub no_start: bool,
    pub dry_run: bool,
    /// Overwrite units / a task this command did not write.
    pub force: bool,
    /// Directory holding the `cutex` + `cutex-mcp` to install (default: the
    /// running executable's directory).
    pub source_dir: PathBuf,
    /// Release label for that build.
    pub release: String,
    /// Unit / task names ([`ServiceNames::for_install`]).
    pub names: ServiceNames,
}

/// The label a build installs as: `r240`, else `dev-<commit7>`, else `dev`.
pub fn build_release_label() -> String {
    match (crate::build_info::RELEASE, crate::build_info::COMMIT) {
        (Some(release), _) if super::layout::valid_label(release) => release.to_string(),
        (_, Some(commit)) => format!("dev-{}", &commit[..commit.len().min(7)]),
        _ => "dev".into(),
    }
}

fn existing_foreign(path: &Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|text| !text.starts_with(UNIT_MARKER))
}

fn runtime_host_present() -> bool {
    std::env::var_os("LOCALAPPDATA")
        .map(|d| PathBuf::from(d).join("runtime-host").join("endpoint.json"))
        .is_some_and(|p| p.is_file())
}

fn current_windows_user() -> String {
    match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
        // An OpenSSH session on a workgroup machine reports USERDOMAIN=WORKGROUP,
        // which is no account domain; local accounts live under the computer name.
        (Ok(domain), Ok(user)) if !domain.is_empty() && !domain.eq_ignore_ascii_case("WORKGROUP") => format!("{domain}\\{user}"),
        (_, Ok(user)) => match std::env::var("COMPUTERNAME") {
            Ok(computer) if !computer.is_empty() => format!("{computer}\\{user}"),
            _ => user,
        },
        _ => String::new(),
    }
}

/// The SID in `whoami /user /fo csv /nh` output (`"host\user","S-1-5-…"`).
fn parse_whoami_sid(output: &str) -> Option<String> {
    let line = output.lines().find(|line| !line.trim().is_empty())?;
    let sid = line.rsplit(',').next()?.trim().trim_matches('"');
    (sid.starts_with("S-1-") && sid.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')).then(|| sid.to_string())
}

/// The task principal: the installing user's SID (unambiguous in every
/// session type), else `DOMAIN\user` from the environment.
fn windows_task_user(exec: &dyn Exec) -> String {
    exec.run("whoami", &args(&["/user", "/fo", "csv", "/nh"]))
        .ok()
        .and_then(|out| parse_whoami_sid(&out))
        .unwrap_or_else(current_windows_user)
}

/// Plan (and unless `dry_run`, perform) an install. Returns the actions.
pub fn install(options: &InstallOptions, exec: &dyn Exec) -> anyhow::Result<Vec<String>> {
    super::layout::ensure_label(&options.release, "release")?;
    let channel = crate::build_info::normalize_channel(&options.channel)
        .with_context(|| format!("unknown channel `{}` (release or main)", options.channel))?;
    let layout = Layout::at(&options.base);
    ensure!(options.base.is_absolute(), "install base must be absolute");
    let exe = options.source_dir.join(CUTEX_EXE);
    let facade = options.source_dir.join(FACADE_EXE);
    ensure!(exe.is_file(), "{} not found", exe.display());
    ensure!(facade.is_file(), "{} not found (cutex-mcp must be next to cutex)", facade.display());
    let release_dir = layout.release_dir(&options.release);
    let mut actions = Vec::new();
    let same_dir = options.source_dir.canonicalize().ok() == release_dir.canonicalize().ok();
    if !same_dir {
        actions.push(format!("copy {} and {} to {}", exe.display(), facade.display(), release_dir.display()));
    }
    let linux = cfg!(target_os = "linux");
    let windows = cfg!(windows);
    let macos = cfg!(target_os = "macos");
    ensure!(linux || windows || macos, "cutex service install supports Linux, macOS and Windows");
    let names = &options.names;
    let (bus_name, management_name, task_name) = (&names.bus_unit, &names.management_unit, &names.task_name);
    let default_units = options.unit_dir.is_none();
    let unit_dir = match &options.unit_dir {
        Some(dir) => dir.clone(),
        None if macos => super::launchd::default_agents_dir().context("no home directory for launchd agents")?,
        None => default_unit_dir().context("no home directory for systemd user units")?,
    };
    // macOS: launchd agents named after the units (`super::launchd`).
    let (bus_label, management_label) =
        (super::launchd::label_for_unit(bus_name), super::launchd::label_for_unit(management_name));
    let (bus_plist, management_plist) = (
        unit_dir.join(super::launchd::plist_file_name(&bus_label)),
        unit_dir.join(super::launchd::plist_file_name(&management_label)),
    );
    let uid = super::launchd::current_uid();
    if linux {
        for unit in [bus_name, management_name] {
            let path = unit_dir.join(unit);
            if existing_foreign(&path) && !options.force {
                bail!("{} exists and was not written by `cutex service install` (owner host?); refusing (use --force to replace it)", path.display());
            }
        }
        actions.push(format!("point {} at releases/{}", layout.current_link().display(), options.release));
        actions.push(format!("write {} and {}", unit_dir.join(bus_name).display(), unit_dir.join(management_name).display()));
        if default_units && !options.no_start {
            actions.push("systemctl --user daemon-reload".into());
            actions.push(format!("systemctl --user enable --now {bus_name} {management_name}"));
        }
    }
    if macos {
        for path in [&bus_plist, &management_plist] {
            if super::launchd::existing_foreign(path) && !options.force {
                bail!("{} exists and was not written by `cutex service install`; refusing (use --force to replace it)", path.display());
            }
        }
        actions.push(format!("point {} at releases/{}", layout.current_link().display(), options.release));
        actions.push(format!("write {} and {}", bus_plist.display(), management_plist.display()));
        if default_units && !options.no_start {
            actions.push(format!("launchctl bootout {}/{management_label} and {}/{bus_label} (if loaded)", super::launchd::domain(uid), super::launchd::domain(uid)));
            actions.push(format!("launchctl bootstrap {} {} {}", super::launchd::domain(uid), bus_plist.display(), management_plist.display()));
        }
    }
    if windows {
        if runtime_host_present() && !options.force {
            bail!("a runtime-host is installed for this user (owner host?); refusing (use --force)");
        }
        actions.push(format!("copy cutex.exe and cutex-mcp.exe to {}", layout.bin_dir().display()));
        actions.push(format!("schtasks /Create /TN {task_name} /XML {}", layout.supervisor_dir().join("cutex-supervisor.xml").display()));
        if !options.no_start {
            actions.push(format!("schtasks /Run /TN {task_name}"));
        }
    }
    actions.push(format!("write {}", layout.install_record_path().display()));
    if options.dry_run {
        return Ok(actions);
    }

    // Perform.
    if !same_dir {
        std::fs::create_dir_all(&release_dir)?;
        super::layout::copy_file_atomic(&exe, &release_dir.join(CUTEX_EXE))?;
        super::layout::copy_file_atomic(&facade, &release_dir.join(FACADE_EXE))?;
        for extra in [super::stage::MANIFEST_FILE, "manifest.json.minisig", "release-receipt.json"] {
            let source = options.source_dir.join(extra);
            if source.is_file() {
                super::layout::copy_file_atomic(&source, &release_dir.join(extra))?;
            }
        }
    }
    // The release file record (integrity.rs): rollback / runner starts of
    // this release check its executable against it.
    let manifest_bytes = std::fs::read(release_dir.join(super::stage::MANIFEST_FILE)).ok();
    super::integrity::record(
        &layout,
        &options.release,
        super::integrity::SOURCE_SERVICE_INSTALL,
        super::integrity::file_hashes(&layout, &options.release)?,
        manifest_bytes.as_deref(),
        None,
    )?;
    let previous = layout.read_install().ok();
    if linux {
        #[cfg(unix)]
        super::layout::point_current(&layout, &options.release)?;
        std::fs::create_dir_all(&unit_dir)?;
        let home = instance_home();
        crate::config::atomic::write_bytes_atomic(&unit_dir.join(bus_name), bus_unit_with(&options.base, home.as_deref()).as_bytes())?;
        crate::config::atomic::write_bytes_atomic(
            &unit_dir.join(management_name),
            management_unit_with(&options.base, bus_name, home.as_deref()).as_bytes(),
        )?;
    }
    if macos {
        #[cfg(unix)]
        super::layout::point_current(&layout, &options.release)?;
        super::launchd::write_agents(&unit_dir, &options.base, names, instance_home().as_deref(), options.force)?;
    }
    if windows {
        super::layout::copy_file_atomic(&release_dir.join(CUTEX_EXE), &layout.bin_dir().join(CUTEX_EXE))?;
        super::layout::copy_file_atomic(&release_dir.join(FACADE_EXE), &layout.bin_dir().join(FACADE_EXE))?;
    }
    let record = InstallRecord {
        schema: INSTALL_SCHEMA.into(),
        os: std::env::consts::OS.into(),
        service_manager: if linux {
            super::service_manager::SYSTEMD
        } else if macos {
            super::service_manager::LAUNCHD
        } else {
            super::service_manager::SUPERVISOR
        }
        .into(),
        channel: channel.into(),
        created_at: previous.as_ref().map(|p| p.created_at.clone()).unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
        active_release: options.release.clone(),
        previous_release: previous.as_ref().map(|p| p.active_release.clone()).filter(|r| r != &options.release),
        unit_dir: (linux || macos).then(|| unit_dir.display().to_string()),
        task_name: windows.then(|| task_name.clone()),
        extra: {
            let mut extra = previous.map(|p| p.extra).unwrap_or_default();
            if linux || macos {
                extra.insert("busUnit".into(), json!(bus_name));
                extra.insert("managementUnit".into(), json!(management_name));
            }
            extra
        },
    };
    layout.write_install(&record)?;
    if linux && default_units && !options.no_start {
        exec.run("systemctl", &args(&["--user", "daemon-reload"]))?;
        exec.run("systemctl", &args(&["--user", "enable", "--now", bus_name, management_name]))?;
    }
    if macos && default_units && !options.no_start {
        let control = super::launchd::LaunchdControl { exec, names: names.clone(), agents_dir: unit_dir.clone(), uid };
        control.stop_all()?;
        control.start_all()?;
    }
    if windows {
        let xml = layout.supervisor_dir().join("cutex-supervisor.xml");
        std::fs::create_dir_all(layout.supervisor_dir())?;
        std::fs::write(&xml, utf16_with_bom(&task_xml(&options.base, &windows_task_user(exec))))?;
        exec.run("schtasks", &[
            "/Create".into(), "/TN".into(), task_name.clone(), "/XML".into(), xml.display().to_string(), "/F".into(),
        ])?;
        if !options.no_start {
            exec.run("schtasks", &args(&["/Run", "/TN", task_name]))?;
        }
    }
    Ok(actions)
}

/// Linger hint (Linux): services stop at logout without it.
pub fn linger_hint(exec: &dyn Exec) -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let user = std::env::var("USER").ok()?;
    let out = exec.run("loginctl", &["show-user".into(), user.clone(), "-p".into(), "Linger".into()]).ok()?;
    (!out.contains("Linger=yes")).then(|| format!("run `loginctl enable-linger {user}` to keep cutex running after you log out"))
}

/// Remove the units / task this install wrote (never data: releases,
/// natives, state and ~/.cutex stay). The install record is renamed.
pub fn uninstall(layout: &Layout, dry_run: bool, exec: &dyn Exec) -> anyhow::Result<Vec<String>> {
    let record = layout.read_install().context("not a managed install")?;
    let names = ServiceNames::of(&record);
    let (bus_name, management_name, task_name) = (&names.bus_unit, &names.management_unit, &names.task_name);
    let mut actions = Vec::new();
    if record.os == "linux" {
        let unit_dir = record.unit_dir.as_ref().map(PathBuf::from).context("install record lacks unit_dir")?;
        let default = default_unit_dir().is_some_and(|d| d == unit_dir);
        // Component services (Job Service) first: they depend on the bus.
        actions.extend(crate::components::job_service::uninstall_unit(exec, dry_run)?);
        if default {
            actions.push(format!("systemctl --user disable --now {management_name} {bus_name}"));
        }
        for unit in [management_name, bus_name] {
            let path = unit_dir.join(unit);
            if path.exists() {
                ensure!(!existing_foreign(&path), "{} was not written by `cutex service install`; leaving it", path.display());
                actions.push(format!("remove {}", path.display()));
            }
        }
        if default {
            actions.push("systemctl --user daemon-reload".into());
        }
        if !dry_run {
            if default {
                exec.run("systemctl", &args(&["--user", "disable", "--now", management_name, bus_name]))?;
            }
            for unit in [management_name, bus_name] {
                let _ = std::fs::remove_file(unit_dir.join(unit));
            }
            if default {
                exec.run("systemctl", &args(&["--user", "daemon-reload"]))?;
            }
        }
    } else if record.os == "macos" {
        let agents_dir = record.unit_dir.as_ref().map(PathBuf::from).context("install record lacks unit_dir")?;
        let default = super::launchd::default_agents_dir().is_some_and(|d| d == agents_dir);
        let uid = super::launchd::current_uid();
        let plists: Vec<PathBuf> = [management_name, bus_name]
            .iter()
            .map(|unit| agents_dir.join(super::launchd::plist_file_name(&super::launchd::label_for_unit(unit))))
            .collect();
        if default {
            actions.push(format!(
                "launchctl bootout {0}/{1} and {0}/{2}",
                super::launchd::domain(uid),
                super::launchd::label_for_unit(management_name),
                super::launchd::label_for_unit(bus_name)
            ));
        }
        for path in &plists {
            if path.exists() {
                ensure!(!super::launchd::existing_foreign(path), "{} was not written by `cutex service install`; leaving it", path.display());
                actions.push(format!("remove {}", path.display()));
            }
        }
        if !dry_run {
            if default {
                super::launchd::LaunchdControl { exec, names: names.clone(), agents_dir: agents_dir.clone(), uid }.stop_all()?;
            }
            for path in &plists {
                let _ = std::fs::remove_file(path);
            }
        }
    } else {
        actions.push("ask the supervisor to stop the services and exit".into());
        actions.push(format!("schtasks /Delete /TN {task_name} /F"));
        if !dry_run {
            if super::supervisor::running(layout) {
                let seq = super::supervisor::request(layout, super::supervisor::Desired::Exit, "service uninstall")?;
                let _ = super::supervisor::wait_ack(layout, seq, Duration::from_secs(60));
            }
            exec.run("schtasks", &args(&["/Delete", "/TN", task_name, "/F"]))?;
        }
    }
    let aside = layout.base.join(format!("install.json.uninstalled-{}", chrono::Utc::now().format("%Y%m%dT%H%M%SZ")));
    actions.push(format!("rename install.json to {}", aside.display()));
    if !dry_run {
        std::fs::rename(layout.install_record_path(), aside)?;
    }
    Ok(actions)
}

/// `cutex service status` (and `/v2/upgrade` `service`).
pub fn status(layout: &Layout, exec: &dyn Exec) -> Value {
    let record = layout.read_install().ok();
    let mut value = json!({
        "base": layout.base.display().to_string(),
        "managed": record.is_some(),
        "install": record,
        "activeRelease": layout.active_release().ok(),
        "releases": layout.installed_releases(),
    });
    let Some(record) = record else { return value };
    // The notification worker runs inside the Management API of a managed
    // install (no unit / child of its own): report its heartbeat.
    value["notificationWorker"] = match crate::notify::outbound::status() {
        Ok(outbound) => json!({
            "embeddedInManagement": true,
            "running": outbound["running"],
            "heartbeat": outbound["heartbeat"],
            "webhookEnabled": outbound["config"]["enabled"],
            "counts": outbound["counts"],
            "lastError": outbound["lastError"],
        }),
        Err(error) => json!({"embeddedInManagement": true, "error": format!("{error:#}")}),
    };
    if record.service_manager == super::service_manager::SYSTEMD {
        let names = ServiceNames::of(&record);
        let mut units = serde_json::Map::new();
        for unit in [&names.bus_unit, &names.management_unit] {
            let show = exec
                .run("systemctl", &args(&["--user", "show", unit, "--property=LoadState,ActiveState,SubState,MainPID", "--no-pager"]))
                .unwrap_or_default();
            let mut fields = serde_json::Map::new();
            for line in show.lines() {
                if let Some((k, v)) = line.split_once('=') {
                    fields.insert(k.to_string(), json!(v));
                }
            }
            units.insert(unit.clone(), Value::Object(fields));
        }
        value["units"] = Value::Object(units);
    } else if record.service_manager == super::service_manager::LAUNCHD {
        let names = ServiceNames::of(&record);
        let uid = super::launchd::current_uid();
        let mut agents = serde_json::Map::new();
        for unit in [&names.bus_unit, &names.management_unit] {
            let label = super::launchd::label_for_unit(unit);
            let fields = match exec.run("launchctl", &super::launchd::print_args(uid, &label)) {
                Ok(text) => super::launchd::parse_print(&text),
                Err(_) => serde_json::Map::from_iter([("state".to_string(), json!("not loaded"))]),
            };
            agents.insert(label, Value::Object(fields));
        }
        value["launchdAgents"] = Value::Object(agents);
    } else {
        value["supervisor"] = json!({
            "running": super::supervisor::running(layout),
            "status": super::supervisor::read_status(layout),
        });
    }
    value
}

// ------------------------------------------------------- runner control

/// What the upgrade runner needs from the service manager.
pub trait ServiceControl {
    fn stop_all(&self) -> anyhow::Result<()>;
    fn start_all(&self) -> anyhow::Result<()>;
}

/// Controls the units the install record names (never the real unit names
/// of another install on the same machine).
pub struct SystemdControl<'a> {
    pub exec: &'a dyn Exec,
    pub names: ServiceNames,
}

impl ServiceControl for SystemdControl<'_> {
    fn stop_all(&self) -> anyhow::Result<()> {
        // Management first (it talks to the bus), then the bus.
        self.exec.run("systemctl", &args(&["--user", "stop", &self.names.management_unit]))?;
        self.exec.run("systemctl", &args(&["--user", "stop", &self.names.bus_unit]))?;
        Ok(())
    }
    fn start_all(&self) -> anyhow::Result<()> {
        self.exec.run("systemctl", &args(&["--user", "start", &self.names.bus_unit]))?;
        self.exec.run("systemctl", &args(&["--user", "start", &self.names.management_unit]))?;
        Ok(())
    }
}

pub struct SupervisorControl<'a> {
    pub layout: &'a Layout,
    pub timeout: Duration,
    /// The task the install registered (`install.json` `taskName`).
    pub task_name: String,
}

impl ServiceControl for SupervisorControl<'_> {
    fn stop_all(&self) -> anyhow::Result<()> {
        ensure!(super::supervisor::running(self.layout), "the cutex supervisor is not running");
        let seq = super::supervisor::request(self.layout, super::supervisor::Desired::Stopped, "upgrade")?;
        super::supervisor::wait_ack(self.layout, seq, self.timeout)?;
        Ok(())
    }
    fn start_all(&self) -> anyhow::Result<()> {
        if !super::supervisor::running(self.layout) {
            // Start the supervisor itself (the task normally does this).
            RealExec.run("schtasks", &args(&["/Run", "/TN", &self.task_name]))?;
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while !super::supervisor::running(self.layout) {
                ensure!(std::time::Instant::now() < deadline, "the cutex supervisor did not start");
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        let seq = super::supervisor::request(self.layout, super::supervisor::Desired::Running, "upgrade")?;
        super::supervisor::wait_ack(self.layout, seq, self.timeout)?;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct FakeExec {
        pub calls: Mutex<Vec<String>>,
        pub fail: Mutex<Option<String>>,
    }

    impl Exec for FakeExec {
        fn run(&self, program: &str, args: &[String]) -> anyhow::Result<String> {
            let line = format!("{program} {}", args.join(" "));
            self.calls.lock().unwrap().push(line.clone());
            if let Some(fail) = self.fail.lock().unwrap().as_ref() {
                if line.contains(fail.as_str()) {
                    bail!("fake failure: {line}");
                }
            }
            if line.contains("show") && line.contains("Linger") {
                return Ok("Linger=no\n".into());
            }
            Ok("ActiveState=active\nMainPID=42\n".into())
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::fake::FakeExec;
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cutex-service-{tag}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn build_dir() -> PathBuf {
        let dir = temp("build");
        std::fs::write(dir.join(CUTEX_EXE), b"#!/bin/sh\n").unwrap();
        std::fs::write(dir.join(FACADE_EXE), b"facade").unwrap();
        dir
    }

    fn options(base: &Path, unit_dir: Option<PathBuf>, source: &Path) -> InstallOptions {
        InstallOptions {
            base: base.to_path_buf(),
            unit_dir,
            channel: "release".into(),
            no_start: false,
            dry_run: false,
            force: false,
            source_dir: source.to_path_buf(),
            release: "r240".into(),
            names: ServiceNames::default(),
        }
    }

    #[test]
    fn units_quote_paths_and_keep_native_children() {
        let unit = management_unit(Path::new("/home/a b/.local/share/cutex"));
        assert!(unit.starts_with(UNIT_MARKER));
        assert!(unit.contains("ExecStart=\"/home/a b/.local/share/cutex/current/cutex\" management serve"), "{unit}");
        assert!(unit.contains("KillMode=process"));
        assert!(unit.contains("Environment=CUTEX_SERVICE_MANAGER=systemd"));
        assert!(unit.contains("Environment=\"CUTEX_INSTALL_BASE=/home/a b/.local/share/cutex\""));
        assert!(unit.contains("After=cutex-agent-bus.service"));
        let bus = bus_unit(Path::new("/srv/cutex"));
        assert!(bus.contains("ExecStart=/srv/cutex/current/cutex agent serve"));
        assert!(!bus.contains("KillMode"));
    }

    #[test]
    fn whoami_sid_is_parsed() {
        assert_eq!(
            parse_whoami_sid("\"winbox\\pat\",\"S-1-5-21-111-222-333-1001\"\r\n").as_deref(),
            Some("S-1-5-21-111-222-333-1001")
        );
        assert_eq!(parse_whoami_sid("ActiveState=active\n"), None);
        assert_eq!(parse_whoami_sid(""), None);
    }

    #[test]
    fn task_xml_is_per_user_and_escaped() {
        let xml = task_xml(Path::new("C:\\Users\\a&b\\AppData\\Local\\cutex"), "EVA\\a&b");
        assert!(xml.contains("<UserId>EVA\\a&amp;b</UserId>"));
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        // Normal priority (4-6): the default 7 runs cutex and every agent below normal.
        assert!(xml.contains("<Priority>5</Priority>"));
        assert!(xml.contains("<Arguments>service run</Arguments>"));
        assert!(xml.contains("<Command>") && xml.contains("AppData\\Local\\cutex"));
        let bytes = utf16_with_bom(&xml);
        assert_eq!(&bytes[..2], &[0xFF, 0xFE]);
    }

    #[test]
    fn install_into_a_temp_unit_dir_never_calls_systemctl() {
        let base = temp("base");
        let units = temp("units");
        let source = build_dir();
        let exec = FakeExec::default();
        let dry = install(&InstallOptions { dry_run: true, ..options(&base, Some(units.clone()), &source) }, &exec).unwrap();
        assert!(dry.iter().any(|a| a.contains("write") && a.contains(BUS_UNIT)));
        assert!(!Layout::at(&base).is_managed(), "dry run changes nothing");
        assert!(!units.join(BUS_UNIT).exists());
        install(&options(&base, Some(units.clone()), &source), &exec).unwrap();
        assert!(exec.calls.lock().unwrap().is_empty(), "{:?}", exec.calls.lock().unwrap());
        let layout = Layout::at(&base);
        assert!(layout.is_managed());
        assert_eq!(layout.active_release().unwrap(), "r240");
        assert!(layout.service_executable().is_file());
        let record = layout.read_install().unwrap();
        assert_eq!((record.service_manager.as_str(), record.channel.as_str()), ("systemd", "release"));
        assert!(std::fs::read_to_string(units.join(MANAGEMENT_UNIT)).unwrap().contains(&base.display().to_string()));
        // Re-install is idempotent.
        install(&options(&base, Some(units.clone()), &source), &exec).unwrap();
        // Status reads the units through systemctl show (fake).
        let status = status(&layout, &exec);
        assert_eq!(status["managed"], true);
        assert_eq!(status["activeRelease"], "r240");
        // Uninstall removes our units, keeps the release.
        uninstall(&layout, false, &exec).unwrap();
        assert!(!units.join(BUS_UNIT).exists());
        assert!(!layout.is_managed());
        assert!(layout.release_executable("r240").is_file());
        for dir in [base, units, source] {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn install_refuses_foreign_units() {
        let base = temp("base");
        let units = temp("units");
        let source = build_dir();
        std::fs::write(units.join(MANAGEMENT_UNIT), "[Service]\nExecStart=/owner/cutex management serve --port 24270\n").unwrap();
        let error = install(&options(&base, Some(units.clone()), &source), &FakeExec::default()).unwrap_err();
        assert!(error.to_string().contains("not written by"), "{error}");
        assert!(std::fs::read_to_string(units.join(MANAGEMENT_UNIT)).unwrap().contains("/owner/cutex"));
        assert!(!Layout::at(&base).is_managed());
        for dir in [base, units, source] {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn drill_names_are_recorded_and_controlled() {
        let base = temp("base");
        let units = temp("units");
        let source = build_dir();
        let exec = FakeExec::default();
        let names = ServiceNames::for_test_label("drill");
        install(&InstallOptions { names: names.clone(), ..options(&base, Some(units.clone()), &source) }, &exec).unwrap();
        assert!(!units.join(BUS_UNIT).exists() && !units.join(MANAGEMENT_UNIT).exists());
        let management = std::fs::read_to_string(units.join("cutex-drill-management-api.service")).unwrap();
        assert!(management.contains("After=cutex-drill-agent-bus.service"), "{management}");
        assert!(units.join("cutex-drill-agent-bus.service").is_file());
        let layout = Layout::at(&base);
        let recorded = ServiceNames::of(&layout.read_install().unwrap());
        assert_eq!(recorded.bus_unit, names.bus_unit);
        assert_eq!(recorded.management_unit, names.management_unit);
        let control = SystemdControl { exec: &exec, names: recorded };
        control.stop_all().unwrap();
        assert_eq!(
            *exec.calls.lock().unwrap(),
            vec![
                "systemctl --user stop cutex-drill-management-api.service".to_string(),
                "systemctl --user stop cutex-drill-agent-bus.service".to_string(),
            ]
        );
        let status = status(&layout, &exec);
        assert!(status["units"]["cutex-drill-agent-bus.service"].is_object());
        uninstall(&layout, false, &exec).unwrap();
        assert!(!units.join("cutex-drill-agent-bus.service").exists());
        // An older record without names means the real ones.
        let mut old = InstallOptions { names: ServiceNames::default(), ..options(&base, Some(units.clone()), &source) };
        old.release = "r241".into();
        install(&old, &exec).unwrap();
        let mut record = layout.read_install().unwrap();
        record.extra.clear();
        assert_eq!(ServiceNames::of(&record), ServiceNames::default());
        for dir in [base, units, source] {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn instance_units_carry_cutex_home() {
        let unit = management_unit_with(Path::new("/srv/cutex"), "cutex-x-agent-bus.service", Some(Path::new("/srv/inst")));
        assert!(unit.contains("Environment=CUTEX_HOME=/srv/inst"), "{unit}");
        assert!(unit.contains("Wants=cutex-x-agent-bus.service"));
        assert!(!management_unit(Path::new("/srv/cutex")).contains("CUTEX_HOME"));
    }

    #[test]
    fn systemd_control_order() {
        let exec = FakeExec::default();
        let control = SystemdControl { exec: &exec, names: ServiceNames::default() };
        control.stop_all().unwrap();
        control.start_all().unwrap();
        assert_eq!(
            *exec.calls.lock().unwrap(),
            vec![
                format!("systemctl --user stop {MANAGEMENT_UNIT}"),
                format!("systemctl --user stop {BUS_UNIT}"),
                format!("systemctl --user start {BUS_UNIT}"),
                format!("systemctl --user start {MANAGEMENT_UNIT}"),
            ]
        );
        assert_eq!(linger_hint(&exec).is_some(), std::env::var("USER").is_ok());
    }
}

/// macOS (launchd). With `--unit-dir` nothing is bootstrapped and launchctl
/// is never called; the default-dir plan is checked as a dry run only.
#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::fake::FakeExec;
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cutex-service-{tag}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn build_dir() -> PathBuf {
        let dir = temp("build");
        std::fs::write(dir.join(CUTEX_EXE), b"#!/bin/sh\n").unwrap();
        std::fs::write(dir.join(FACADE_EXE), b"facade").unwrap();
        dir
    }

    fn options(base: &Path, unit_dir: Option<PathBuf>, source: &Path) -> InstallOptions {
        InstallOptions {
            base: base.to_path_buf(),
            unit_dir,
            channel: "release".into(),
            no_start: false,
            dry_run: false,
            force: false,
            source_dir: source.to_path_buf(),
            release: "r240".into(),
            names: ServiceNames::default(),
        }
    }

    #[test]
    fn install_into_a_temp_agents_dir_writes_plists_and_never_calls_launchctl() {
        let base = temp("base");
        let agents = temp("agents");
        let source = build_dir();
        let exec = FakeExec::default();
        let dry = install(&InstallOptions { dry_run: true, ..options(&base, Some(agents.clone()), &source) }, &exec).unwrap();
        assert!(dry.iter().any(|a| a.contains("write") && a.contains("dev.cutex.agent-bus.plist")), "{dry:?}");
        assert!(!dry.iter().any(|a| a.contains("launchctl")));
        assert!(!Layout::at(&base).is_managed());
        install(&options(&base, Some(agents.clone()), &source), &exec).unwrap();
        assert!(exec.calls.lock().unwrap().is_empty(), "{:?}", exec.calls.lock().unwrap());
        let layout = Layout::at(&base);
        let record = layout.read_install().unwrap();
        assert_eq!(record.service_manager, "launchd");
        assert_eq!(record.os, "macos");
        assert_eq!(record.unit_dir.as_deref(), Some(agents.display().to_string().as_str()));
        assert_eq!(layout.active_release().unwrap(), "r240");
        let plist = std::fs::read_to_string(agents.join("dev.cutex.management-api.plist")).unwrap();
        assert!(plist.contains(&format!("<string>{}/current/cutex</string>", base.display())));
        assert!(agents.join("dev.cutex.agent-bus.plist").is_file());
        // Status asks launchctl print (fake).
        let status = status(&layout, &exec);
        assert!(status["launchdAgents"]["dev.cutex.agent-bus"].is_object(), "{status}");
        // Uninstall of a non-default agents dir removes only our plists, no bootout.
        exec.calls.lock().unwrap().clear();
        uninstall(&layout, false, &exec).unwrap();
        assert!(exec.calls.lock().unwrap().is_empty());
        assert!(!agents.join("dev.cutex.agent-bus.plist").exists());
        assert!(!layout.is_managed());
        for dir in [base, agents, source] {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn default_agents_dir_plan_bootstraps_in_the_gui_domain() {
        let base = temp("base");
        let source = build_dir();
        let exec = FakeExec::default();
        let plan = install(&InstallOptions { dry_run: true, ..options(&base, None, &source) }, &exec).unwrap();
        let uid = unsafe { libc::getuid() };
        assert!(plan.iter().any(|a| a.starts_with(&format!("launchctl bootstrap gui/{uid} ")) && a.contains("Library/LaunchAgents/dev.cutex.agent-bus.plist")), "{plan:?}");
        assert!(exec.calls.lock().unwrap().is_empty());
        assert!(!Layout::at(&base).is_managed());
        for dir in [base, source] {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn install_refuses_foreign_plists() {
        let base = temp("base");
        let agents = temp("agents");
        let source = build_dir();
        std::fs::write(agents.join("dev.cutex.management-api.plist"), "<?xml version=\"1.0\"?>\n<plist version=\"1.0\"><dict/></plist>\n").unwrap();
        let error = install(&options(&base, Some(agents.clone()), &source), &FakeExec::default()).unwrap_err();
        assert!(error.to_string().contains("not written by"), "{error}");
        assert!(!Layout::at(&base).is_managed());
        for dir in [base, agents, source] {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}
