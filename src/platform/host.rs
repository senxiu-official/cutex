//! Host identity helpers.

use std::process::Command;
use std::sync::OnceLock;

static DETECTED_HOST_NAME: OnceLock<String> = OnceLock::new();

pub fn current_host_name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DETECTED_HOST_NAME.get_or_init(|| {
            Command::new("hostname")
                .output()
                .ok()
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "unknown".to_string())
        }).clone())
}

static OS_DESCRIPTION: OnceLock<String> = OnceLock::new();

/// Operating system and version of this host, e.g. `Windows 11 (10.0.26200)`,
/// `Ubuntu 24.04.1 LTS`, `macOS 15.1` (cached; never spawns a console window
/// on Windows).
pub fn os_description() -> String {
    OS_DESCRIPTION.get_or_init(detect_os_description).clone()
}

/// `(major, minor, build)` of the running Windows kernel (`RtlGetVersion`,
/// not subject to the manifest version lie).
#[cfg(windows)]
fn windows_kernel_version() -> Option<(u32, u32, u32)> {
    #[repr(C)]
    struct OsVersionInfoW {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform: u32,
        csd: [u16; 128],
    }
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn RtlGetVersion(info: *mut OsVersionInfoW) -> i32;
    }
    let mut info = OsVersionInfoW {
        size: std::mem::size_of::<OsVersionInfoW>() as u32,
        major: 0,
        minor: 0,
        build: 0,
        platform: 0,
        csd: [0; 128],
    };
    // SAFETY: RtlGetVersion fills the caller-sized struct and never fails
    // for OSVERSIONINFOW.
    if unsafe { RtlGetVersion(&mut info) } != 0 || info.major == 0 {
        return None;
    }
    Some((info.major, info.minor, info.build))
}

#[cfg_attr(not(windows), allow(dead_code))]
fn windows_family(major: u32, build: u32) -> &'static str {
    if major == 10 && build >= 22000 {
        "Windows 11"
    } else if major == 10 {
        "Windows 10"
    } else {
        "Windows"
    }
}

#[cfg(windows)]
fn detect_os_description() -> String {
    match windows_kernel_version() {
        Some((major, minor, build)) => {
            format!("{} ({major}.{minor}.{build})", windows_family(major, build))
        }
        None => "Windows".to_string(),
    }
}

/// `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProductName`, e.g.
/// `Windows 10 Pro` (Windows 11 still reports "Windows 10" there).
#[cfg(windows)]
fn windows_product_name() -> Option<String> {
    const HKEY_LOCAL_MACHINE: isize = 0x8000_0002_u32 as i32 as isize;
    const RRF_RT_REG_SZ: u32 = 0x0000_0002;
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn RegGetValueW(
            key: isize,
            sub_key: *const u16,
            value: *const u16,
            flags: u32,
            kind: *mut u32,
            data: *mut core::ffi::c_void,
            size: *mut u32,
        ) -> i32;
    }
    let wide = |text: &str| text.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let sub_key = wide(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion");
    let value = wide("ProductName");
    let mut buffer = [0u16; 256];
    let mut size = std::mem::size_of_val(&buffer) as u32;
    // SAFETY: both names are NUL-terminated; buffer / size describe a valid
    // writable region; RRF_RT_REG_SZ makes the result NUL-terminated.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            sub_key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if status != 0 {
        return None;
    }
    let len = buffer.iter().position(|&unit| unit == 0).unwrap_or(buffer.len());
    let text = String::from_utf16_lossy(&buffer[..len]).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// `Windows 11 Pro 10.0.26200` from the registry product name and the kernel
/// version (the family comes from the build number, the edition from the
/// product name).
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_version_text(product_name: Option<&str>, major: u32, minor: u32, build: u32) -> String {
    let family = windows_family(major, build);
    let name = match product_name.map(str::trim).filter(|name| !name.is_empty()) {
        Some(product) => match ["Windows 10", "Windows 11"]
            .iter()
            .find_map(|prefix| product.strip_prefix(prefix))
        {
            Some(edition) if edition.trim().is_empty() => family.to_string(),
            Some(edition) => format!("{family} {}", edition.trim()),
            // Server and older releases: the product name is the whole name.
            None => product.to_string(),
        },
        None => family.to_string(),
    };
    format!("{name} {major}.{minor}.{build}")
}

