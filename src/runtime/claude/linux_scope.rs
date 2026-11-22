use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, ensure};
use fs2::FileExt;

use super::{ClaudeSession, CutexSessionRecord, write_private_pretty_json_atomic};
use crate::runtime::process_scope::{
    CUTEX_MANAGED_AGENT_ISOLATION_ENV_VAR, CUTEX_MANAGED_AGENT_SCOPE_UNIT_ENV_VAR,
    LinuxRuntimeScope, SystemdUserManager,
};

fn receipt_path(session: &ClaudeSession) -> PathBuf {
    session.state_dir.join("linux-process-scope.json")
}

fn start_lease(session: &ClaudeSession) -> anyhow::Result<File> {
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(session.state_dir.join("linux-process-scope.lock"))?)
}

fn read_unit(session: &ClaudeSession) -> anyhow::Result<Option<String>> {
    match std::fs::read(receipt_path(session)) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).context("invalid Claude Linux scope receipt")?,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("failed to read Claude Linux scope receipt"),
    }
}

pub(super) fn recorded(
    record: &CutexSessionRecord,
    session: &ClaudeSession,
) -> anyhow::Result<Option<LinuxRuntimeScope>> {
    let Some(unit) = read_unit(session)? else {
        return Ok(None);
    };
    let scope = LinuxRuntimeScope::new(&record.cutex_session_id, "claude", &session.state_dir);
    ensure!(
        unit == scope.unit(),
        "Claude Linux scope does not match its session"
    );
    Ok(Some(scope))
}

pub(super) fn verify_adapter(session: &ClaudeSession, pid: u32) -> anyhow::Result<()> {
    if let Some(unit) = read_unit(session)? {
        ensure!(
            crate::runtime::process_scope::linux_pid_in_scope(pid, &unit)?,
            "Claude adapter outside its recorded Linux scope"
        );
    }
    Ok(())
}

