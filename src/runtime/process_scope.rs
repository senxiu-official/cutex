//! Per-managed-Agent process isolation for manager-owned runtime cores.
//!
//! On Linux with a usable systemd user manager, the app-server process is
//! launched through a transient scope. `systemd-run --scope` execs the target
//! in place, so the PID recorded by the durable runtime binding remains the
//! app-server PID while all tool/build descendants inherit the Agent cgroup.
//! On Windows (`windows_job_object:<name>`) the runtime is created suspended
//! inside its own named Job object with `KILL_ON_JOB_CLOSE`, whose last handle
//! lives in the runtime root itself (`platform::windows_job`): the runtime
//! survives management restarts like a scope does, its descendants die with
//! the root, and stop terminates the whole job.
//! Platforms without that boundary keep the existing direct launch and expose
//! a precise fallback reason in the child environment and service log. On
//! macOS (`direct:macos_process_group`) the runtime is started with setsid()
//! and the PID-based stop signals its whole process family
//! (`platform::process::terminate_process_and_wait`).

use std::fmt;
#[cfg(any(target_os = "linux", windows))]
use std::time::Duration;
#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::{Command, Stdio};
#[cfg(target_os = "linux")]
use std::thread;
#[cfg(target_os = "linux")]
use std::time::Instant;

#[cfg(target_os = "linux")]
use anyhow::Context;
use sha2::{Digest, Sha256};

use crate::agent_bus::identity::sanitize_session_component;
use crate::launch::command::LaunchCommand;
#[cfg(target_os = "linux")]
use crate::platform::command::command_exists_in_path;

pub const CUTEX_MANAGED_AGENT_ISOLATION_ENV_VAR: &str = "CUTEX_MANAGED_AGENT_ISOLATION";
pub const CUTEX_MANAGED_AGENT_SCOPE_UNIT_ENV_VAR: &str = "CUTEX_MANAGED_AGENT_SCOPE_UNIT";

const MANAGED_AGENT_SLICE: &str = "cutex-agents.slice";
/// Windows kernel object namespace of managed Agent jobs: `Global` so that a
/// stop issued from another logon session (SSH, service) finds the same job;
/// the job's DACL admits only the current user.
const WINDOWS_JOB_NAMESPACE: &str = "Global\\";
#[cfg(any(target_os = "linux", windows))]
const SCOPE_STOP_WAIT: Duration = Duration::from_secs(2);
#[cfg(target_os = "linux")]
const SCOPE_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedAgentIsolationFallback {
    /// macOS: no systemd; the runtime is its own session / process group
    /// leader and stop signals its whole family (platform::process).
    MacosProcessGroup,
    UnsupportedPlatform,
    SystemdNotBooted,
    CgroupV2Unavailable,
    SystemdUserManagerUnavailable,
    SystemdRunUnavailable,
    SystemctlUnavailable,
    SystemdRunMissingRequiredOptions,
}

impl ManagedAgentIsolationFallback {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MacosProcessGroup => "macos_process_group",
            Self::UnsupportedPlatform => "unsupported_platform",
            Self::SystemdNotBooted => "systemd_not_booted",
            Self::CgroupV2Unavailable => "cgroup_v2_unavailable",
            Self::SystemdUserManagerUnavailable => "systemd_user_manager_unavailable",
            Self::SystemdRunUnavailable => "systemd_run_unavailable",
            Self::SystemctlUnavailable => "systemctl_unavailable",
            Self::SystemdRunMissingRequiredOptions => "systemd_run_missing_required_options",
        }
    }
}

impl ManagedAgentProcessIsolation {
    /// Value of `CUTEX_MANAGED_AGENT_ISOLATION` in the runtime environment
    /// and of the service log line: `systemd_scope:<unit>`,
    /// `windows_job_object:<job>` or `direct:<reason>`.
    pub fn env_value(&self) -> String {
        match self {
            Self::SystemdScope { unit_name } => format!("systemd_scope:{unit_name}"),
            Self::WindowsJobObject { job_name } => format!("windows_job_object:{job_name}"),
            Self::Direct { reason } => format!("direct:{reason}"),
        }
    }
}

