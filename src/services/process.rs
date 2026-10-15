//! Spawn a service command in its own process tree and kill the whole tree.
//!
//! Windows: the child is assigned to a private job object with
//! `KILL_ON_JOB_CLOSE` right after spawn; stop terminates the job, and if the
//! daemon dies the job handle closes and the tree goes with it (the supervisor
//! starts managed services again on the next daemon start).
//! Unix: the child leads a new process group; stop sends SIGTERM to the group
//! and SIGKILL after a grace period.

use std::path::Path;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use super::store::ServiceCommand;
use super::store::ServiceRecord;

/// Environment variables never passed to service processes.
fn inherited_env_blocked(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    upper.starts_with("CUTEX_") || upper.starts_with("CODEX_")
}

fn substitute(value: &str, port: u16) -> String {
    value.replace("{port}", &port.to_string())
}

/// The `Command` for a record (not yet spawned); exposed for tests.
pub fn build_command(record: &ServiceRecord, default_cwd: &Path) -> anyhow::Result<Command> {
    let command = record
        .command
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("service `{}` has no command", record.slug))?;
    let port = record.port;
    let mut builder = match command {
        ServiceCommand::Shell(line) => shell_command(&substitute(line, port)),
        ServiceCommand::Argv(argv) => {
            let mut builder = Command::new(substitute(&argv[0], port));
            builder.args(argv[1..].iter().map(|arg| substitute(arg, port)));
            builder
        }
    };
    let cwd = record.cwd.as_deref().map(Path::new).unwrap_or(default_cwd);
    builder.current_dir(cwd);
    for (key, _) in std::env::vars_os() {
        if key.to_str().is_some_and(inherited_env_blocked) {
            builder.env_remove(&key);
        }
    }
    builder.env("PORT", port.to_string());
    // Loopback by default (frameworks that honour HOST); a record env overrides.
    builder.env("HOST", "127.0.0.1");
    builder.env("PYTHONUNBUFFERED", "1");
    for (key, value) in &record.env {
        builder.env(key, substitute(value, port));
    }
    builder
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(builder)
}

#[cfg(windows)]
fn shell_command(line: &str) -> Command {
    use std::os::windows::process::CommandExt;
    let comspec = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
    let mut command = Command::new(comspec);
    // `/S /C "<line>"`: cmd strips exactly the outer quotes and runs the rest
    // verbatim, so the line keeps its own quoting.
    command.raw_arg(format!("/D /S /C \"{line}\""));
    command
}

#[cfg(not(windows))]
fn shell_command(line: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(line);
    command
}

/// Handle on a spawned service's whole process tree.
pub struct ProcessTree {
    pid: u32,
    exited: AtomicBool,
    #[cfg(windows)]
    job: Option<std::os::windows::io::OwnedHandle>,
}

impl std::fmt::Debug for ProcessTree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessTree")
            .field("pid", &self.pid)
            .finish()
    }
}

impl ProcessTree {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The root process was reaped: never signal its (reusable) ids again.
    pub fn mark_exited(&self) {
        self.exited.store(true, Ordering::SeqCst);
    }

    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::SeqCst)
    }

    /// Terminate every process in the tree (best effort, non-blocking).
    pub fn kill(&self) {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            if let Some(job) = &self.job {
                // Terminating the job also covers children of an exited root.
                unsafe {
                    windows_sys::Win32::System::JobObjects::TerminateJobObject(
                        job.as_raw_handle(),
                        1,
                    );
                }
                return;
            }
            if !self.has_exited() {
                let _ = Command::new("taskkill")
                    .args(["/T", "/F", "/PID", &self.pid.to_string()])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
        #[cfg(unix)]
        {
            let group = -(self.pid as i32);
            unsafe {
                libc::kill(group, libc::SIGTERM);
            }
            let pid = self.pid;
            // The group may outlive its leader; SIGKILL whatever is left.
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(5));
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            });
        }
    }
}