static OS_VERSION: OnceLock<Option<String>> = OnceLock::new();

/// Short human OS version for clients (`GET /v2/host` `osVersion`), e.g.
/// `Windows 11 Pro 10.0.26200`, `Ubuntu 24.04.1 LTS` (os-release
/// `PRETTY_NAME`), `macOS 15.1`; `None` when unknown. Cached.
pub fn os_version() -> Option<String> {
    OS_VERSION.get_or_init(detect_os_version).clone()
}

#[cfg(windows)]
fn detect_os_version() -> Option<String> {
    let (major, minor, build) = windows_kernel_version()?;
    Some(windows_version_text(windows_product_name().as_deref(), major, minor, build))
}

#[cfg(target_os = "macos")]
fn detect_os_version() -> Option<String> {
    Some(detect_os_description()).filter(|text| text != "macOS")
}

#[cfg(all(unix, not(target_os = "macos")))]
fn detect_os_version() -> Option<String> {
    std::fs::read_to_string("/etc/os-release")
        .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))
        .ok()
        .and_then(|text| os_release_pretty_name(&text))
}

#[cfg(not(any(windows, unix)))]
fn detect_os_version() -> Option<String> {
    None
}

#[cfg(target_os = "macos")]
fn detect_os_description() -> String {
    Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|version| format!("macOS {version}"))
        .unwrap_or_else(|| "macOS".to_string())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn detect_os_description() -> String {
    let pretty = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|text| os_release_pretty_name(&text));
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    match (pretty, kernel) {
        (Some(pretty), Some(kernel)) => format!("{pretty}, Linux {kernel}"),
        (Some(pretty), None) => pretty,
        (None, Some(kernel)) => format!("Linux {kernel}"),
        (None, None) => std::env::consts::OS.to_string(),
    }
}

/// `PRETTY_NAME` of an `/etc/os-release` text.
pub fn os_release_pretty_name(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix("PRETTY_NAME="))
        .map(|value| value.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|value| !value.is_empty())
}

/// The shell a Codex agent's commands run in on this host: PowerShell on
/// Windows, else the login shell (`$SHELL`) by name.
pub fn agent_shell_name() -> String {
    if cfg!(windows) {
        return "PowerShell".to_string();
    }
    std::env::var("SHELL")
        .ok()
        .and_then(|shell| {
            std::path::Path::new(shell.trim())
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "sh".to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn os_release_pretty_name_parses_quoted_values() {
        let text = "NAME=\"Ubuntu\"\nPRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\nID=ubuntu\n";
        assert_eq!(super::os_release_pretty_name(text).as_deref(), Some("Ubuntu 24.04.1 LTS"));
        assert_eq!(super::os_release_pretty_name("ID=x\n"), None);
        assert!(!super::os_description().is_empty());
    }

    #[test]
    fn windows_version_text_combines_family_edition_and_build() {
        use super::windows_version_text as text;
        assert_eq!(text(Some("Windows 10 Pro"), 10, 0, 26200), "Windows 11 Pro 10.0.26200");
        assert_eq!(text(Some("Windows 10 Home"), 10, 0, 19045), "Windows 10 Home 10.0.19045");
        assert_eq!(
            text(Some("Windows Server 2022 Datacenter"), 10, 0, 20348),
            "Windows Server 2022 Datacenter 10.0.20348"
        );
        assert_eq!(text(None, 10, 0, 22631), "Windows 11 10.0.22631");
        assert_eq!(text(Some("  "), 6, 3, 9600), "Windows 6.3.9600");
    }

    #[test]
    fn os_version_is_short_and_best_effort() {
        if let Some(version) = super::os_version() {
            assert!(!version.trim().is_empty());
            assert!(!version.contains('\n'));
            if cfg!(windows) {
                assert!(version.starts_with("Windows"), "{version}");
            }
        }
    }
}
