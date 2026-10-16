use crate::model::{ExecutionObservation, JobError};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::thread;
use std::time::Duration;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub(crate) const EXECUTION_OBSERVATION_BASIS: &str = "runner_release_to_wait_v1";

pub(crate) struct ExecutionStart {
    pub monotonic: Instant,
    pub wall_epoch_millis: Option<u64>,
}

pub(crate) fn epoch_millis(value: SystemTime) -> Option<u64> {
    value
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
}

pub(crate) fn complete_observation(
    observation: &mut ExecutionObservation,
    exit_wall: SystemTime,
    elapsed: Duration,
) {
    let exit = epoch_millis(exit_wall);
    observation.exit_observed_at_epoch_millis =
        match (observation.start_observed_at_epoch_millis, exit) {
            (Some(start), Some(end)) if end < start => None,
            (_, value) => value,
        };
    observation.observed_run_duration_millis = u64::try_from(elapsed.as_millis()).ok();
}

pub(crate) struct LiveProcess {
    pub pid: u32,
    pub start_ticks: u64,
    pub _liveness_write: OwnedFd,
    pub cancelled: Arc<AtomicBool>,
}

pub(crate) struct Spawned {
    pub child: Child,
    pub sentinel: Child,
    pub live: LiveProcess,
    pub stdout_observed: Arc<AtomicU64>,
    pub stderr_observed: Arc<AtomicU64>,
    pub stdout_drain: thread::JoinHandle<std::io::Result<()>>,
    pub stderr_drain: thread::JoinHandle<std::io::Result<()>>,
    pub execution_start: ExecutionStart,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_contained(
    current_exe: &Path,
    launcher: &str,
    sandbox_json: &str,
    argv: &[String],
    cwd: &str,
    env: &BTreeMap<String, String>,
    stdout_path: &Path,
    stderr_path: &Path,
    output_limit: u64,
) -> Result<Spawned, JobError> {
    let (gate_read, gate_write) = pipe()?;
    let (live_read, live_write) = pipe()?;
    clear_cloexec(gate_read.as_raw_fd())?;
    clear_cloexec(live_read.as_raw_fd())?;

    let mut command = Command::new(current_exe);
    command
        .arg("__runner")
        .arg(gate_read.as_raw_fd().to_string())
        .arg(launcher)
        .arg(sandbox_json)
        .arg(cwd)
        .arg("--")
        .args(argv)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    drop(gate_read);
    let pid = child.id();
    let start_ticks = process_start_ticks(pid)?;

    let sentinel = Command::new(current_exe)
        .arg("__sentinel")
        .arg(live_read.as_raw_fd().to_string())
        .arg(pid.to_string())
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    drop(live_read);

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| JobError::Invalid("runner stdout unavailable".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| JobError::Invalid("runner stderr unavailable".into()))?;
    let stdout_observed = Arc::new(AtomicU64::new(0));
    let stderr_observed = Arc::new(AtomicU64::new(0));
    let stdout_drain = drain_bounded(
        stdout,
        stdout_path,
        output_limit,
        Arc::clone(&stdout_observed),
    )?;
    let stderr_drain = drain_bounded(
        stderr,
        stderr_path,
        output_limit,
        Arc::clone(&stderr_observed),
    )?;
    let execution_start = ExecutionStart {
        monotonic: Instant::now(),
        wall_epoch_millis: epoch_millis(SystemTime::now()),
    };
    File::from(gate_write).write_all(&[1])?;
    Ok(Spawned {
        child,
        sentinel,
        live: LiveProcess {
            pid,
            start_ticks,
            _liveness_write: live_write,
            cancelled: Arc::new(AtomicBool::new(false)),
        },
        stdout_observed,
        stderr_observed,
        stdout_drain,
        stderr_drain,
        execution_start,
    })
}

pub(crate) fn drain_bounded<R: Read + Send + 'static>(
    mut input: R,
    path: &Path,
    limit: u64,
    observed: Arc<AtomicU64>,
) -> Result<thread::JoinHandle<std::io::Result<()>>, JobError> {
    let mut output = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    let tail_path = path.with_file_name(format!(
        "{}.tail",
        path.file_name().expect("output filename").to_string_lossy()
    ));
    Ok(thread::spawn(move || {
        let mut tail = std::collections::VecDeque::new();
        let tail_limit = (limit / 2) as usize;
        let mut kept = 0u64;
        let mut buffer = [0u8; 8192];
        loop {
            let n = input.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            observed.fetch_add(n as u64, Ordering::Relaxed);
            tail.extend(&buffer[..n]);
            if tail.len() > tail_limit {
                tail.drain(..tail.len() - tail_limit);
            }
            if kept < limit {
                let take = n.min((limit - kept) as usize);
                output.write_all(&buffer[..take])?;
                kept += take as u64;
            }
        }
        let total = observed.load(Ordering::Relaxed);
        if total > limit {
            // Sidecar starts with the original byte offset, followed by the final tail.
            // Publish only once drained; running reads retain their prefix semantics.
            let temporary = tail_path.with_extension("tail.tmp");
            let mut saved = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&temporary)?;
            saved.write_all(&(total - tail.len() as u64).to_le_bytes())?;
            saved.write_all(tail.make_contiguous())?;
            saved.sync_all()?;
            output.set_len(limit - tail_limit as u64)?;
            std::fs::rename(temporary, tail_path)?;
        }
        output.flush()?;
        output.sync_all()
    }))
}