/// Spawn `command` in a fresh process tree.
pub fn spawn(mut command: Command) -> anyhow::Result<(Child, ProcessTree)> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use std::os::windows::io::FromRawHandle;
        use std::os::windows::io::OwnedHandle;
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::JobObjects::*;
        use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        let job = (!job.is_null()).then(|| unsafe { OwnedHandle::from_raw_handle(job) });
        if let Some(job) = &job {
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            unsafe {
                SetInformationJobObject(
                    job.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of_val(&limits) as u32,
                );
            }
        }
        let child = command.spawn()?;
        let job = job.filter(|job| unsafe {
            AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) != 0
        });
        let tree = ProcessTree {
            pid: child.id(),
            exited: AtomicBool::new(false),
            job,
        };
        Ok((child, tree))
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        let child = command.spawn()?;
        let tree = ProcessTree {
            pid: child.id(),
            exited: AtomicBool::new(false),
        };
        Ok((child, tree))
    }
}

/// At most this much login-link output is read (a login URL is one short line).
const LOGIN_LINK_MAX_OUTPUT: usize = 16 * 1024;

/// Why a login-link command gave no usable link. Never carries its output (a login URL is a
/// one-time credential).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginLinkError {
    Spawn,
    Timeout,
    Exit,
    NoLink,
}

impl LoginLinkError {
    pub fn label(self) -> &'static str {
        match self {
            Self::Spawn => "spawnFailed",
            Self::Timeout => "timeout",
            Self::Exit => "exitStatus",
            Self::NoLink => "noLink",
        }
    }
}

/// Runs `record.login_link_command` like the service itself (same cwd / env / `{port}`) and
/// returns the path and query of the last line that is a URL of this service on loopback
/// (`localhost`, `127.0.0.1` or `[::1]` with the record's port). The process tree is killed
/// after `timeout`; stderr is discarded.
pub fn login_link_path(
    record: &ServiceRecord,
    default_cwd: &Path,
    timeout: std::time::Duration,
) -> Result<String, LoginLinkError> {
    use std::io::Read;
    let command = record
        .login_link_command
        .clone()
        .ok_or(LoginLinkError::Spawn)?;
    let probe = ServiceRecord {
        command: Some(command),
        ..record.clone()
    };
    let mut builder = build_command(&probe, default_cwd).map_err(|_| LoginLinkError::Spawn)?;
    builder.stderr(Stdio::null());
    let (mut child, tree) = spawn(builder).map_err(|_| LoginLinkError::Spawn)?;
    let mut stdout = child.stdout.take().ok_or(LoginLinkError::Spawn)?;
    let reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = (&mut stdout)
            .take(LOGIN_LINK_MAX_OUTPUT as u64)
            .read_to_end(&mut output);
        output
    });
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            _ => break None,
        }
    };
    let Some(status) = status else {
        tree.kill();
        let _ = child.wait();
        tree.mark_exited();
        return Err(LoginLinkError::Timeout);
    };
    tree.mark_exited();
    // Grandchildren holding stdout open must not stall the request either.
    let output = {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(reader.join().unwrap_or_default());
        });
        receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap_or_default()
    };
    if !status.success() {
        return Err(LoginLinkError::Exit);
    }
    let text = String::from_utf8_lossy(&output);
    text.lines()
        .rev()
        .find_map(|line| loopback_link_path(line.trim(), record.port))
        .ok_or(LoginLinkError::NoLink)
}

