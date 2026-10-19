//! Per-managed-Agent Windows Job objects (the Windows counterpart of the Linux
//! systemd scope in `runtime::process_scope`).
//!
//! Lifetime: the job is named and has `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
//! Its only long-lived handle is duplicated INTO the runtime root process
//! itself; the spawning daemon closes its own handle right after the launch.
//! So the job (and its name) lives exactly as long as the runtime root:
//! - management serve may exit / restart / upgrade while the runtime keeps
//!   running and is adopted again by its durable binding (no daemon handle);
//! - when the root exits for any reason (stop by PID, crash, Task Manager)
//!   the kernel closes its handle table, the last job handle closes and every
//!   remaining descendant (Claude Code, shells, dev servers, builds) is killed;
//! - an explicit stop opens the job by name and calls `TerminateJobObject`.
//!
//! No `BREAKAWAY_OK` limit: descendants cannot leave the job, and processes
//! started by the job's members join it (nested jobs need Windows 8+).
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::time::{Duration, Instant};

use anyhow::{ensure, Context};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::SystemServices::{JOB_OBJECT_QUERY, JOB_OBJECT_TERMINATE};
use windows_sys::Win32::System::Threading::*;

/// Exit code of processes killed by an explicit Agent stop.
const AGENT_JOB_EXIT_CODE: u32 = 1;
const JOB_POLL_INTERVAL: Duration = Duration::from_millis(50);

fn check(ok: i32) -> anyhow::Result<()> {
    if ok == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn wide(value: &str) -> anyhow::Result<Vec<u16>> {
    let mut result: Vec<u16> = OsStr::new(value).encode_wide().collect();
    ensure!(!result.contains(&0), "NUL in job object name");
    result.push(0);
    Ok(result)
}

/// Create the Agent's named job (current-user DACL, kill on last handle
/// close). An existing job with the same name means a previous runtime root
/// of this Agent is still alive: fail closed, like `systemd-run --unit` does
/// for an active unit, instead of joining it.
pub fn create_agent_job(name: &str) -> anyhow::Result<OwnedHandle> {
    let (job, existed) = create_or_open(name)?;
    ensure!(
        !existed,
        "managed Agent job {name} still exists: a previous runtime of this Agent is alive; stop the Agent first"
    );
    configure_new_job(job)
}

/// Like [`create_agent_job`], but join the Agent's existing job (a previous
/// runtime root of the Agent still alive, e.g. an adapter whose startup was
/// not confirmed). Both trees then end together with the job.
pub fn create_or_join_agent_job(name: &str) -> anyhow::Result<OwnedHandle> {
    let (job, existed) = create_or_open(name)?;
    if !existed {
        return configure_new_job(job);
    }
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    check(unsafe {
        QueryInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&mut limits as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of_val(&limits) as u32,
            std::ptr::null_mut(),
        )
    })
    .with_context(|| format!("failed to inspect existing managed Agent job {name}"))?;
    ensure!(
        limits.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE != 0,
        "existing job {name} is not a managed Agent job"
    );
    Ok(job)
}

fn create_or_open(name: &str) -> anyhow::Result<(OwnedHandle, bool)> {
    let wide_name = wide(name)?;
    let raw = unsafe { CreateJobObjectW(std::ptr::null(), wide_name.as_ptr()) };
    let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    ensure!(
        !raw.is_null(),
        "failed to create managed Agent job {name}: {}",
        std::io::Error::last_os_error()
    );
    Ok((unsafe { OwnedHandle::from_raw_handle(raw) }, existed))
}

fn configure_new_job(job: OwnedHandle) -> anyhow::Result<OwnedHandle> {
    crate::platform::private_fs::secure_runtime_object(job.as_raw_handle())
        .context("failed to secure managed Agent job")?;
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    check(unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of_val(&limits) as u32,
        )
    })
    .context("failed to set managed Agent job limits")?;
    Ok(job)
}

