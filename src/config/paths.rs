//! Filesystem layout for cutex runtime and profile state.

use std::fs;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;

/// Marker file that makes a directory an accepted `CUTEX_TEST_PRIVATE_HOME`.
pub const PRIVATE_HOME_MARKER: &str = ".cutex-test-private-home";

/// Debug builds only: a non-empty value other than `0` makes this process
/// refuse the OS user's real home (`home_dir()` returns `None`, `config_dir()`
/// fails). The repo's `.cargo/config.toml` sets it for every process cargo
/// starts (`cargo test`, `cargo run`), so `cutex` children spawned by tests
/// inherit it. Release builds ignore it.
pub const FORBID_REAL_HOME_ENV: &str = "CUTEX_TEST_FORBID_REAL_HOME";

// Test-home guard (incident 2026-10-09: an unfiltered `cargo test` without
// `CUTEX_TEST_PRIVATE_HOME` imported the live `~/.cutex` session store into
// SQLite). Test code must never resolve the real profile:
// - lib unit tests (`cfg(test)`) and every debug test-harness process (lib,
//   bin and integration test binaries in `target/<profile>/deps/`) use `HOME`
//   only when it is not the real profile, else a per-process private temp
//   home (with the marker);
// - debug `cutex` children with `CUTEX_TEST_FORBID_REAL_HOME` refuse the
//   real home;
// - `config_dir()` and the session-store importer refuse the real `~/.cutex`
//   in all of those processes (defence in depth, also covers `CUTEX_HOME`).

/// One lock for every library test that changes `HOME`: test homes with
/// separate locks raced each other ("Profile not found" in parallel runs).
#[cfg(test)]
pub(crate) fn test_home_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
pub fn home_dir() -> Option<PathBuf> {
    Some(guarded_test_home(environment_home()))
}

#[cfg(not(test))]
pub fn home_dir() -> Option<PathBuf> {
    // A test runner must never silently fall through to the real user home.
    #[cfg(not(debug_assertions))]
    if std::env::var_os("CUTEX_TEST_PRIVATE_HOME").is_some() {
        return None;
    }
    #[cfg(debug_assertions)]
    {
        if std::env::var_os("CUTEX_TEST_PRIVATE_HOME").is_some() {
            return private_test_home_dir();
        }
        if in_test_harness() {
            return Some(guarded_test_home(environment_home()));
        }
        #[cfg(windows)]
        if let Some(environment_home) = environment_home() {
            let native_home = dirs::home_dir();
            if native_home.as_ref() != Some(&environment_home) {
                return None;
            }
        }
        if forbid_real_home_env() {
            return dirs::home_dir().filter(|home| !is_real_user_home(home));
        }
    }
    dirs::home_dir()
}

fn environment_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(all(not(test), debug_assertions))]
fn private_test_home_dir() -> Option<PathBuf> {
    let boundary = std::env::var_os("CUTEX_TEST_PRIVATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)?;
    if !boundary.is_dir() || !boundary.join(PRIVATE_HOME_MARKER).is_file() {
        return None;
    }
    // `HOME` still pointing at the real profile (a shell default) never wins
    // over the private boundary.
    environment_home()
        .filter(|home| home.is_dir() && !is_real_user_home(home))
        .or(Some(boundary))
        .filter(|home| !is_real_user_home(home))
}

/// The OS account's real profile directory, independent of `HOME`
/// (Windows: the known Profile folder; Unix: the passwd entry).
pub fn real_user_home() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        if let Some(home) = passwd_home() {
            return Some(home);
        }
    }
    dirs::home_dir()
}