impl fmt::Display for ManagedAgentIsolationFallback {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedAgentProcessIsolation {
    SystemdScope {
        unit_name: String,
    },
    /// Windows: a named Job object per Agent (`platform::windows_job`).
    WindowsJobObject {
        job_name: String,
    },
    Direct {
        reason: ManagedAgentIsolationFallback,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedAgentLaunchPlan {
    pub launch: LaunchCommand,
    pub isolation: ManagedAgentProcessIsolation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedAgentScopeStopOutcome {
    pub found: bool,
    pub stopped: bool,
    pub forced: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SystemdScopeSupport {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Available,
    /// Windows: no systemd; a per-Agent Job object is the boundary.
    #[cfg_attr(not(windows), allow(dead_code))]
    WindowsJobObject,
    #[cfg_attr(windows, allow(dead_code))]
    Unavailable(ManagedAgentIsolationFallback),
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct SystemdScopeState {
    control_group: String,
    cgroup_path: PathBuf,
    populated: bool,
}

/// Stable, human-recognizable transient unit name for one durable Agent.
///
/// The readable component is diagnostic only. The SHA-256 prefix keeps two
/// long or similarly sanitized durable identities from sharing a stop target.
pub fn managed_agent_scope_unit_name(cutex_session_id: &str) -> String {
    scope_unit_name_for(crate::config::instance::current_name().as_deref(), cutex_session_id)
}

/// Slice of managed Agents; an independent instance (`CUTEX_HOME`) uses its own.
pub fn managed_agent_slice() -> String {
    slice_name_for(crate::config::instance::current_name().as_deref())
}

/// Stable per-Agent Windows Job object name (`Global\cutex-agent-…`), with
/// the same readable label, identity digest and instance qualification as
/// the Linux scope unit.
pub fn managed_agent_job_name(cutex_session_id: &str) -> String {
    job_name_for(crate::config::instance::current_name().as_deref(), cutex_session_id)
}

fn job_name_for(instance: Option<&str>, cutex_session_id: &str) -> String {
    let unit = scope_unit_name_for(instance, cutex_session_id);
    let base = unit.strip_suffix(".scope").unwrap_or(&unit);
    format!("{WINDOWS_JOB_NAMESPACE}{base}")
}

fn slice_name_for(instance: Option<&str>) -> String {
    match instance {
        Some(name) => format!("cutex-{name}-agents.slice"),
        None => MANAGED_AGENT_SLICE.to_string(),
    }
}

/// Instance names (`[a-z0-9-]`) qualify the unit so two instances never
/// share a stop target.
fn scope_unit_name_for(instance: Option<&str>, cutex_session_id: &str) -> String {
    let prefix = match instance {
        Some(name) => format!("cutex-{name}-agent"),
        None => "cutex-agent".to_string(),
    };
    let label = sanitize_session_component(cutex_session_id, 48, "session");
    let digest = Sha256::digest(cutex_session_id.as_bytes());
    let identity = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{prefix}-{label}-{identity}.scope")
}

pub fn managed_agent_launch_plan(
    cutex_session_id: &str,
    launch: LaunchCommand,
) -> ManagedAgentLaunchPlan {
    managed_agent_launch_plan_with_support(
        cutex_session_id,
        launch,
        current_systemd_scope_support(true),
    )
}

pub fn managed_agent_process_isolation(cutex_session_id: &str) -> ManagedAgentProcessIsolation {
    managed_agent_launch_plan(cutex_session_id, LaunchCommand::new("true")).isolation
}

fn managed_agent_launch_plan_with_support(
    cutex_session_id: &str,
    launch: LaunchCommand,
    support: SystemdScopeSupport,
) -> ManagedAgentLaunchPlan {
    match support {
        SystemdScopeSupport::Available => {
            let unit_name = managed_agent_scope_unit_name(cutex_session_id);
            let launch = launch
                .env_unset(CUTEX_MANAGED_AGENT_SCOPE_UNIT_ENV_VAR)
                .env(CUTEX_MANAGED_AGENT_SCOPE_UNIT_ENV_VAR, &unit_name)
                .env_unset(CUTEX_MANAGED_AGENT_ISOLATION_ENV_VAR)
                .env(
                    CUTEX_MANAGED_AGENT_ISOLATION_ENV_VAR,
                    format!("systemd_scope:{unit_name}"),
                );
            ManagedAgentLaunchPlan {
                launch: wrap_in_systemd_scope(launch, cutex_session_id, &unit_name),
                isolation: ManagedAgentProcessIsolation::SystemdScope { unit_name },
            }
        }
        SystemdScopeSupport::WindowsJobObject => {
            // The program is launched as is; the spawner creates the job and
            // assigns the suspended runtime to it (`spawn_managed_agent_runtime_launch`).
            let isolation = ManagedAgentProcessIsolation::WindowsJobObject {
                job_name: managed_agent_job_name(cutex_session_id),
            };
            ManagedAgentLaunchPlan {
                launch: launch
                    .env_unset(CUTEX_MANAGED_AGENT_SCOPE_UNIT_ENV_VAR)
                    .env_unset(CUTEX_MANAGED_AGENT_ISOLATION_ENV_VAR)
                    .env(CUTEX_MANAGED_AGENT_ISOLATION_ENV_VAR, isolation.env_value()),
                isolation,
            }
        }
        SystemdScopeSupport::Unavailable(reason) => {
            let isolation = ManagedAgentProcessIsolation::Direct { reason };
            ManagedAgentLaunchPlan {
                launch: launch
                    .env_unset(CUTEX_MANAGED_AGENT_SCOPE_UNIT_ENV_VAR)
                    .env_unset(CUTEX_MANAGED_AGENT_ISOLATION_ENV_VAR)
                    .env(CUTEX_MANAGED_AGENT_ISOLATION_ENV_VAR, isolation.env_value()),
                isolation,
            }
        }
    }
}

fn wrap_in_systemd_scope(
    launch: LaunchCommand,
    cutex_session_id: &str,
    unit_name: &str,
) -> LaunchCommand {
    let LaunchCommand {
        program,
        args,
        envs,
        env_removes,
    } = launch;
    let mut wrapped = LaunchCommand::new("systemd-run").args([
        "--user".to_string(),
        "--scope".to_string(),
        "--quiet".to_string(),
        "--collect".to_string(),
        "--expand-environment=no".to_string(),
        format!("--unit={unit_name}"),
        format!("--slice={}", managed_agent_slice()),
        "--property=KillMode=control-group".to_string(),
        format!("--description=Cutex managed Agent {cutex_session_id}"),
        "--".to_string(),
        program,
    ]);
    wrapped.args.extend(args);
    wrapped.envs = envs;
    wrapped.env_removes = env_removes;
    wrapped
}

/// Return the populated cgroup for this Agent's active scope, if any.
///
/// This is primarily an observability/test seam. Absence is not an error on a
/// platform using the documented direct fallback.
#[cfg(target_os = "linux")]
pub fn managed_agent_scope_control_group(cutex_session_id: &str) -> anyhow::Result<Option<String>> {
    match current_systemd_scope_support(false) {
        SystemdScopeSupport::Unavailable(_) | SystemdScopeSupport::WindowsJobObject => Ok(None),
        SystemdScopeSupport::Available => Ok(query_scope_state(&managed_agent_scope_unit_name(
            cutex_session_id,
        ))?
        .filter(|state| state.populated)
        .map(|state| state.control_group)),
    }
}

/// Authoritative absence check for recovery of a *known systemd scope*.
/// Unlike the launch fallback probe, unavailable systemd is not absence.
pub fn managed_agent_scope_is_absent(cutex_session_id: &str) -> anyhow::Result<bool> {
    #[cfg(target_os = "linux")]
    {
        match current_systemd_scope_support(false) {
            SystemdScopeSupport::Available => Ok(query_scope_state(
                &managed_agent_scope_unit_name(cutex_session_id),
            )?
            .is_none_or(|state| !state.populated)),
            SystemdScopeSupport::Unavailable(reason) => {
                anyhow::bail!("managed Agent scope observation unavailable: {reason}")
            }
            SystemdScopeSupport::WindowsJobObject => unreachable!("Linux never reports a Windows job"),
        }
    }
    #[cfg(windows)]
    {
        use crate::platform::windows_job;
        Ok(
            match windows_job::open_agent_job(&managed_agent_job_name(cutex_session_id))? {
                None => true,
                Some(job) => windows_job::active_processes(&job)? == 0,
            },
        )
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = cutex_session_id;
        anyhow::bail!("systemd scope recovery is unsupported on this platform")
    }
}

#[cfg(not(target_os = "linux"))]
pub fn managed_agent_scope_control_group(
    _cutex_session_id: &str,
) -> anyhow::Result<Option<String>> {
    Ok(None)
}

/// Gracefully terminate every process in one managed Agent scope, escalating
/// to SIGKILL only when the caller's existing `force` contract permits it.
///
/// A missing scope is normal for legacy, Windows, and non-systemd launches;
/// callers continue through the established PID-based lifecycle path.
#[cfg(target_os = "linux")]
pub fn terminate_managed_agent_scope(
    cutex_session_id: &str,
    force: bool,
) -> anyhow::Result<ManagedAgentScopeStopOutcome> {
    let support = current_systemd_scope_support(false);
    let SystemdScopeSupport::Available = support else {
        let SystemdScopeSupport::Unavailable(reason) = support else {
            unreachable!("Linux reports a systemd scope or a fallback")
        };
        return Ok(ManagedAgentScopeStopOutcome {
            found: false,
            stopped: true,
            forced: false,
            detail: format!("scope_fallback:{reason}"),
        });
    };

    let unit_name = managed_agent_scope_unit_name(cutex_session_id);
    let Some(state) = query_scope_state(&unit_name)? else {
        return Ok(ManagedAgentScopeStopOutcome {
            found: false,
            stopped: true,
            forced: false,
            detail: "scope_not_found".to_string(),
        });
    };
    if !state.populated {
        wait_for_scope_collection(&unit_name)?;
        return Ok(ManagedAgentScopeStopOutcome {
            found: false,
            stopped: true,
            forced: false,
            detail: "scope_empty".to_string(),
        });
    }

    signal_scope(&unit_name, "TERM", &state.cgroup_path)?;
    if wait_for_scope_empty(&state.cgroup_path, SCOPE_STOP_WAIT)? {
        wait_for_scope_collection(&unit_name)?;
        return Ok(ManagedAgentScopeStopOutcome {
            found: true,
            stopped: true,
            forced: false,
            detail: "scope_terminated".to_string(),
        });
    }
    if !force {
        return Ok(ManagedAgentScopeStopOutcome {
            found: true,
            stopped: false,
            forced: false,
            detail: "scope_terminate_timeout".to_string(),
        });
    }

    signal_scope(&unit_name, "KILL", &state.cgroup_path)?;
    let stopped = wait_for_scope_empty(&state.cgroup_path, SCOPE_STOP_WAIT)?;
    if stopped {
        wait_for_scope_collection(&unit_name)?;
    }
    Ok(ManagedAgentScopeStopOutcome {
        found: true,
        stopped,
        forced: true,
        detail: if stopped {
            "scope_force_killed".to_string()
        } else {
            "scope_force_kill_timeout".to_string()
        },
    })
}

/// Windows: terminate every process of the Agent's Job object.
///
/// Windows has no graceful signal for a process tree; the graceful part of a
/// stop (turn interrupt, manager disconnect) happened before this call, and
/// the PID path it replaces already ended in a forced kill. `forced` echoes
/// the caller's `force` so results read as on Linux.
#[cfg(windows)]
pub fn terminate_managed_agent_scope(
    cutex_session_id: &str,
    force: bool,
) -> anyhow::Result<ManagedAgentScopeStopOutcome> {
    use crate::platform::windows_job;
    let job_name = managed_agent_job_name(cutex_session_id);
    let Some(job) = windows_job::open_agent_job(&job_name)? else {
        return Ok(ManagedAgentScopeStopOutcome {
            found: false,
            stopped: true,
            forced: false,
            detail: "job_not_found".to_string(),
        });
    };
    if windows_job::active_processes(&job)? == 0 {
        return Ok(ManagedAgentScopeStopOutcome {
            found: false,
            stopped: true,
            forced: false,
            detail: "job_empty".to_string(),
        });
    }
    windows_job::terminate_job(&job)?;
    let stopped = windows_job::wait_for_job_empty(&job, SCOPE_STOP_WAIT)?;
    Ok(ManagedAgentScopeStopOutcome {
        found: true,
        stopped,
        forced: force,
        detail: if stopped {
            "job_terminated".to_string()
        } else {
            "job_terminate_timeout".to_string()
        },
    })
}

#[cfg(not(any(target_os = "linux", windows)))]
pub fn terminate_managed_agent_scope(
    _cutex_session_id: &str,
    _force: bool,
) -> anyhow::Result<ManagedAgentScopeStopOutcome> {
    let SystemdScopeSupport::Unavailable(reason) = current_systemd_scope_support(false) else {
        unreachable!("this platform exposes no process boundary")
    };
    Ok(ManagedAgentScopeStopOutcome {
        found: false,
        stopped: true,
        forced: false,
        detail: format!("scope_fallback:{reason}"),
    })
}

#[cfg(target_os = "linux")]
fn current_systemd_scope_support(require_launcher: bool) -> SystemdScopeSupport {
    if !Path::new("/run/systemd/system").is_dir() {
        return SystemdScopeSupport::Unavailable(ManagedAgentIsolationFallback::SystemdNotBooted);
    }
    if !Path::new("/sys/fs/cgroup/cgroup.controllers").is_file() {
        return SystemdScopeSupport::Unavailable(
            ManagedAgentIsolationFallback::CgroupV2Unavailable,
        );
    }
    if !systemd_user_manager_endpoint_exists() {
        return SystemdScopeSupport::Unavailable(
            ManagedAgentIsolationFallback::SystemdUserManagerUnavailable,
        );
    }
    if !command_exists_in_path("systemctl") {
        return SystemdScopeSupport::Unavailable(
            ManagedAgentIsolationFallback::SystemctlUnavailable,
        );
    }
    if !require_launcher {
        return SystemdScopeSupport::Available;
    }
    if !command_exists_in_path("systemd-run") {
        return SystemdScopeSupport::Unavailable(
            ManagedAgentIsolationFallback::SystemdRunUnavailable,
        );
    }
    if !systemd_run_has_required_options() {
        return SystemdScopeSupport::Unavailable(
            ManagedAgentIsolationFallback::SystemdRunMissingRequiredOptions,
        );
    }
    SystemdScopeSupport::Available
}

#[cfg(windows)]
fn current_systemd_scope_support(_require_launcher: bool) -> SystemdScopeSupport {
    SystemdScopeSupport::WindowsJobObject
}

#[cfg(target_os = "macos")]
fn current_systemd_scope_support(_require_launcher: bool) -> SystemdScopeSupport {
    SystemdScopeSupport::Unavailable(ManagedAgentIsolationFallback::MacosProcessGroup)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn current_systemd_scope_support(_require_launcher: bool) -> SystemdScopeSupport {
    SystemdScopeSupport::Unavailable(ManagedAgentIsolationFallback::UnsupportedPlatform)
}

#[cfg(target_os = "linux")]
fn systemd_user_manager_endpoint_exists() -> bool {
    let configured = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|runtime| runtime.join("systemd/private"));
    let conventional = PathBuf::from(format!("/run/user/{}/systemd/private", unsafe {
        libc::geteuid()
    }));
    configured
        .into_iter()
        .chain(std::iter::once(conventional))
        .any(|endpoint| endpoint.exists())
}

#[cfg(target_os = "linux")]
fn systemd_run_has_required_options() -> bool {
    let output = Command::new("systemd-run")
        .arg("--help")
        .stdin(Stdio::null())
        .output();
    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let help = String::from_utf8_lossy(&output.stdout);
    [
        "--user",
        "--scope",
        "--collect",
        "--expand-environment",
        "--slice",
        "--property",
    ]
    .iter()
    .all(|option| help.contains(option))
}

#[cfg(target_os = "linux")]
fn query_scope_state(unit_name: &str) -> anyhow::Result<Option<SystemdScopeState>> {
    let output = Command::new("systemctl")
        .args([
            "--user",
            "show",
            "--no-pager",
            "--property=LoadState",
            "--property=ControlGroup",
            unit_name,
        ])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to inspect managed Agent scope {unit_name}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "failed to inspect managed Agent scope {unit_name}: status={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8(output.stdout)
        .context("systemctl returned non-UTF-8 managed Agent scope state")?;
    let mut load_state = None;
    let mut control_group = None;
    for line in stdout.lines() {
        if let Some(value) = line.strip_prefix("LoadState=") {
            load_state = Some(value.trim());
        } else if let Some(value) = line.strip_prefix("ControlGroup=") {
            control_group = Some(value.trim());
        }
    }
    if load_state == Some("not-found") {
        return Ok(None);
    }
    if load_state != Some("loaded") {
        anyhow::bail!(
            "managed Agent scope {unit_name} has unexpected LoadState={}",
            load_state.unwrap_or("missing")
        );
    }
    let control_group = control_group
        .filter(|value| !value.is_empty())
        .with_context(|| format!("managed Agent scope {unit_name} omitted ControlGroup"))?;
    let cgroup_path = cgroup_fs_path(unit_name, control_group)?;
    let populated = cgroup_is_populated(&cgroup_path)?;
    Ok(Some(SystemdScopeState {
        control_group: control_group.to_string(),
        cgroup_path,
        populated,
    }))
}

#[cfg(target_os = "linux")]
fn cgroup_fs_path(unit_name: &str, control_group: &str) -> anyhow::Result<PathBuf> {
    let relative = Path::new(control_group)
        .strip_prefix("/")
        .context("systemd scope ControlGroup is not absolute")?;
    if relative
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
        || relative.file_name().and_then(|value| value.to_str()) != Some(unit_name)
    {
        anyhow::bail!("systemd returned an invalid managed Agent ControlGroup: {control_group}");
    }
    Ok(Path::new("/sys/fs/cgroup").join(relative))
}

#[cfg(target_os = "linux")]
fn cgroup_is_populated(cgroup_path: &Path) -> anyhow::Result<bool> {
    let events_path = cgroup_path.join("cgroup.events");
    let events = match std::fs::read_to_string(&events_path) {
        Ok(events) => events,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to inspect managed Agent cgroup {}",
                    events_path.display()
                )
            })
        }
    };
    events
        .lines()
        .find_map(|line| line.strip_prefix("populated "))
        .map(|value| match value.trim() {
            "0" => Ok(false),
            "1" => Ok(true),
            other => anyhow::bail!("invalid cgroup populated value: {other}"),
        })
        .transpose()?
        .context("managed Agent cgroup.events omitted populated")
}

#[cfg(target_os = "linux")]
fn signal_scope(unit_name: &str, signal: &str, cgroup_path: &Path) -> anyhow::Result<()> {
    let output = Command::new("systemctl")
        .args([
            "--user",
            "kill",
            "--kill-whom=all",
            &format!("--signal={signal}"),
            unit_name,
        ])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to signal managed Agent scope {unit_name}"))?;
    if output.status.success() || !cgroup_is_populated(cgroup_path)? {
        return Ok(());
    }
    anyhow::bail!(
        "failed to signal managed Agent scope {unit_name}: status={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

#[cfg(target_os = "linux")]
fn wait_for_scope_empty(cgroup_path: &Path, timeout: Duration) -> anyhow::Result<bool> {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if !cgroup_is_populated(cgroup_path)? {
            return Ok(true);
        }
        thread::sleep(SCOPE_POLL_INTERVAL);
    }
    Ok(!cgroup_is_populated(cgroup_path)?)
}

#[cfg(target_os = "linux")]
fn wait_for_scope_collection(unit_name: &str) -> anyhow::Result<()> {
    let started = Instant::now();
    while started.elapsed() < SCOPE_STOP_WAIT {
        if query_scope_state(unit_name)?.is_none() {
            return Ok(());
        }
        thread::sleep(SCOPE_POLL_INTERVAL);
    }
    anyhow::bail!("managed Agent scope stopped but was not collected: {unit_name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_scopes_and_slices_are_qualified_by_the_instance_name() {
        let id = "cutex.018e0000-0000-7000-8000-0002a0001f93";
        let default = scope_unit_name_for(None, id);
        let instance = scope_unit_name_for(Some("kutex"), id);
        assert!(default.starts_with("cutex-agent-cutex.018e0000-"));
        assert!(instance.starts_with("cutex-kutex-agent-cutex.018e0000-"));
        assert_eq!(default.rsplit('-').next(), instance.rsplit('-').next());
        assert_eq!(slice_name_for(None), "cutex-agents.slice");
        assert_eq!(slice_name_for(Some("kutex")), "cutex-kutex-agents.slice");
        assert_eq!(managed_agent_scope_unit_name(id), default);
    }

    #[test]
    fn scope_unit_name_is_stable_readable_safe_and_identity_bound() {
        let first = managed_agent_scope_unit_name("cutex.018e0000-0000-7000-8000-0002a0001f93");
        let repeated = managed_agent_scope_unit_name("cutex.018e0000-0000-7000-8000-0002a0001f93");
        let second = managed_agent_scope_unit_name("cutex.018e0000-0000-7000-8000-0002a0001f94");

        assert_eq!(first, repeated);
        assert_ne!(first, second);
        assert!(first.starts_with("cutex-agent-cutex.018e0000-"));
        assert!(first.ends_with(".scope"));
        assert!(first
            .chars()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, '.' | '-' | '_')));
    }