/// Give the runtime root its own (non-inheritable) handle to its job, so the
/// job outlives the spawning daemon but not the root.
pub fn transfer_job_handle(job: &OwnedHandle, process: HANDLE) -> anyhow::Result<()> {
    let mut retained = std::ptr::null_mut();
    check(unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            job.as_raw_handle(),
            process,
            &mut retained,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    })
    .context("failed to hand the managed Agent job to its runtime")
}

/// Place a child created with `CREATE_SUSPENDED` in `job`, hand it the job
/// handle and resume it. The child runs no instruction before it is a job
/// member, so it cannot start an unaccounted descendant. On error the caller
/// must kill the (still suspended) child.
pub fn adopt_suspended_child(job: &OwnedHandle, child: &std::process::Child) -> anyhow::Result<()> {
    let process = child.as_raw_handle();
    check(unsafe { AssignProcessToJobObject(job.as_raw_handle(), process) })
        .context("failed to assign the managed Agent runtime to its job")?;
    transfer_job_handle(job, process)?;
    resume_suspended_process(child.id())
}

/// Resume the single initial thread of a process created suspended (std's
/// `Child` does not expose its main thread handle on stable Rust).
fn resume_suspended_process(pid: u32) -> anyhow::Result<()> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    ensure!(
        snapshot != INVALID_HANDLE_VALUE,
        "failed to list runtime threads: {}",
        std::io::Error::last_os_error()
    );
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
    let mut resumed = 0;
    let mut more = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == pid {
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            ensure!(
                !thread.is_null(),
                "failed to open runtime thread: {}",
                std::io::Error::last_os_error()
            );
            let thread = unsafe { OwnedHandle::from_raw_handle(thread) };
            ensure!(
                unsafe { ResumeThread(thread.as_raw_handle()) } != u32::MAX,
                "failed to resume runtime thread: {}",
                std::io::Error::last_os_error()
            );
            resumed += 1;
        }
        more = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) } != 0;
    }
    ensure!(resumed > 0, "suspended runtime {pid} has no thread to resume");
    Ok(())
}

/// Open an existing Agent job; `None` when no job of that name exists (the
/// runtime root, which holds the last handle, has exited).
pub fn open_agent_job(name: &str) -> anyhow::Result<Option<OwnedHandle>> {
    let wide_name = wide(name)?;
    let raw = unsafe { OpenJobObjectW(JOB_OBJECT_QUERY | JOB_OBJECT_TERMINATE, 0, wide_name.as_ptr()) };
    if raw.is_null() {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) {
            return Ok(None);
        }
        return Err(error).with_context(|| format!("failed to open managed Agent job {name}"));
    }
    Ok(Some(unsafe { OwnedHandle::from_raw_handle(raw) }))
}

pub fn active_processes(job: &OwnedHandle) -> anyhow::Result<u32> {
    let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
    check(unsafe {
        QueryInformationJobObject(
            job.as_raw_handle(),
            JobObjectBasicAccountingInformation,
            (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
            std::mem::size_of_val(&info) as u32,
            std::ptr::null_mut(),
        )
    })
    .context("failed to query managed Agent job")?;
    Ok(info.ActiveProcesses)
}

pub fn terminate_job(job: &OwnedHandle) -> anyhow::Result<()> {
    check(unsafe { TerminateJobObject(job.as_raw_handle(), AGENT_JOB_EXIT_CODE) })
        .context("failed to terminate managed Agent job")
}

/// Wait until no process of the job is alive.
pub fn wait_for_job_empty(job: &OwnedHandle, timeout: Duration) -> anyhow::Result<bool> {
    let started = Instant::now();
    loop {
        if active_processes(job)? == 0 {
            return Ok(true);
        }
        if started.elapsed() >= timeout {
            return Ok(false);
        }
        std::thread::sleep(JOB_POLL_INTERVAL);
    }
}
