//! Cross-platform process liveness and termination helpers.

use std::collections::HashSet;
#[cfg(unix)]
use std::io;
#[cfg(windows)]
use std::process::Command;
#[cfg(windows)]
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessTerminateOutcome {
    pub stopped: bool,
    pub forced: bool,
    pub detail: String,
}

/// Returns the operating-system creation time for one live process. Callers
/// use this together with a durable launch timestamp to reject PID reuse; an
/// unavailable or unparseable identity is an error rather than weak liveness.
pub fn process_started_at(pid: u32) -> anyhow::Result<DateTime<Utc>> {
    if pid == 0 {
        anyhow::bail!("invalid runtime pid: 0");
    }
    #[cfg(target_os = "linux")]
    {
        return linux_process_started_at(pid);
    }
    #[cfg(windows)]
    {
        return windows_process_started_at(pid);
    }
    #[cfg(target_os = "macos")]
    {
        return super::macos_proc::stat(pid)?
            .with_context(|| format!("failed to read process identity of pid {pid}"))?
            .started_at()
            .context("process creation time is out of range");
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = pid;
        anyhow::bail!("process creation time is unsupported on this platform");
    }
}

/// Executable image of a live process (`/proc/<pid>/exe` on Linux,
/// `proc_pidpath` on macOS). A process that does not exist is
/// `ErrorKind::NotFound`, like the Linux read_link.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn process_executable(pid: u32) -> std::io::Result<std::path::PathBuf> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/exe"))
    }
    #[cfg(target_os = "macos")]
    {
        super::macos_proc::exe(pid)?.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, format!("no process {pid}"))
        })
    }
}

/// Current working directory of a live process (`/proc/<pid>/cwd` on
/// Linux, `PROC_PIDVNODEPATHINFO` on macOS); a missing process is
/// `ErrorKind::NotFound`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn process_cwd(pid: u32) -> std::io::Result<std::path::PathBuf> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/cwd"))
    }
    #[cfg(target_os = "macos")]
    {
        super::macos_proc::cwd(pid)?.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, format!("no process {pid}"))
        })
    }
}

/// Wall-clock time this operating-system boot started. Every process alive
/// now started after it, so a runtime occurrence recorded before it belongs
/// to a previous boot and cannot be running any more (its PIDs may since have
/// been reused by unrelated processes).
pub fn system_boot_time() -> anyhow::Result<DateTime<Utc>> {
    #[cfg(target_os = "linux")]
    {
        let seconds = std::fs::read_to_string("/proc/stat")?
            .lines()
            .find_map(|line| line.strip_prefix("btime "))
            .context("/proc/stat omitted btime")?
            .trim()
            .parse::<i64>()
            .context("/proc/stat btime is invalid")?;
        return DateTime::from_timestamp(seconds, 0).context("boot time is out of range");
    }
    #[cfg(windows)]
    {
        let uptime_ms = unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() };
        let uptime = chrono::Duration::milliseconds(i64::try_from(uptime_ms).unwrap_or(i64::MAX));
        return Utc::now()
            .checked_sub_signed(uptime)
            .context("boot time is out of range");
    }
    #[cfg(target_os = "macos")]
    {
        return super::macos_proc::boot_time();
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        anyhow::bail!("system boot time is unsupported on this platform");
    }
}

#[cfg(target_os = "linux")]
fn linux_process_started_at(pid: u32) -> anyhow::Result<DateTime<Utc>> {
    let stat_path = format!("/proc/{pid}/stat");
    let stat = std::fs::read_to_string(&stat_path)
        .with_context(|| format!("failed to read process identity {stat_path}"))?;
    let command_end = stat
        .rfind(')')
        .context("process stat omitted its command terminator")?;
    let fields = stat[command_end + 1..]
        .split_whitespace()
        .collect::<Vec<_>>();
    // The slice begins at field 3 (`state`); starttime is field 22.
    let start_ticks = fields
        .get(19)
        .context("process stat omitted starttime")?
        .parse::<u64>()
        .context("process stat starttime is invalid")?;
    let boot_time = std::fs::read_to_string("/proc/stat")?
        .lines()
        .find_map(|line| line.strip_prefix("btime "))
        .context("/proc/stat omitted btime")?
        .trim()
        .parse::<i64>()
        .context("/proc/stat btime is invalid")?;
    let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks_per_second <= 0 {
        anyhow::bail!("sysconf(_SC_CLK_TCK) returned {ticks_per_second}");
    }
    let ticks_per_second = ticks_per_second as u64;
    let seconds = boot_time
        .checked_add((start_ticks / ticks_per_second) as i64)
        .context("process creation time overflow")?;
    let nanos = ((start_ticks % ticks_per_second) * 1_000_000_000 / ticks_per_second) as u32;
    DateTime::from_timestamp(seconds, nanos).context("process creation time is out of range")
}