    #[test]
    fn systemd_scope_plan_preserves_target_environment_and_exact_arguments() {
        let target = LaunchCommand::new("cute-codex")
            .args(["app-server", "literal-$HOME", "literal-$$"])
            .env("CUTEX_AGENT_ID", "runtime-agent")
            .env_remove("STALE_VALUE");
        let plan = managed_agent_launch_plan_with_support(
            "cutex.scope-test",
            target,
            SystemdScopeSupport::Available,
        );
        let ManagedAgentProcessIsolation::SystemdScope { unit_name } = &plan.isolation else {
            panic!("expected systemd scope");
        };

        assert_eq!(plan.launch.program, "systemd-run");
        assert!(plan.launch.args.iter().any(|arg| arg == "--scope"));
        assert!(plan
            .launch
            .args
            .iter()
            .any(|arg| arg == "--expand-environment=no"));
        assert!(plan
            .launch
            .args
            .iter()
            .any(|arg| arg == &format!("--unit={unit_name}")));
        assert!(plan
            .launch
            .args
            .windows(4)
            .any(|args| args == ["--", "cute-codex", "app-server", "literal-$HOME"]));
        assert!(plan.launch.args.iter().any(|arg| arg == "literal-$$"));
        assert!(plan
            .launch
            .envs
            .iter()
            .any(|(key, value)| key == "CUTEX_AGENT_ID" && value == "runtime-agent"));
        assert!(plan.launch.envs.iter().any(|(key, value)| {
            key == CUTEX_MANAGED_AGENT_SCOPE_UNIT_ENV_VAR && value == unit_name
        }));
        assert!(plan
            .launch
            .env_removes
            .iter()
            .any(|key| key == "STALE_VALUE"));
    }