pub(super) fn is_empty(
    record: &CutexSessionRecord,
    session: &ClaudeSession,
) -> anyhow::Result<bool> {
    let Some(scope) = recorded(record, session)? else {
        return Ok(true);
    };
    if !scope.is_empty()? {
        return Ok(false);
    }
    // An unregistered wrapper may still be starting outside the scope.
    // Its inherited flock makes absence distinguishable from that interval.
    let lease = start_lease(session)?;
    match FileExt::try_lock_exclusive(&lease) {
        Ok(()) => scope.is_empty(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn verify_empty(
    record: &CutexSessionRecord,
    session: &ClaudeSession,
) -> anyhow::Result<()> {
    ensure!(
        is_empty(record, session)?,
        "Claude runtime scope is populated or still starting"
    );
    Ok(())
}

pub(super) fn stop(record: &CutexSessionRecord, session: &ClaudeSession) -> anyhow::Result<()> {
    if let Some(scope) = recorded(record, session)? {
        ensure!(
            scope.terminate(true)?.stopped,
            "failed to empty Claude runtime scope"
        );
        verify_empty(record, session)?;
        std::fs::remove_file(receipt_path(session))?;
    }
    Ok(())
}
fn claude_systemd_scope_command(
    direct: &Command,
    launcher: PathBuf,
    args: Vec<String>,
    manager: &SystemdUserManager,
) -> Command {
    const BUS: &str = "DBUS_SESSION_BUS_ADDRESS";
    let mut command = Command::new(launcher);
    let bus = direct.get_envs().find(|(key, _)| *key == BUS)
        .map_or_else(|| std::env::var_os(BUS), |(_, value)| value.map(|value| value.to_owned()));
    if let Some(bus) = bus {
        let mut arg = std::ffi::OsString::from("--setenv=DBUS_SESSION_BUS_ADDRESS=");
        arg.push(bus);
        command.arg(arg);
    }
    command.args(args).arg(direct.get_program()).args(direct.get_args());
    for (key, value) in direct.get_envs() {
        if let Some(value) = value {
            command.env(key, value);
        } else {
            command.env_remove(key);
        }
    }
    manager.configure_command(&mut command);
    command
}

pub(super) fn prepare_command(
    record: &CutexSessionRecord,
    session: &ClaudeSession,
    mut direct: Command,
    log: File,
) -> anyhow::Result<(Command, Option<File>)> {
    let (mut command, lease) = match LinuxRuntimeScope::launcher(&record.cutex_session_id) {
        Ok((launcher, manager)) => {
            let scope =
                LinuxRuntimeScope::new(&record.cutex_session_id, "claude", &session.state_dir);
            let mut command = claude_systemd_scope_command(
                &direct, launcher, scope.arguments()?, &manager,
            );
            command
                .current_dir(&record.cwd)
                .env(CUTEX_MANAGED_AGENT_SCOPE_UNIT_ENV_VAR, scope.unit())
                .env(
                    CUTEX_MANAGED_AGENT_ISOLATION_ENV_VAR,
                    format!("systemd_scope:{}", scope.unit()),
                );
            // Retain the intent before spawn so a daemon crash during startup
            // still leaves an exclusive scope and a discoverable stop target.
            write_private_pretty_json_atomic(
                &receipt_path(session),
                scope.unit(),
                "Claude Linux scope receipt",
            )?;
            let lease = start_lease(session)?;
            FileExt::try_lock_exclusive(&lease).context("Claude scope launch is still owned")?;
            let fd = lease.as_raw_fd();
            // The adapter inherits the open file description through systemd-run.
            // Closing the daemon's copy after spawn preserves it through crashes.
            unsafe {
                command.pre_exec(move || {
                    if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            (command, Some(lease))
        }
        Err(reason) => {
            if recorded(record, session)?.is_some() {
                std::fs::remove_file(receipt_path(session))?;
            }
            direct
                .env_remove(CUTEX_MANAGED_AGENT_SCOPE_UNIT_ENV_VAR)
                .env(
                    CUTEX_MANAGED_AGENT_ISOLATION_ENV_VAR,
                    format!("direct:{reason}"),
                );
            (direct, None)
        }
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);
    Ok((command, lease))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::os::unix::{fs::PermissionsExt, net::UnixListener};

    #[test]
    fn claude_scope_restores_the_session_bus_and_honors_explicit_removal() {
        let dir = std::env::temp_dir().join(format!("cs-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("systemd")).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let _listener = UnixListener::bind(dir.join("systemd/private")).unwrap();
        let runtime_dir = dir.to_str().unwrap();
        let manager = SystemdUserManager::from_runtime_dir(runtime_dir.to_owned()).unwrap();
        for address in [None, Some("unix:path=/claude/session-bus")] {
            let mut direct = Command::new("claude");
            direct.arg("literal-$HOME");
            if let Some(bus) = address {
                direct.env("DBUS_SESSION_BUS_ADDRESS", bus);
            } else {
                direct.env_remove("DBUS_SESSION_BUS_ADDRESS");
            }
            let scoped = claude_systemd_scope_command(
                &direct, PathBuf::from("/usr/bin/systemd-run"),
                vec!["--scope".to_owned(), "--".to_owned()], &manager,
            );
            let bus_option = address.map(|value| format!("--setenv=DBUS_SESSION_BUS_ADDRESS={value}"));
            assert_eq!(
                scoped.get_args().find(|arg| arg.to_string_lossy().starts_with("--setenv=DBUS_SESSION_BUS_ADDRESS=")),
                bus_option.as_deref().map(OsStr::new),
            );
            assert!(scoped.get_envs().any(|(key, value)| key == "XDG_RUNTIME_DIR"
                && value == Some(OsStr::new(runtime_dir))));
            assert!(scoped.get_envs().any(|(key, value)| key == "DBUS_SESSION_BUS_ADDRESS" && value.is_none()));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
