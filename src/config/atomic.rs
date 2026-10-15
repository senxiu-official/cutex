//! Atomic file writing helpers for cutex JSON and runtime state files.

use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use anyhow::anyhow;
use anyhow::Context;
use serde::Serialize;
use uuid::Uuid;

#[cfg(windows)]
static WINDOWS_ATOMIC_WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn write_pretty_json_atomic<T: Serialize + ?Sized>(
    path: &Path,
    value: &T,
    label: &str,
) -> anyhow::Result<()> {
    let data = serde_json::to_vec_pretty(value)
        .with_context(|| format!("Failed to serialize {label}: {}", path.display()))?;
    write_bytes_atomic(path, &data)
        .with_context(|| format!("Failed to write {label}: {}", path.display()))
}

pub fn write_private_pretty_json_atomic<T: Serialize + ?Sized>(
    path: &Path,
    value: &T,
    label: &str,
) -> anyhow::Result<()> {
    let data = serde_json::to_vec_pretty(value)
        .with_context(|| format!("Failed to serialize {label}: {}", path.display()))?;
    write_bytes_atomic_with_privacy(path, &data, true)
        .with_context(|| format!("Failed to write {label}: {}", path.display()))
}

pub fn write_private_bytes_atomic(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    write_bytes_atomic_with_privacy(path, data, true)
        .with_context(|| format!("Failed to write private file: {}", path.display()))
}

pub fn write_bytes_atomic(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    write_bytes_atomic_with_privacy(path, data, false)
}

fn write_bytes_atomic_with_privacy(
    path: &Path,
    data: &[u8],
    owner_only: bool,
) -> anyhow::Result<()> {
    #[cfg(not(unix))]
    let _ = owner_only;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent directory: {}", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("Failed to create parent dir: {}", parent.display()))?;

    // Windows can reject simultaneous replacements of the same destination
    // with ERROR_ACCESS_DENIED even though every writer uses a distinct temp
    // file. Keep in-process state writers ordered; the bounded retry in
    // `replace_file_atomic` also covers a short-lived cross-process share lock.
    #[cfg(windows)]
    let _windows_atomic_write_guard = WINDOWS_ATOMIC_WRITE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let tmp_path = atomic_write_temp_path(path);
    let result = (|| -> anyhow::Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if owner_only {
            use std::os::unix::fs::OpenOptionsExt;

            options.mode(0o600);
        }
        let mut file = options
            .open(&tmp_path)
            .with_context(|| format!("Failed to create temp file: {}", tmp_path.display()))?;
        file.write_all(data)
            .with_context(|| format!("Failed to write temp file: {}", tmp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("Failed to sync temp file: {}", tmp_path.display()))?;
        drop(file);
        replace_file_atomic(&tmp_path, path)
            .with_context(|| format!("Failed to replace {}", path.display()))?;
        #[cfg(unix)]
        if owner_only {
            use std::os::unix::fs::PermissionsExt;

            fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                .with_context(|| format!("Failed to secure private file {}", path.display()))?;
        }
        sync_parent_dir_after_atomic_replace(path)
            .with_context(|| format!("Failed to sync parent dir for {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

fn atomic_write_temp_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|value| value.to_string_lossy())
        .unwrap_or_else(|| "cutex".into());
    path.with_file_name(format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ))
}

#[cfg(unix)]
fn replace_file_atomic(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

#[cfg(windows)]
fn replace_file_atomic(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::time::Duration;

    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    const ERROR_LOCK_VIOLATION: i32 = 33;
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;
    const MAX_ATTEMPTS: u32 = 8;

    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(
            existing_file_name: *const u16,
            new_file_name: *const u16,
            flags: u32,
        ) -> i32;
    }

    fn wide_null(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let from = wide_null(from);
    let to = wide_null(to);
    let mut delay_ms = 1;
    for attempt in 0..MAX_ATTEMPTS {
        let ok = unsafe {
            MoveFileExW(
                from.as_ptr(),
                to.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if ok != 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        let retryable = matches!(
            error.raw_os_error(),
            Some(ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)
        );
        if !retryable || attempt + 1 == MAX_ATTEMPTS {
            return Err(error);
        }
        std::thread::sleep(Duration::from_millis(delay_ms));
        delay_ms = (delay_ms * 2).min(32);
    }
    unreachable!("Windows atomic replace loop returns on success or final failure")
}

#[cfg(not(any(unix, windows)))]
fn replace_file_atomic(from: &Path, to: &Path) -> io::Result<()> {
    if to.exists() {
        fs::remove_file(to)?;
    }
    fs::rename(from, to)
}

#[cfg(unix)]
fn sync_parent_dir_after_atomic_replace(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent_dir_after_atomic_replace(_path: &Path) -> io::Result<()> {
    Ok(())
}