    #[test]
    fn windows_job_names_mirror_scope_identity_in_the_global_namespace() {
        let id = "cutex.018e0000-0000-7000-8000-0002a0001f93";
        let default = job_name_for(None, id);
        let instance = job_name_for(Some("kutex"), id);
        let other = job_name_for(None, "cutex.018e0000-0000-7000-8000-0002a0001f94");

        assert_eq!(default, job_name_for(None, id));
        assert_eq!(managed_agent_job_name(id), default);
        assert_ne!(default, other);
        assert_ne!(default, instance);
        assert!(default.starts_with("Global\\cutex-agent-cutex.018e0000-"));
        assert!(instance.starts_with("Global\\cutex-kutex-agent-cutex.018e0000-"));
        // Same identity digest as the Linux scope unit, without the unit suffix.
        let scope = scope_unit_name_for(None, id);
        assert_eq!(
            default.strip_prefix("Global\\"),
            scope.strip_suffix(".scope")
        );
        // Kernel object names: one namespace separator, no other backslash,
        // well below MAX_PATH.
        let name = default.strip_prefix("Global\\").unwrap();
        assert!(!name.contains('\\'));
        assert!(default.len() < 260);
        let long = job_name_for(Some("kutex"), &"x".repeat(500));
        assert!(long.len() < 260, "{long}");
    }

