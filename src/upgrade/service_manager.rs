//! Which service manager runs this daemon (`/v2/host` `serviceManager`).
//!
//! - `CUTEX_SERVICE_MANAGER` (set by the units `cutex service install`
//!   writes and by the `cutex service run` supervisor): `systemd` |
//!   `cutex-supervisor`;
//! - else Linux: the process runs in a `.service` cgroup (the owner hosts'
//!   hand-written units), the unit comes from `/proc/self/cgroup`
//!   (`INVOCATION_ID` alone is inherited by hand-started processes);
//! - else Windows: a published runtime-host endpoint (owner hosts);
//! - else `none` (started by hand / by `management serve` itself).

use serde_json::{json, Value};

pub const SERVICE_MANAGER_ENV: &str = "CUTEX_SERVICE_MANAGER";
pub const SYSTEMD: &str = "systemd";
pub const SUPERVISOR: &str = "cutex-supervisor";
pub const RUNTIME_HOST: &str = "runtime-host";
pub const NONE: &str = "none";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Detected {
    pub kind: &'static str,
    pub unit: Option<String>,
}

pub fn detect() -> Detected {
    detect_with(
        std::env::var(SERVICE_MANAGER_ENV).ok().as_deref(),
        std::env::var_os("INVOCATION_ID").is_some(),
        cgroup_unit(),
        runtime_host_published(),
    )
}

fn detect_with(env: Option<&str>, invocation_id: bool, unit: Option<String>, runtime_host: bool) -> Detected {
    match env.map(str::trim) {
        Some(SYSTEMD) => return Detected { kind: SYSTEMD, unit },
        Some(SUPERVISOR) => return Detected { kind: SUPERVISOR, unit: None },
        _ => {}
    }
    // `INVOCATION_ID` alone is not enough: it is inherited by everything a
    // systemd-started process launches (a terminal, a desktop app), so a
    // daemon started by hand from there looked systemd-managed. The process
    // must sit in a `.service` cgroup.
    if cfg!(target_os = "linux") && unit.is_some() {
        let _ = invocation_id;
        return Detected { kind: SYSTEMD, unit };
    }
    if cfg!(windows) && runtime_host {
        return Detected { kind: RUNTIME_HOST, unit: None };
    }
    Detected { kind: NONE, unit: None }
}

/// `cutex-management-api.service` from `0::/user.slice/…/cutex-management-api.service`.
fn cgroup_unit() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
        return parse_cgroup_unit(&text);
    }
    #[allow(unreachable_code)]
    None
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_cgroup_unit(text: &str) -> Option<String> {
    text.lines()
        .filter_map(|line| line.rsplit(':').next())
        .filter_map(|path| path.rsplit('/').next())
        .find(|unit| unit.ends_with(".service") && unit.len() <= 128)
        .map(str::to_string)
}

fn runtime_host_published() -> bool {
    if !cfg!(windows) {
        return false;
    }
    let dir = std::env::var_os("RUNTIME_HOST_STATE_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(|d| std::path::PathBuf::from(d).join("runtime-host")));
    dir.is_some_and(|dir| dir.join("endpoint.json").is_file())
}

/// `/v2/host` `serviceManager`: `{kind, unit, managedInstall}`.
pub fn host_view() -> Value {
    let detected = detect();
    json!({
        "kind": detected.kind,
        "unit": detected.unit,
        "managedInstall": super::layout::Layout::detect().is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_environment_wins() {
        assert_eq!(detect_with(Some("systemd"), false, None, true).kind, SYSTEMD);
        assert_eq!(detect_with(Some("cutex-supervisor"), true, Some("x.service".into()), true).kind, SUPERVISOR);
    }

    #[test]
    fn falls_back_by_platform() {
        let none = detect_with(None, false, None, false);
        assert_eq!(none, Detected { kind: NONE, unit: None });
        let systemd = detect_with(None, true, Some("cutex-management-api.service".into()), false);
        if cfg!(target_os = "linux") {
            assert_eq!(systemd.kind, SYSTEMD);
            assert_eq!(systemd.unit.as_deref(), Some("cutex-management-api.service"));
        } else {
            assert_eq!(systemd.kind, NONE);
        }
        // Inherited INVOCATION_ID, but a scope cgroup (hand-run from a terminal).
        assert_eq!(detect_with(None, true, None, false), Detected { kind: NONE, unit: None });
        let host = detect_with(None, false, None, true);
        assert_eq!(host.kind, if cfg!(windows) { RUNTIME_HOST } else { NONE });
    }

    #[test]
    fn cgroup_unit_parsing() {
        let v2 = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/cutex-management-api.service\n";
        assert_eq!(parse_cgroup_unit(v2).as_deref(), Some("cutex-management-api.service"));
        assert_eq!(parse_cgroup_unit("0::/user.slice/user-1000.slice/session-2.scope\n"), None);
        assert_eq!(parse_cgroup_unit(""), None);
    }
}
