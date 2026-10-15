//! Lifetime guard for Windows services launched through a tracked shell.
//!
//! The Windows Desktop service manifest uses `cmd.exe /c` to establish the
//! small, non-secret environment required by the hosted Cutex services. The
//! Runtime Host owns that shell process. Windows does not automatically end a
//! console child when its parent shell is terminated, so the child must retain
//! and wait on the launcher's process handle to preserve the hosted-runtime
//! lifetime boundary.

use std::io;
use std::mem::size_of;
use std::thread;

use anyhow::Context;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, WaitForSingleObject, INFINITE, PROCESS_SYNCHRONIZE,
};

/// Ends this hosted service when the Runtime Host's tracked launcher exits.
///
/// The parent is resolved once and held by process handle, so PID reuse cannot
/// redirect the guard. Callers should arm this only for the two Windows hosted
/// service entrypoints, after the provider-set launcher marker is validated.
pub fn arm_launcher_exit_guard() -> anyhow::Result<()> {
    let parent_process_id = current_parent_process_id()?;
    let parent_handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, parent_process_id) };
    if parent_handle.is_null() {
        return Err(io::Error::last_os_error()).with_context(|| {
            format!("failed to open hosted launcher process {parent_process_id}")
        });
    }

    // Raw Windows handles are pointer aliases and therefore not `Send`. The
    // integer value is moved to the dedicated waiter and reconstructed there;
    // that thread is the sole owner after a successful spawn.
    let transferable_handle = parent_handle as usize;
    let spawn = thread::Builder::new()
        .name("cutex-hosted-launcher-guard".to_owned())
        .spawn(move || {
            let parent_handle = transferable_handle as HANDLE;
            let wait = unsafe { WaitForSingleObject(parent_handle, INFINITE) };
            unsafe {
                CloseHandle(parent_handle);
            }
            if wait == WAIT_OBJECT_0 {
                std::process::exit(0);
            }
        });
    if let Err(error) = spawn {
        unsafe {
            CloseHandle(parent_handle);
        }
        return Err(error).context("failed to start hosted launcher lifetime guard");
    }
    Ok(())
}

fn current_parent_process_id() -> anyhow::Result<u32> {
    let current_process_id = unsafe { GetCurrentProcessId() };
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error())
            .context("failed to snapshot Windows processes for hosted launcher guard");
    }

    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..PROCESSENTRY32W::default()
    };
    let mut found = None;
    let mut available = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while available {
        if entry.th32ProcessID == current_process_id {
            found = Some(entry.th32ParentProcessID);
            break;
        }
        available = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    unsafe {
        CloseHandle(snapshot);
    }

    match found {
        Some(parent_process_id) if parent_process_id != 0 => Ok(parent_process_id),
        Some(_) => anyhow::bail!("hosted Cutex process has no parent launcher"),
        None => anyhow::bail!(
            "current process {current_process_id} was absent from the Windows process snapshot"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_one_live_parent_handle() {
        let parent_process_id = current_parent_process_id().expect("parent process is visible");
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, parent_process_id) };
        assert!(!handle.is_null());
        unsafe {
            CloseHandle(handle);
        }
    }
}