#[cfg(windows)]
fn windows_process_started_at(pid: u32) -> anyhow::Result<DateTime<Utc>> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    unsafe {
        let mut handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            handle = OpenProcess(PROCESS_QUERY_INFORMATION, 0, pid);
        }
        if handle.is_null() {
            anyhow::bail!("failed to open runtime pid {pid} for creation-time verification");
        }
        let zero_filetime = || FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut creation = zero_filetime();
        let mut exit = zero_filetime();
        let mut kernel = zero_filetime();
        let mut user = zero_filetime();
        let ok = GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) != 0;
        CloseHandle(handle);
        if !ok {
            anyhow::bail!("failed to read creation time for runtime pid {pid}");
        }
        let windows_ticks =
            ((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64;
        const WINDOWS_TO_UNIX_EPOCH_100NS: u64 = 116_444_736_000_000_000;
        let unix_ticks = windows_ticks
            .checked_sub(WINDOWS_TO_UNIX_EPOCH_100NS)
            .context("runtime creation time predates the Unix epoch")?;
        let seconds = (unix_ticks / 10_000_000) as i64;
        let nanos = ((unix_ticks % 10_000_000) * 100) as u32;
        DateTime::from_timestamp(seconds, nanos).context("process creation time is out of range")
    }
}

pub fn terminate_process_and_wait(
    pid: u32,
    force: bool,
) -> anyhow::Result<ProcessTerminateOutcome> {
    if pid == 0 {
        anyhow::bail!("invalid runtime pid: 0");
    }
    let tracked_pids = process_tree_pids(pid);
    if !any_process_is_running(&tracked_pids) {
        return Ok(ProcessTerminateOutcome {
            stopped: true,
            forced: false,
            detail: "process_already_exited".to_string(),
        });
    }
    // macOS: no cgroup / job object holds the runtime's descendants, so the
    // whole family (process group, session and parent tree) gets SIGTERM,
    // as `systemctl kill --kill-whom=all` does for a Linux scope.
    #[cfg(target_os = "macos")]
    let graceful = terminate_process_tree(&tracked_pids, false);
    #[cfg(not(target_os = "macos"))]
    let graceful = terminate_process(pid, false);
    if let Err(err) = graceful {
        if !force {
            return Err(err);
        }
    }
    if wait_for_processes_exit(&tracked_pids, Duration::from_secs(2)) {
        return Ok(ProcessTerminateOutcome {
            stopped: true,
            forced: false,
            detail: "terminated".to_string(),
        });
    }
    if force {
        // Descendants started while the family was stopping join it.
        #[cfg(target_os = "macos")]
        let tracked_pids = macos_refresh_family(&tracked_pids);
        terminate_process_tree(&tracked_pids, true)?;
        if wait_for_processes_exit(&tracked_pids, Duration::from_secs(2)) {
            return Ok(ProcessTerminateOutcome {
                stopped: true,
                forced: true,
                detail: "force_killed".to_string(),
            });
        }
        return Ok(ProcessTerminateOutcome {
            stopped: false,
            forced: true,
            detail: "force_kill_timeout".to_string(),
        });
    }
    Ok(ProcessTerminateOutcome {
        stopped: false,
        forced: false,
        detail: "terminate_timeout".to_string(),
    })
}

fn process_tree_pids(pid: u32) -> Vec<u32> {
    #[cfg(windows)]
    {
        windows_process_tree_pids(pid).unwrap_or_else(|_| vec![pid])
    }
    #[cfg(target_os = "macos")]
    {
        super::macos_proc::snapshot()
            .map(|snapshot| macos_family(&[pid], &snapshot))
            .unwrap_or_else(|_| vec![pid])
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        vec![pid]
    }
}