#[cfg(unix)]
fn passwd_home() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let status = unsafe {
        libc::getpwuid_r(
            libc::getuid(),
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() || entry.pw_dir.is_null() {
        return None;
    }
    let bytes = unsafe { std::ffi::CStr::from_ptr(entry.pw_dir) }.to_bytes();
    (!bytes.is_empty()).then(|| PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}

/// True when `path` is the OS user's real profile directory.
pub fn is_real_user_home(path: &Path) -> bool {
    use crate::config::instance::comparable;
    real_user_home().is_some_and(|real| comparable(path) == comparable(&real))
}

/// True in a cargo test process: `cfg(test)` code, or (debug builds) a test
/// harness binary under `target/<profile>/deps/`.
pub fn in_test_harness() -> bool {
    if cfg!(test) {
        return true;
    }
    #[cfg(debug_assertions)]
    {
        static HARNESS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *HARNESS.get_or_init(|| {
            std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().and_then(Path::file_name).map(|n| n == "deps"))
                .unwrap_or(false)
        })
    }
    #[cfg(not(debug_assertions))]
    false
}

#[cfg(debug_assertions)]
fn forbid_real_home_env() -> bool {
    std::env::var_os(FORBID_REAL_HOME_ENV).is_some_and(|value| !value.is_empty() && value != "0")
}

/// True when this process must never use the real `~/.cutex` (tests, and
/// debug processes told so by `CUTEX_TEST_FORBID_REAL_HOME`).
pub fn real_home_forbidden() -> bool {
    if in_test_harness() {
        return true;
    }
    #[cfg(debug_assertions)]
    {
        if forbid_real_home_env() {
            return true;
        }
    }
    false
}

/// Fails when this process must not touch the real home and `path` is the
/// real `~/.cutex` or inside it.
pub fn ensure_not_real_state(path: &Path) -> anyhow::Result<()> {
    if !real_home_forbidden() {
        return Ok(());
    }
    use crate::config::instance::{comparable, is_within};
    let Some(real) = real_user_home() else {
        return Ok(());
    };
    let real_state = comparable(&real.join(".cutex"));
    let candidate = comparable(path);
    if candidate == real_state || is_within(&candidate, &real_state) {
        anyhow::bail!(
            "refusing to use the real cutex home {} from a test process (set CUTEX_TEST_PRIVATE_HOME and HOME to a private test dir)",
            path.display()
        );
    }
    Ok(())
}

/// `HOME` when it is not the real profile, else this process's private temp
/// home.
fn guarded_test_home(candidate: Option<PathBuf>) -> PathBuf {
    match candidate {
        Some(home) if !is_real_user_home(&home) => home,
        _ => auto_test_home(),
    }
}

/// Per-process private test home (created once, with the marker). Each use is
/// appended to `real-home-fallbacks.log` in it (thread name = test name), to
/// find tests that do not set up their own home.
fn auto_test_home() -> PathBuf {
    static AUTO_HOME: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    let root = AUTO_HOME.get_or_init(|| {
        let root = std::env::temp_dir().join(format!(
            "cutex-auto-test-home-{}-{}",
            std::process::id(),
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ));
        assert!(
            !is_real_user_home(&root),
            "cutex test guard: temp dir resolves to the real user home"
        );
        fs::create_dir_all(&root)
            .and_then(|()| fs::write(root.join(PRIVATE_HOME_MARKER), b""))
            .unwrap_or_else(|error| {
                panic!(
                    "cutex test guard: cannot create the private test home {}: {error}",
                    root.display()
                )
            });
        root
    });
    if let Ok(mut log) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("real-home-fallbacks.log"))
    {
        use std::io::Write;
        let thread = std::thread::current();
        let line = format!("{}\n", thread.name().unwrap_or("<unnamed>"));
        let _ = log.write_all(line.as_bytes());
    }
    root.clone()
}

pub fn config_dir() -> anyhow::Result<PathBuf> {
    // `CUTEX_HOME` selects an independent instance (docs/INSTANCES.md).
    let dir = match crate::config::instance::custom_home()? {
        Some(home) => home,
        None => home_dir()
            .context("Could not determine home directory")?
            .join(".cutex"),
    };
    ensure_not_real_state(&dir)?;
    Ok(dir)
}

/// Legacy layouts resolve inside an independent instance, where they never
/// exist: an instance never reads or migrates the OS user's legacy dirs.
fn instance_legacy_dir(name: &str) -> anyhow::Result<Option<PathBuf>> {
    Ok(crate::config::instance::custom_home()?.map(|home| home.join("legacy-disabled").join(name)))
}

pub fn legacy_config_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) = instance_legacy_dir(".codez-cli")? {
        return Ok(dir);
    }
    let home = home_dir().context("Could not determine home directory")?;
    Ok(home.join(".codez-cli"))
}

pub fn runtime_dir() -> anyhow::Result<PathBuf> {
    Ok(config_dir()?.join("runtime"))
}

