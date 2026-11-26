use std::{
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::profiles::{
    materialize,
    model::{CliKind, MaterializedAccountFiles, StoredAccount},
    store::account_by_key,
};

use super::{Candidate, Failure};

mod providers;
mod scratch;

pub(super) fn sweep_startup() -> anyhow::Result<()> {
    scratch::sweep_startup()
}

const MAX_WIRE: u64 = 256 * 1024;
pub const ARGUMENT: &str = "--internal-text-rewrite-worker";

#[derive(Serialize, Deserialize)]
pub(super) struct Answer {
    pub text: String,
    pub model: Option<String>,
    pub requested_model: String,
    pub provider: String,
    pub startup_ms: Option<u128>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Job {
    profile: Profile,
    message: String,
    work: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum Profile {
    Agent(String),
    Feature(String),
}

pub(super) fn family(profile: &Candidate<'_>) -> &'static str {
    let Candidate::Agent(profile) = profile else {
        return "feature";
    };
    match profile.cli_kind {
        CliKind::Codex => "codex",
        CliKind::Claude => "claude",
        CliKind::Dsh => "dsh",
        CliKind::Kimi => "kimi",
    }
}

struct RestoreAuthPermissions {
    files: MaterializedAccountFiles,
    before: Option<(u64, std::time::SystemTime)>,
}

impl RestoreAuthPermissions {
    fn for_profile(profile: &StoredAccount) -> Option<Self> {
        if !providers::uses_native_subscription(profile) {
            return None;
        }
        let files = materialize::materialized_account_files(profile).ok()?;
        let before = Self::stamp(&files.auth_path);
        Some(Self { files, before })
    }

    fn stamp(path: &Path) -> Option<(u64, std::time::SystemTime)> {
        let metadata = std::fs::metadata(path).ok()?;
        Some((metadata.len(), metadata.modified().ok()?))
    }
}

impl Drop for RestoreAuthPermissions {
    fn drop(&mut self) {
        if Self::stamp(&self.files.auth_path) != self.before
            && materialize::set_materialized_file_permissions(&self.files).is_err()
        {
            eprintln!("text rewrite: could not re-secure profile auth file");
        }
    }
}

struct Running {
    child: Child,
    #[cfg(windows)]
    job: std::os::windows::io::OwnedHandle,
}

impl Running {
    fn spawn(command: &mut Command) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
            Ok(Self {
                child: command.spawn()?,
            })
        }
        #[cfg(windows)]
        {
            use crate::platform::windows_job;
            use std::os::windows::process::CommandExt;
            use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED};
            let job =
                windows_job::create_agent_job(&format!("cutex-rewrite-{}", uuid::Uuid::new_v4()))
                    .map_err(io::Error::other)?;
            command.creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
            let mut child = command.spawn()?;
            if let Err(error) = windows_job::assign_suspended_worker(&job, &child) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::other(error));
            }
            Ok(Self { child, job })
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        #[cfg(unix)]
        // The unreaped worker PID is the process-group id created above.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        #[cfg(windows)]
        {
            let _ = crate::platform::windows_job::terminate_job(&self.job);
            let _ =
                crate::platform::windows_job::wait_for_job_empty(&self.job, Duration::from_secs(1));
        }
        let _ = self.child.wait();
    }
}