/// The process family of `roots` on macOS: the roots, every process in a
/// process group or session one of them leads (or led, when it exited), and their descendants by
/// parent PID, repeated until nothing is added. Roots come first and children
/// after their parents, so a reverse walk signals leaves first. A runtime is
/// started with `setsid()`, so its group and session hold every descendant
/// that did not start its own session; those are found through the parent
/// tree while their parent lives.
#[cfg(target_os = "macos")]
pub(crate) fn macos_family(roots: &[u32], snapshot: &[super::macos_proc::ProcStat]) -> Vec<u32> {
    let mut family: Vec<u32> = Vec::new();
    for root in roots {
        if !family.contains(root) {
            family.push(*root);
        }
    }
    // A root that already exited still names its group and session: the
    // kernel never hands out a PID that is a live process group ID, so
    // members with that group or session are the root's.
    let absent: Vec<u32> = roots
        .iter()
        .copied()
        .filter(|root| !snapshot.iter().any(|p| p.pid == *root))
        .collect();
    loop {
        let leaders: Vec<u32> = snapshot
            .iter()
            .filter(|p| family.contains(&p.pid) && (p.pgid == p.pid || p.sid == p.pid))
            .map(|p| p.pid)
            .chain(absent.iter().copied())
            .collect();
        let added: Vec<u32> = snapshot
            .iter()
            .filter(|p| p.pid > 1 && !family.contains(&p.pid))
            .filter(|p| {
                family.contains(&p.ppid) || leaders.contains(&p.pgid) || leaders.contains(&p.sid)
            })
            .map(|p| p.pid)
            .collect();
        if added.is_empty() {
            return family;
        }
        family.extend(added);
    }
}

#[cfg(target_os = "macos")]
fn macos_refresh_family(tracked: &[u32]) -> Vec<u32> {
    let Ok(snapshot) = super::macos_proc::snapshot() else {
        return tracked.to_vec();
    };
    let mut family = tracked.to_vec();
    for pid in macos_family(tracked, &snapshot) {
        if !family.contains(&pid) {
            family.push(pid);
        }
    }
    family
}

pub fn any_process_is_running(pids: &[u32]) -> bool {
    pids.iter().copied().any(process_is_running)
}

fn wait_for_processes_exit(pids: &[u32], timeout: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if !any_process_is_running(pids) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    !any_process_is_running(pids)
}

fn terminate_process_tree(pids: &[u32], force: bool) -> anyhow::Result<()> {
    let mut last_error = None;
    for pid in pids.iter().rev().copied() {
        if !process_is_running(pid) {
            continue;
        }
        if let Err(err) = terminate_process(pid, force) {
            last_error = Some(err);
        }
    }
    if any_process_is_running(pids) {
        if let Some(err) = last_error {
            return Err(err);
        }
    }
    Ok(())
}

fn terminate_process(pid: u32, force: bool) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        if pid > i32::MAX as u32 {
            anyhow::bail!("runtime pid exceeds platform range: {pid}");
        }
        let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
        let result = unsafe { libc::kill(pid as libc::pid_t, signal) };
        if result == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if matches!(err.raw_os_error(), Some(code) if code == libc::ESRCH) {
            return Ok(());
        }
        return Err(err).with_context(|| format!("Failed to terminate runtime pid {pid}"));
    }
    #[cfg(windows)]
    {
        if force {
            let script = format!(
                "if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ Stop-Process -Id {pid} -Force -ErrorAction Stop }}"
            );
            let status = Command::new("powershell")
                .args(["-NoProfile", "-Command", &script])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .with_context(|| format!("Failed to start Stop-Process for runtime pid {pid}"))?;
            if status.success() || !process_is_running(pid) {
                return Ok(());
            }
        }
        let mut command = Command::new("taskkill");
        command.arg("/PID").arg(pid.to_string()).arg("/T");
        if force {
            command.arg("/F");
        }
        let status = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .with_context(|| format!("Failed to start taskkill for runtime pid {pid}"))?;
        if status.success() || !process_is_running(pid) {
            return Ok(());
        }
        anyhow::bail!("taskkill failed for runtime pid {pid} with status {status}");
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        let _ = force;
        anyhow::bail!("runtime termination is not supported on this platform");
    }
}