    #[test]
    fn windows_job_plan_launches_the_target_directly_and_reports_the_job() {
        let target = LaunchCommand::new("C:\\cutex\\cute-codex.exe")
            .args(["app-server", "--listen", "literal-%PATH%"])
            .env("CUTEX_AGENT_ID", "runtime-agent")
            .env(CUTEX_MANAGED_AGENT_SCOPE_UNIT_ENV_VAR, "inherited-unit")
            .env(CUTEX_MANAGED_AGENT_ISOLATION_ENV_VAR, "stale")
            .env_remove("STALE_VALUE");
        let plan = managed_agent_launch_plan_with_support(
            "cutex.job-test",
            target.clone(),
            SystemdScopeSupport::WindowsJobObject,
        );
        let ManagedAgentProcessIsolation::WindowsJobObject { job_name } = &plan.isolation else {
            panic!("expected a Windows job object: {:?}", plan.isolation);
        };

        assert_eq!(job_name, &managed_agent_job_name("cutex.job-test"));
        assert_eq!(plan.launch.program, target.program);
        assert_eq!(plan.launch.args, target.args);
        let isolation: Vec<_> = plan
            .launch
            .envs
            .iter()
            .filter(|(key, _)| key == CUTEX_MANAGED_AGENT_ISOLATION_ENV_VAR)
            .map(|(_, value)| value.as_str())
            .collect();
        assert_eq!(isolation, [format!("windows_job_object:{job_name}").as_str()]);
        assert!(plan
            .launch
            .envs
            .iter()
            .all(|(key, _)| key != CUTEX_MANAGED_AGENT_SCOPE_UNIT_ENV_VAR));
        assert!(plan
            .launch
            .env_removes
            .iter()
            .any(|key| key == CUTEX_MANAGED_AGENT_SCOPE_UNIT_ENV_VAR));
        assert!(plan
            .launch
            .envs
            .iter()
            .any(|(key, value)| key == "CUTEX_AGENT_ID" && value == "runtime-agent"));
        assert!(plan
            .launch
            .env_removes
            .iter()
            .any(|key| key == "STALE_VALUE"));
    }

