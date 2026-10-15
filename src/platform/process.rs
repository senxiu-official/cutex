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
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = pid;
        anyhow::bail!("process creation time is unsupported on this platform");
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
    #[cfg(not(any(target_os = "linux", windows)))]
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
    if let Err(err) = terminate_process(pid, false) {
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
    #[cfg(not(windows))]
    {
        vec![pid]
    }
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

pub fn running_process_ids(pids: &[u32]) -> HashSet<u32> {
    #[cfg(windows)]
    {
        running_process_ids_with_fallback(
            pids,
            windows_process_is_running_without_tasklist,
            windows_tasklist_running_process_ids,
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
            return None;
        }
        let mut exit_code = 0u32;
        let ok = GetExitCodeProcess(handle, &mut exit_code) != 0;
        CloseHandle(handle);
        ok.then_some(exit_code == STILL_ACTIVE as u32)
    }
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
}
