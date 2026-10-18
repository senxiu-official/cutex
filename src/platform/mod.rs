//! OS-specific process, terminal, filesystem, and permission helpers.
//!
//! Target boundary: centralize `cfg(unix)` / `cfg(windows)` behavior here
//! instead of scattering platform checks through domain logic.

pub mod command;
pub mod file_stamp;
pub mod heap;
pub mod shared_assets;
pub mod host;
#[cfg(target_os = "macos")]
pub mod macos_proc;
pub mod owner_policy;
#[cfg(windows)]
pub mod private_fs;
pub mod process;
#[cfg(windows)]
pub mod windows_parent_guard;
#[cfg(windows)]
pub mod windows_spawn;

use std::time::SystemTime;
use std::time::UNIX_EPOCH;

pub fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