pub fn host_codex_home_dir() -> anyhow::Result<PathBuf> {
    Ok(config_dir()?.join("codex-home"))
}

pub fn legacy_host_codex_home_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) = instance_legacy_dir(".codex-codez")? {
        return Ok(dir);
    }
    let home = home_dir().context("Could not determine home directory")?;
    Ok(home.join(".codex-codez"))
}

pub fn docker_runtime_home_dir() -> anyhow::Result<PathBuf> {
    Ok(runtime_dir()?.join("docker-home"))
}

pub fn legacy_docker_runtime_home_dir() -> anyhow::Result<PathBuf> {
    Ok(runtime_dir()?.join("thirdparty").join("userhome"))
}

pub fn login_runtime_root() -> anyhow::Result<PathBuf> {
    Ok(runtime_dir()?.join("login"))
}

pub fn migrate_legacy_runtime_layout() -> anyhow::Result<()> {
    if crate::config::instance::is_isolated() {
        return Ok(());
    }
    migrate_dir_if_needed(&legacy_config_dir()?, &config_dir()?)?;
    migrate_dir_if_needed(&legacy_host_codex_home_dir()?, &host_codex_home_dir()?)?;
    Ok(())
}

fn migrate_dir_if_needed(from: &Path, to: &Path) -> anyhow::Result<()> {
    if !from.exists() || to.exists() {
        return Ok(());
    }

    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "Failed to create migration parent dir: {}",
                parent.display()
            )
        })?;
    }

    fs::rename(from, to).with_context(|| {
        format!(
            "Failed to migrate legacy directory {} -> {}",
            from.display(),
            to.display()
        )
    })?;
    Ok(())
}

pub fn accounts_path() -> anyhow::Result<PathBuf> {
    Ok(config_dir()?.join("accounts.json"))
}

pub fn quick_state_path() -> anyhow::Result<PathBuf> {
    Ok(config_dir()?.join("state.json"))
}

pub fn config_path() -> anyhow::Result<PathBuf> {
    Ok(config_dir()?.join("config.json"))
}

#[cfg(test)]
mod guard_tests {
    use super::*;
    use crate::config::instance::comparable;

    fn real_state_dir() -> PathBuf {
        real_user_home()
            .expect("the test host has a real user home")
            .join(".cutex")
    }

    #[test]
    fn test_code_never_resolves_the_real_home() {
        // Whatever HOME the runner has (often the real profile).
        let resolved = home_dir().expect("tests always resolve a home");
        assert!(!is_real_user_home(&resolved), "{}", resolved.display());
        let config = config_dir().expect("config dir");
        assert_ne!(comparable(&config), comparable(&real_state_dir()));
    }

    #[test]
    fn unset_or_real_home_falls_back_to_a_private_temp_home() {
        let real = real_user_home().expect("real user home");
        for candidate in [None, Some(real.clone()), Some(real.join("."))] {
            let home = guarded_test_home(candidate);
            assert!(!is_real_user_home(&home), "{}", home.display());
            assert!(home.starts_with(std::env::temp_dir()));
            assert!(home.join(PRIVATE_HOME_MARKER).is_file());
        }
        let other = std::env::temp_dir().join("cutex-guard-explicit-home");
        assert_eq!(guarded_test_home(Some(other.clone())), other);
    }

    #[test]
    fn real_state_is_refused_under_test() {
        assert!(real_home_forbidden());
        assert!(in_test_harness());
        let real = real_state_dir();
        assert!(ensure_not_real_state(&real).is_err());
        assert!(ensure_not_real_state(&real.join("cutex-sessions.json")).is_err());
        assert!(
            ensure_not_real_state(&real.join("runtime").join("agent-bus-audit.jsonl")).is_err()
        );
        let other = std::env::temp_dir()
            .join("cutex-guard-other")
            .join(".cutex");
        assert!(ensure_not_real_state(&other).is_ok());
    }

    #[test]
    fn an_independent_instance_on_the_real_home_is_refused_under_test() {
        let real = real_state_dir();
        crate::config::instance::set_test_selection(crate::config::instance::Selection::Custom(
            real,
        ));
        let result = config_dir();
        crate::config::instance::set_test_selection(crate::config::instance::Selection::Default);
        assert!(result.is_err());
    }
}
