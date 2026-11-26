//! Host memory signals sampled by the Management maintenance thread.

/// Returns host pressure, or None if the host signal is unavailable.
/// /proc/meminfo covers the host and can miss pressure inside a memory cgroup.
#[cfg(target_os = "linux")]
pub(crate) fn sample(already_low: bool) -> Option<bool> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kib = |field: &str| -> Option<u64> {
        info.lines()
            .find_map(|line| line.strip_prefix(field))?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    };
    Some(low_headroom(
        kib("MemAvailable:")? * 1024,
        kib("MemTotal:")? * 1024,
        already_low,
    ))
}

#[cfg(windows)]
pub(crate) fn sample(already_low: bool) -> Option<bool> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: status is writable and dwLength describes its full size.
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        return None;
    }
    Some(
        low_headroom(status.ullAvailPhys, status.ullTotalPhys, already_low)
            || low_headroom(
                status.ullAvailPageFile,
                status.ullTotalPageFile,
                already_low,
            ),
    )
}

#[cfg(target_os = "macos")]
pub(crate) fn sample(_already_low: bool) -> Option<bool> {
    let mut level = 0u32;
    let mut size = std::mem::size_of_val(&level);
    // SAFETY: level is writable for size bytes. Null newp requests a read.
    let result = unsafe {
        libc::sysctlbyname(
            c"kern.memorystatus_vm_pressure_level".as_ptr(),
            (&mut level as *mut u32).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    match (result, size, level) {
        (0, 4, 1) => Some(false),
        (0, 4, 2 | 4) => Some(true),
        _ => None,
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub(crate) fn sample(_already_low: bool) -> Option<bool> {
    None
}

#[cfg(any(windows, target_os = "linux", test))]
fn low_headroom(available: u64, total: u64, already_low: bool) -> bool {
    const MIB: u64 = 1024 * 1024;
    let threshold = if already_low {
        (total / 12).clamp(384 * MIB, 3072 * MIB)
    } else {
        (total / 20).clamp(256 * MIB, 2048 * MIB)
    };
    available < threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headroom_has_hysteresis_and_is_bounded_on_large_hosts() {
        let mib = 1024 * 1024;
        assert!(low_headroom(400 * mib, 10_000 * mib, false));
        assert!(!low_headroom(600 * mib, 10_000 * mib, false));
        assert!(low_headroom(600 * mib, 10_000 * mib, true));
        assert!(!low_headroom(900 * mib, 10_000 * mib, true));
        assert!(!low_headroom(4000 * mib, 1_000_000 * mib, true));
    }
}