pub(super) fn run(
    profile: &Candidate<'_>,
    message: &str,
    deadline: Instant,
    cancelled: impl Fn() -> bool,
) -> Result<Answer, Failure> {
    let work = scratch::WorkDir::create().map_err(|_| Failure::ProfileUnavailable)?;
    let request = serde_json::to_vec(&Job {
        profile: match profile {
            Candidate::Agent(account) => Profile::Agent(account.id.clone()),
            Candidate::Feature(id) => Profile::Feature(id.clone()),
        },
        message: message.into(),
        work: work.path().to_path_buf(),
    })
    .map_err(|_| Failure::Upstream)?;
    let image = std::env::current_exe().map_err(|_| Failure::ProfileUnavailable)?;
    if cancelled() {
        return Err(Failure::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(Failure::Timeout);
    }
    let mut command = Command::new(image);
    command
        .arg(ARGUMENT)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // The parent restores custody after the process tree has stopped, including
    // when a token refresh was interrupted by disconnect or timeout.
    let _auth_permissions = match profile {
        Candidate::Agent(account) => RestoreAuthPermissions::for_profile(account),
        Candidate::Feature(_) => None,
    };
    let mut running = Running::spawn(&mut command).map_err(|_| Failure::ProfileUnavailable)?;
    let mut input = running.child.stdin.take().ok_or(Failure::Upstream)?;
    let output = running.child.stdout.take().ok_or(Failure::Upstream)?;
    thread::scope(|scope| {
        scope.spawn(move || {
            let _ = input.write_all(&request);
        });
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        scope.spawn(move || {
            let mut bytes = Vec::new();
            let result = output
                .take(MAX_WIRE + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| Failure::Upstream)
                .and_then(|_| {
                    if bytes.len() as u64 > MAX_WIRE {
                        Err(Failure::InvalidOutput)
                    } else {
                        serde_json::from_slice::<Result<Answer, Failure>>(&bytes)
                            .map_err(|_| Failure::Upstream)?
                    }
                });
            let _ = send.send(result);
        });
        let result = loop {
            if cancelled() {
                break Err(Failure::Cancelled);
            }
            if Instant::now() >= deadline {
                break Err(Failure::Timeout);
            }
            match receive.recv_timeout(Duration::from_millis(20)) {
                Ok(result) => break result,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => break Err(Failure::Upstream),
            }
        };
        // Kill and reap before joining pipe threads or deleting private files.
        drop(running);
        result
    })
}

pub(super) fn entry() -> i32 {
    let result = (|| {
        let mut input = Vec::new();
        io::stdin()
            .take(MAX_WIRE + 1)
            .read_to_end(&mut input)
            .map_err(|_| Failure::Upstream)?;
        if input.len() as u64 > MAX_WIRE {
            return Err(Failure::InvalidOutput);
        }
        let request: Job = serde_json::from_slice(&input).map_err(|_| Failure::Upstream)?;
        let work =
            scratch::WorkDir::attach(request.work).map_err(|_| Failure::ProfileUnavailable)?;
        match request.profile {
            Profile::Agent(id) => {
                let profile = account_by_key(&id)
                    .map_err(|_| Failure::ProfileUnavailable)?
                    .filter(|profile| profile.id == id)
                    .ok_or(Failure::ProfileUnavailable)?;
                providers::run(&profile, &request.message, work.path())
            }
            Profile::Feature(id) => {
                let profile = crate::management::v2::feature_profiles::provider(&id)
                    .map_err(|_| Failure::ProfileUnavailable)?
                    .ok_or(Failure::ProfileUnavailable)?;
                providers::run_feature(profile, &request.message)
            }
        }
    })();
    i32::from(serde_json::to_writer(io::stdout().lock(), &result).is_err())
}

fn isolated_command(
    image: &Path,
    home: &Path,
    proxy: &crate::config::proxy_outbound::OutboundProxy,
) -> Command {
    let mut command = Command::new(image);
    command
        .env_clear()
        .current_dir(home)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("APPDATA", home)
        .env("LOCALAPPDATA", home)
        .env("TEMP", home)
        .env("TMP", home)
        .env("TMPDIR", home);
    for key in ["SYSTEMROOT", "WINDIR", "COMSPEC", "PATH", "LANG", "LC_ALL"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    if let Some(proxy) = proxy.resolve() {
        command
            .env("HTTPS_PROXY", &proxy.url)
            .env("HTTP_PROXY", proxy.url)
            .env("NO_PROXY", proxy.no_proxy);
    }
    command
}