pub(crate) fn cancel(process: &LiveProcess, grace: Duration) -> Result<(), JobError> {
    if process_start_ticks(process.pid)? != process.start_ticks {
        return Err(JobError::Conflict("process birth identity changed".into()));
    }
    process.cancelled.store(true, Ordering::Release);
    signal_group(process.pid, libc::SIGTERM)?;
    let deadline = std::time::Instant::now() + grace;
    while std::time::Instant::now() < deadline {
        if !group_exists(process.pid) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    signal_group(process.pid, libc::SIGKILL)
}

pub(crate) fn runner_main(args: &[String]) -> Result<(), JobError> {
    if args.len() < 6 || args[4] != "--" {
        return Err(JobError::Invalid(
            "invalid private runner invocation".into(),
        ));
    }
    let gate: i32 = args[0]
        .parse()
        .map_err(|_| JobError::Invalid("invalid gate fd".into()))?;
    let mut gate = unsafe { File::from_raw_fd(gate) };
    let mut byte = [0u8; 1];
    if gate.read_exact(&mut byte).is_err() || byte[0] != 1 {
        return Err(JobError::Invalid(
            "launch gate closed before containment".into(),
        ));
    }
    let err = Command::new(&args[1])
        .arg("sandbox")
        .arg("--sandbox-state-json")
        .arg(&args[2])
        .arg("--")
        .arg("/bin/sh")
        .arg("-c")
        .arg("cd -- \"$1\" && shift && exec \"$@\"")
        .arg("cutex-job")
        .arg(&args[3])
        .args(&args[5..])
        .current_dir(&args[3])
        .stdin(Stdio::null())
        .exec();
    Err(JobError::Io(err))
}

pub(crate) fn sentinel_main(args: &[String]) -> Result<(), JobError> {
    if args.len() != 2 {
        return Err(JobError::Invalid(
            "invalid private sentinel invocation".into(),
        ));
    }
    let fd: i32 = args[0]
        .parse()
        .map_err(|_| JobError::Invalid("invalid liveness fd".into()))?;
    let pgid: u32 = args[1]
        .parse()
        .map_err(|_| JobError::Invalid("invalid process group".into()))?;
    let mut pipe = unsafe { File::from_raw_fd(fd) };
    let mut sink = [0u8; 32];
    while pipe.read(&mut sink)? != 0 {}
    let _ = signal_group(pgid, libc::SIGTERM);
    thread::sleep(Duration::from_secs(2));
    if group_exists(pgid) {
        let _ = signal_group(pgid, libc::SIGKILL);
    }
    Ok(())
}

pub(crate) fn exit_code(status: ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|signal| -signal))
}

pub(crate) fn process_start_ticks(pid: u32) -> Result<u64, JobError> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let close = stat
        .rfind(')')
        .ok_or_else(|| JobError::Invalid("malformed proc stat".into()))?;
    stat[close + 2..]
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| JobError::Invalid("proc start time missing".into()))?
        .parse()
        .map_err(|_| JobError::Invalid("invalid proc start time".into()))
}

fn signal_group(pgid: u32, signal: i32) -> Result<(), JobError> {
    let result = unsafe { libc::kill(-(pgid as i32), signal) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(JobError::Io(error));
        }
    }
    Ok(())
}

fn group_exists(pgid: u32) -> bool {
    unsafe { libc::kill(-(pgid as i32), 0) == 0 }
}

fn pipe() -> Result<(OwnedFd, OwnedFd), JobError> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(JobError::Io(std::io::Error::last_os_error()));
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn clear_cloexec(fd: i32) -> Result<(), JobError> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        return Err(JobError::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

use std::os::unix::fs::OpenOptionsExt;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_failure_and_reversal_never_invent_wall_time_or_duration() {
        assert_eq!(epoch_millis(UNIX_EPOCH - Duration::from_millis(1)), None);
        let mut observation = ExecutionObservation {
            basis: EXECUTION_OBSERVATION_BASIS.into(),
            start_observed_at_epoch_millis: Some(2_000),
            exit_observed_at_epoch_millis: None,
            observed_run_duration_millis: None,
        };
        complete_observation(
            &mut observation,
            UNIX_EPOCH + Duration::from_millis(1_000),
            Duration::from_millis(345),
        );
        assert_eq!(observation.exit_observed_at_epoch_millis, None);
        assert_eq!(observation.observed_run_duration_millis, Some(345));
    }
}