    #[test]
    fn isolation_env_values_name_the_boundary() {
        assert_eq!(
            ManagedAgentProcessIsolation::SystemdScope {
                unit_name: "u.scope".into()
            }
            .env_value(),
            "systemd_scope:u.scope"
        );
        assert_eq!(
            ManagedAgentProcessIsolation::WindowsJobObject {
                job_name: "Global\\j".into()
            }
            .env_value(),
            "windows_job_object:Global\\j"
        );
        assert_eq!(
            ManagedAgentProcessIsolation::Direct {
                reason: ManagedAgentIsolationFallback::MacosProcessGroup
            }
            .env_value(),
            "direct:macos_process_group"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_reports_a_job_object_boundary() {
        assert!(matches!(
            managed_agent_process_isolation("cutex.windows-job-probe"),
            ManagedAgentProcessIsolation::WindowsJobObject { .. }
        ));
    }

    #[test]
    fn unsupported_systemd_boundary_is_an_explicit_direct_fallback() {
        let plan = managed_agent_launch_plan_with_support(
            "cutex.scope-fallback",
            LaunchCommand::new("cute-codex"),
            SystemdScopeSupport::Unavailable(
                ManagedAgentIsolationFallback::SystemdUserManagerUnavailable,
            ),
        );

        assert_eq!(
            plan.isolation,
            ManagedAgentProcessIsolation::Direct {
                reason: ManagedAgentIsolationFallback::SystemdUserManagerUnavailable,
            }
        );
        assert_eq!(plan.launch.program, "cute-codex");
        assert!(plan.launch.envs.iter().any(|(key, value)| {
            key == CUTEX_MANAGED_AGENT_ISOLATION_ENV_VAR
                && value == "direct:systemd_user_manager_unavailable"
        }));
        assert!(plan
            .launch
            .env_removes
            .iter()
            .any(|key| key == CUTEX_MANAGED_AGENT_SCOPE_UNIT_ENV_VAR));
    }
}