/// `http(s)://localhost|127.0.0.1|[::1]:<port>/path?query` → `/path?query`.
fn loopback_link_path(line: &str, port: u16) -> Option<String> {
    let url = url::Url::parse(line).ok()?;
    let host_ok = match url.host()? {
        url::Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(ip) => ip.is_loopback(),
        url::Host::Ipv6(ip) => ip.is_loopback(),
    };
    if !matches!(url.scheme(), "http" | "https")
        || !host_ok
        || url.port_or_known_default() != Some(port)
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    let mut path = url.path().to_string();
    if let Some(query) = url.query() {
        path.push('?');
        path.push_str(query);
    }
    (crate::services::proxy::auth::safe_next(&path) == path && path.len() <= 2048).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::store::ServiceRuntime;
    use std::io::Read;
    use std::time::Duration;
    use std::time::Instant;

    fn record(command: ServiceCommand) -> ServiceRecord {
        ServiceRecord {
            proxy_host_header: "preserve".into(),
            slug: "t".into(),
            display_name: "t".into(),
            host: "h".into(),
            port: 4321,
            path: None,
            owner_cutex_session_id: None,
            project_id: None,
            runtime: ServiceRuntime::Managed,
            lifetime: Default::default(),
            root: None,
            command: Some(command),
            cwd: None,
            env: [("GREETING".to_string(), "hi-{port}".to_string())].into(),
            login_link_command: None,
            stopped: false,
            port_allocated: false,
            ceiling: None,
            provenance: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn loopback_link_path_accepts_only_this_service_on_loopback() {
        assert_eq!(
            loopback_link_path("http://localhost:4321/?nonce=abc", 4321).as_deref(),
            Some("/?nonce=abc")
        );
        assert_eq!(
            loopback_link_path("http://127.0.0.1:4321/login/x?a=1&b=2", 4321).as_deref(),
            Some("/login/x?a=1&b=2")
        );
        assert_eq!(
            loopback_link_path("http://[::1]:4321/", 4321).as_deref(),
            Some("/")
        );
        for bad in [
            "http://localhost:4322/?nonce=abc",
            "http://example.com:4321/?nonce=abc",
            "http://user:pw@localhost:4321/",
            "https://localhost/?nonce=abc",
            "ftp://localhost:4321/",
            "Web UI -> http://localhost:4321/",
            "",
        ] {
            assert_eq!(loopback_link_path(bad, 4321), None, "{bad}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn login_link_path_runs_like_the_service() {
        let mut login = record(ServiceCommand::Shell("true".into()));
        let cwd = std::env::temp_dir();
        let timeout = Duration::from_secs(10);
        // The last loopback URL for this port wins; noise and other hosts are skipped.
        login.login_link_command = Some(ServiceCommand::Shell(
            "echo noise; echo 'http://localhost:{port}/?nonce=old'; echo \"http://localhost:$PORT/?nonce=$GREETING\"; echo http://example.com:4321/x; echo done >&2".into(),
        ));
        assert_eq!(
            login_link_path(&login, &cwd, timeout).as_deref(),
            Ok("/?nonce=hi-4321")
        );
        login.login_link_command = Some(ServiceCommand::Shell(
            "echo http://localhost:4321/; exit 4".into(),
        ));
        assert_eq!(
            login_link_path(&login, &cwd, timeout),
            Err(LoginLinkError::Exit)
        );
        login.login_link_command = Some(ServiceCommand::Shell("echo nothing".into()));
        assert_eq!(
            login_link_path(&login, &cwd, timeout),
            Err(LoginLinkError::NoLink)
        );
        login.login_link_command = Some(ServiceCommand::Shell("sleep 30".into()));
        let started = Instant::now();
        assert_eq!(
            login_link_path(&login, &cwd, Duration::from_millis(300)),
            Err(LoginLinkError::Timeout)
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        login.login_link_command = None;
        assert_eq!(
            login_link_path(&login, &cwd, timeout),
            Err(LoginLinkError::Spawn)
        );
    }

    #[test]
    fn shell_command_sees_port_and_env() {
        let line = if cfg!(windows) {
            "echo port={port} env=%PORT% %GREETING%"
        } else {
            "echo port={port} env=$PORT $GREETING"
        };
        let command = build_command(
            &record(ServiceCommand::Shell(line.into())),
            &std::env::temp_dir(),
        )
        .unwrap();
        let (mut child, tree) = spawn(command).unwrap();
        let mut output = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut output)
            .unwrap();
        child.wait().unwrap();
        tree.mark_exited();
        assert!(
            output.contains("port=4321 env=4321 hi-4321"),
            "unexpected output {output:?}"
        );
    }

    #[test]
    fn kill_terminates_the_tree() {
        // A shell that starts a long-lived grandchild.
        let line = if cfg!(windows) {
            "ping -n 60 127.0.0.1 > NUL"
        } else {
            "sleep 60 & sleep 60"
        };
        let command = build_command(
            &record(ServiceCommand::Shell(line.into())),
            &std::env::temp_dir(),
        )
        .unwrap();
        let (mut child, tree) = spawn(command).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let started = Instant::now();
        tree.kill();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut exited = false;
        while Instant::now() < deadline {
            if child.try_wait().unwrap().is_some() {
                exited = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(exited, "root process exits after kill");
        assert!(started.elapsed() < Duration::from_secs(10));
        tree.mark_exited();
    }
}