#[cfg(windows)]
fn windows_process_tree_pids(root: u32) -> anyhow::Result<Vec<u32>> {
    let script = format!(
        r#"$ErrorActionPreference = "Stop"
$root = {root}
$processes = Get-CimInstance Win32_Process | Select-Object ProcessId, ParentProcessId
$known = New-Object 'System.Collections.Generic.HashSet[uint32]'
[void]$known.Add([uint32]$root)
$changed = $true
while ($changed) {{
  $changed = $false
  foreach ($proc in $processes) {{
    if ($known.Contains([uint32]$proc.ParentProcessId) -and -not $known.Contains([uint32]$proc.ProcessId)) {{
      [void]$known.Add([uint32]$proc.ProcessId)
      $changed = $true
    }}
  }}
}}
$known | Sort-Object
"#
    );
    let output = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .stdin(Stdio::null())
        .output()
        .context("Failed to query Windows process tree")?;
    if !output.status.success() {
        anyhow::bail!(
            "Windows process tree query failed with status {}",
            output.status
        );
    }
    let mut pids = output
        .stdout
        .split(|byte| *byte == b'\n' || *byte == b'\r')
        .filter_map(|line| std::str::from_utf8(line).ok())
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .collect::<Vec<_>>();
    if !pids.contains(&root) {
        pids.push(root);
    }
    pids.sort_unstable();
    pids.dedup();
    Ok(pids)
}

pub fn process_is_running(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        if pid > i32::MAX as u32 {
            return false;
        }
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if result == 0 {
            return true;
        }
        let err = io::Error::last_os_error();
        !matches!(err.raw_os_error(), Some(code) if code == libc::ESRCH)
    }
    #[cfg(windows)]
    {
        running_process_ids(&[pid]).contains(&pid)
    }
    #[cfg(not(any(unix, windows)))]
    {
        true
    }
}

/// The PIDs of `pids` that are running. On Windows each PID is asked directly
/// (`OpenProcess`); only the ones that cannot be answered that way (access
/// denied: a protected or other-session process) share one process snapshot,
/// so a batch costs at most one fallback however many PIDs it checks.
pub fn running_process_ids(pids: &[u32]) -> HashSet<u32> {
    #[cfg(windows)]
    {
        running_process_ids_with_fallback(
            pids,
            windows_process_is_running_without_tasklist,
            windows_fallback_running_process_ids,
        )
    }
    #[cfg(not(windows))]
    {
        pids.iter()
            .copied()
            .filter(|pid| *pid > 0 && process_is_running(*pid))
            .collect()
    }
}

#[cfg(any(windows, test))]
fn running_process_ids_with_fallback(
    pids: &[u32],
    mut check: impl FnMut(u32) -> Option<bool>,
    fallback: impl FnOnce() -> Option<HashSet<u32>>,
) -> HashSet<u32> {
    let mut requested = pids
        .iter()
        .copied()
        .filter(|pid| *pid > 0)
        .collect::<Vec<_>>();
    requested.sort_unstable();
    requested.dedup();

    let mut running = HashSet::new();
    let mut unresolved = Vec::new();
    for pid in requested {
        match check(pid) {
            Some(true) => {
                running.insert(pid);
            }
            Some(false) => {}
            None => unresolved.push(pid),
        }
    }
    if unresolved.is_empty() {
        return running;
    }

    let Some(fallback_running) = fallback() else {
        return running;
    };
    running.extend(
        unresolved
            .into_iter()
            .filter(|pid| fallback_running.contains(pid)),
    );
    running
}

#[cfg(windows)]
fn windows_process_is_running_without_tasklist(pid: u32) -> Option<bool> {
    use windows_sys::Win32::System::Threading::PROCESS_QUERY_INFORMATION;
    use windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;

    windows_process_is_running_with_access(pid, PROCESS_QUERY_LIMITED_INFORMATION)
        .or_else(|| windows_process_is_running_with_access(pid, PROCESS_QUERY_INFORMATION))
}

#[cfg(windows)]
fn windows_process_is_running_with_access(pid: u32, access: u32) -> Option<bool> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Foundation::STILL_ACTIVE;
    use windows_sys::Win32::System::Threading::GetExitCodeProcess;
    use windows_sys::Win32::System::Threading::OpenProcess;

    unsafe {
        let handle = OpenProcess(access, 0, pid);
        if handle.is_null() {
            return open_process_failure_liveness(
                windows_sys::Win32::Foundation::GetLastError(),
            );
        }
        let mut exit_code = 0u32;
        let ok = GetExitCodeProcess(handle, &mut exit_code) != 0;
        CloseHandle(handle);
        ok.then_some(exit_code == STILL_ACTIVE as u32)
    }
}

/// What a failed `OpenProcess` says about the PID. `ERROR_INVALID_PARAMETER`
/// is Windows' answer for a PID that names no process: definitely not running
/// (before, this went to the `tasklist` fallback, about 2.4 s per dead PID).
/// Anything else (`ERROR_ACCESS_DENIED` for a protected or elevated process)
/// says nothing about liveness: unknown, resolved by the fallback.
#[cfg(any(windows, test))]
fn open_process_failure_liveness(error: u32) -> Option<bool> {
    const ERROR_INVALID_PARAMETER: u32 = 87;
    (error == ERROR_INVALID_PARAMETER).then_some(false)
}

/// The running PIDs for the ones `OpenProcess` could not answer: a ToolHelp
/// snapshot (milliseconds), else `tasklist` (seconds) if the snapshot fails.
#[cfg(windows)]
fn windows_fallback_running_process_ids() -> Option<HashSet<u32>> {
    windows_snapshot_running_process_ids().or_else(windows_tasklist_running_process_ids)
}

#[cfg(windows)]
fn windows_snapshot_running_process_ids() -> Option<HashSet<u32>> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..PROCESSENTRY32W::default()
    };
    let mut running = HashSet::new();
    let mut available = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while available {
        running.insert(entry.th32ProcessID);
        available = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    unsafe {
        CloseHandle(snapshot);
    }
    // An empty walk is a failed snapshot, not "nothing runs" (this process does).
    (!running.is_empty()).then_some(running)
}

#[cfg(windows)]
fn windows_tasklist_running_process_ids() -> Option<HashSet<u32>> {
    let output = Command::new("tasklist")
        .args(["/FO", "CSV", "/NH"])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(tasklist_csv_pid_field)
            .filter_map(|field| field.parse::<u32>().ok())
            .collect(),
    )
}

#[cfg(windows)]
fn tasklist_csv_pid_field(line: &str) -> Option<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' if in_quotes && chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                fields.push(std::mem::take(&mut field));
            }
            _ => field.push(ch),
        }
    }
    fields.push(field);
    fields.get(1).map(|field| field.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_process_failures_mean_gone_or_unknown() {
        // No such process: not running, no fallback.
        assert_eq!(open_process_failure_liveness(87), Some(false));
        // Access denied (protected / elevated / other session): unknown.
        assert_eq!(open_process_failure_liveness(5), None);
        assert_eq!(open_process_failure_liveness(0), None);
    }

    #[test]
    fn a_batch_takes_at_most_one_fallback() {
        let mut fallbacks = 0;
        let running = running_process_ids_with_fallback(
            &[10, 11, 12, 13, 13, 0],
            |pid| match pid {
                10 => Some(true),
                11 => Some(false),
                _ => None,
            },
            || {
                fallbacks += 1;
                Some(HashSet::from([12]))
            },
        );
        assert_eq!(fallbacks, 1);
        assert_eq!(running, HashSet::from([10, 12]));

        // Every PID answered directly (alive or gone): no fallback at all.
        let mut fallbacks = 0;
        let running = running_process_ids_with_fallback(
            &[10, 11],
            |pid| Some(pid == 10),
            || {
                fallbacks += 1;
                None
            },
        );
        assert_eq!(fallbacks, 0);
        assert_eq!(running, HashSet::from([10]));
    }

    #[cfg(windows)]
    #[test]
    fn a_dead_pid_is_answered_directly_and_fast() {
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cmd");
        let pid = child.id();
        child.wait().expect("wait cmd");
        // Dropping the Child closes its process handle: the PID then names no process.
        drop(child);
        let started = Instant::now();
        assert_eq!(windows_process_is_running_without_tasklist(pid), Some(false));
        assert!(!process_is_running(pid));
        assert!(running_process_ids(&[pid]).is_empty());
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "dead PID check took {:?} (tasklist fallback?)",
            started.elapsed()
        );
    }

    #[cfg(windows)]
    #[test]
    fn protected_and_live_pids_are_running() {
        let me = std::process::id();
        assert_eq!(windows_process_is_running_without_tasklist(me), Some(true));
        let snapshot = windows_snapshot_running_process_ids().expect("process snapshot");
        assert!(snapshot.contains(&me));
        // PID 4 is the System process: always there, and without elevation
        // OpenProcess may be refused; either way it is running.
        assert!(snapshot.contains(&4));
        assert!(process_is_running(4));
        assert_eq!(running_process_ids(&[me, 4]), HashSet::from([me, 4]));
    }

    #[test]
    fn process_zero_is_not_running() {
        assert!(!process_is_running(0));
        assert!(!any_process_is_running(&[0]));
    }

    #[test]
    fn current_process_creation_time_is_observable_and_not_in_the_future() {
        let started_at = process_started_at(std::process::id()).expect("current process identity");
        let now = Utc::now();
        assert!(started_at <= now);
        assert!(now - started_at < chrono::Duration::days(1));
    }

    #[test]
    fn running_process_ids_filters_zero_dead_and_duplicate_pids() {
        let current = std::process::id();
        let running = running_process_ids(&[0, current, current, u32::MAX]);

        assert_eq!(running, HashSet::from([current]));
    }

    #[test]
    fn running_process_ids_uses_one_bulk_fallback_for_unresolved_pids() {
        use std::cell::Cell;

        let fallback_calls = Cell::new(0);
        let running = running_process_ids_with_fallback(
            &[0, 10, 20, 20, 30],
            |pid| match pid {
                10 => Some(true),
                20 => Some(false),
                _ => None,
            },
            || {
                fallback_calls.set(fallback_calls.get() + 1);
                Some(HashSet::from([30, 40]))
            },
        );

        assert_eq!(running, HashSet::from([10, 30]));
        assert_eq!(fallback_calls.get(), 1);
    }

    #[test]
    fn terminate_process_rejects_zero_pid() {
        assert!(terminate_process_and_wait(0, false).is_err());
    }

    /// macOS: a runtime started in its own session is stopped with its whole
    /// family: a child in the same group, a grandchild that ignores SIGTERM
    /// and a grandchild that started its own session.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_terminate_stops_the_whole_process_family() {
        use std::io::BufRead;
        use std::os::unix::process::CommandExt;
        let script = "import os,time,signal,sys\n\
pids=[]\n\
a=os.fork()\n\
if a==0:\n time.sleep(600)\n\
pids.append(a)\n\
r,w=os.pipe()\n\
b=os.fork()\n\
if b==0:\n signal.signal(signal.SIGTERM,signal.SIG_IGN)\n c=os.fork()\n if c==0:\n  os.setsid()\n  signal.signal(signal.SIGTERM,signal.SIG_IGN)\n  os.write(w,(str(os.getpid())+'\\n').encode())\n  time.sleep(600)\n time.sleep(600)\n\
pids.append(b)\n\
line=os.read(r,64).decode().strip()\n\
pids.append(int(line))\n\
print(' '.join(map(str,pids)),flush=True)\n\
time.sleep(600)\n";
        let mut command = std::process::Command::new("/usr/bin/python3");
        command.args(["-c", script]).stdout(std::process::Stdio::piped());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        let descendants: Vec<u32> = line.split_whitespace().map(|p| p.parse().unwrap()).collect();
        assert_eq!(descendants.len(), 3);
        let root = child.id();
        let family = process_tree_pids(root);
        for pid in std::iter::once(root).chain(descendants.iter().copied()) {
            assert!(family.contains(&pid), "{pid} missing from {family:?}");
        }
        assert!(!family.contains(&std::process::id()));
        // Without force the SIGTERM-ignoring members survive.
        let soft = terminate_process_and_wait(root, false).unwrap();
        assert!(!soft.stopped);
        let _ = child.try_wait();
        let hard = terminate_process_and_wait(root, true).unwrap();
        let _ = child.wait();
        assert!(hard.stopped, "{hard:?}");
        for pid in descendants {
            assert!(!process_is_running(pid) || super::super::macos_proc::stat(pid).unwrap().is_some_and(|s| s.zombie));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_family_walks_groups_sessions_and_parents() {
        use super::super::macos_proc::ProcStat;
        let p = |pid, ppid, pgid, sid| ProcStat { pid, ppid, pgid, sid, start_ticks: 1, zombie: false, uid: 0 };
        let snapshot = vec![
            p(1, 0, 1, 1),
            p(100, 1, 100, 100),  // root, session leader
            p(101, 100, 100, 100), // child
            p(102, 1, 100, 100),  // orphan still in the root's session
            p(103, 101, 103, 103), // grandchild with its own session
            p(104, 103, 103, 103), // its child
            p(200, 1, 200, 200),  // unrelated
        ];
        let family = macos_family(&[100], &snapshot);
        assert_eq!(family[0], 100);
        for pid in [101, 102, 103, 104] {
            assert!(family.contains(&pid));
        }
        assert!(!family.contains(&200) && !family.contains(&1));
        assert_eq!(macos_family(&[999], &snapshot), vec![999]);
        // The root exited: its session members are still its family.
        let without_root: Vec<_> = snapshot.into_iter().filter(|p| p.pid != 100).collect();
        let family = macos_family(&[100], &without_root);
        for pid in [101, 102, 103, 104] {
            assert!(family.contains(&pid));
        }
        assert!(!family.contains(&200));
    }
}
