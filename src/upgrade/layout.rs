//! New-install layout (cutex-desktop backend-auto-update RFC §4.1):
//!
//! ```text
//! <base>/                    Linux $XDG_DATA_HOME/cutex (~/.local/share/cutex)
//!                            Windows %LOCALAPPDATA%\cutex
//!   install.json             InstallRecord (written by `cutex service install`)
//!   releases/<release>/      cutex[.exe], cutex-mcp[.exe], manifest.json(+.minisig);
//!                            immutable: pinned agents' bundles point into it
//!   natives/<version>/       codex, codex-app-server, codex-code-mode-host, schema
//!   runtimes/claude-rN/      (P1: not installed by cutex yet)
//!   current -> releases/<r>  Linux: symlink, swapped atomically
//!   bin/cutex.exe            Windows: fixed path, rename swap (RFC §10.10)
//!   staging/<id>/            downloads and unpacking
//!   upgrade/                 settings.json, state.json, history.jsonl, runs/<id>/
//!   run/                     supervisor status / control (Windows)
//! ~/.cutex/                  state, layout unchanged
//! ```
//!
//! Only a base with `install.json` is a managed install. Upgrades refuse to
//! act on anything else, so the owner's manually deployed hosts (P3) are
//! never touched by this code.

use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const INSTALL_BASE_ENV: &str = "CUTEX_INSTALL_BASE";
pub const INSTALL_RECORD_FILE: &str = "install.json";
pub const INSTALL_SCHEMA: &str = "cutex/install/v1";

pub const CUTEX_EXE: &str = if cfg!(windows) { "cutex.exe" } else { "cutex" };
pub const FACADE_EXE: &str = if cfg!(windows) { "cutex-mcp.exe" } else { "cutex-mcp" };

/// What `cutex service install` recorded. Unknown fields are kept on
/// rewrite (`extra`), so an older binary never drops a newer one's fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallRecord {
    pub schema: String,
    /// `linux` | `windows`.
    pub os: String,
    /// `systemd` | `cutex-supervisor`.
    pub service_manager: String,
    /// `release` | `main`.
    pub channel: String,
    pub created_at: String,
    /// The release the services run (Linux: also `current`).
    pub active_release: String,
    /// The release that ran before the last swap (rollback target).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_release: Option<String>,
    /// systemd unit directory used at install (Linux).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit_dir: Option<String>,
    /// Task Scheduler task name (Windows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_name: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub base: PathBuf,
}

/// A release / native version / run label usable as one path component.
pub fn valid_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 64
        && label != "."
        && label != ".."
        && label.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

pub fn ensure_label(label: &str, what: &str) -> anyhow::Result<()> {
    ensure!(valid_label(label), "invalid {what} `{label}` (expected [A-Za-z0-9._-], at most 64 chars)");
    Ok(())
}

/// Platform default base, or `CUTEX_INSTALL_BASE`.
pub fn default_base() -> Option<PathBuf> {
    if let Some(base) = std::env::var_os(INSTALL_BASE_ENV).filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(base));
    }
    if cfg!(windows) {
        return std::env::var_os("LOCALAPPDATA").map(|dir| PathBuf::from(dir).join("cutex"));
    }
    // A test process never follows the real XDG data dir.
    let test = std::env::var_os("CUTEX_TEST_PRIVATE_HOME").is_some();
    if !test {
        if let Some(data) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
            let data = PathBuf::from(data);
            if data.is_absolute() {
                return Some(data.join("cutex"));
            }
        }
    }
    crate::config::paths::home_dir().map(|home| home.join(".local/share/cutex"))
}

impl Layout {
    pub fn at(base: impl Into<PathBuf>) -> Self {
        Self { base: base.into() }
    }

    /// The default base (managed or not).
    pub fn default_layout() -> Option<Self> {
        default_base().map(Self::at)
    }

    /// The managed install this process belongs to: `CUTEX_INSTALL_BASE`,
    /// else derived from the running executable (`<base>/releases/<r>/cutex`,
    /// `<base>/current/cutex`, `<base>/bin/cutex.exe`); only when its
    /// `install.json` exists. Never the platform default by itself: a
    /// hand-deployed binary (owner hosts) is unmanaged even when a managed
    /// install exists elsewhere on the machine.
    pub fn detect() -> Option<Self> {
        if let Some(base) = std::env::var_os(INSTALL_BASE_ENV).filter(|v| !v.is_empty()) {
            let layout = Self::at(base);
            return layout.is_managed().then_some(layout);
        }
        let exe = std::env::current_exe().ok()?;
        Self::from_executable(&exe).filter(Self::is_managed)
    }

    /// The base an executable path implies, if it sits in a layout position.
    pub fn from_executable(exe: &Path) -> Option<Self> {
        let dir = exe.parent()?;
        let name = dir.file_name()?.to_str()?;
        let parent = dir.parent()?;
        if name == "bin" || name == "current" {
            return Some(Self::at(parent));
        }
        if parent.file_name()?.to_str()? == "releases" {
            return Some(Self::at(parent.parent()?));
        }
        None
    }

    pub fn is_managed(&self) -> bool {
        self.install_record_path().is_file()
    }

    pub fn install_record_path(&self) -> PathBuf {
        self.base.join(INSTALL_RECORD_FILE)
    }
    pub fn releases_dir(&self) -> PathBuf {
        self.base.join("releases")
    }
    pub fn release_dir(&self, release: &str) -> PathBuf {
        self.releases_dir().join(release)
    }
    pub fn natives_dir(&self) -> PathBuf {
        self.base.join("natives")
    }
    pub fn native_dir(&self, version: &str) -> PathBuf {
        self.natives_dir().join(version)
    }
    pub fn runtimes_dir(&self) -> PathBuf {
        self.base.join("runtimes")
    }
    pub fn current_link(&self) -> PathBuf {
        self.base.join("current")
    }
    pub fn bin_dir(&self) -> PathBuf {
        self.base.join("bin")
    }
    pub fn staging_dir(&self) -> PathBuf {
        self.base.join("staging")
    }
    pub fn upgrade_dir(&self) -> PathBuf {
        self.base.join("upgrade")
    }
    pub fn settings_path(&self) -> PathBuf {
        self.upgrade_dir().join("settings.json")
    }
    pub fn state_path(&self) -> PathBuf {
        self.upgrade_dir().join("state.json")
    }
    pub fn history_path(&self) -> PathBuf {
        self.upgrade_dir().join("history.jsonl")
    }
    pub fn runs_dir(&self) -> PathBuf {
        self.upgrade_dir().join("runs")
    }
    pub fn run_dir(&self, run_id: &str) -> PathBuf {
        self.runs_dir().join(run_id)
    }
    pub fn lock_path(&self) -> PathBuf {
        self.upgrade_dir().join("upgrade.lock")
    }
    pub fn supervisor_dir(&self) -> PathBuf {
        self.base.join("run")
    }

    /// Where the services start cutex from: `current/cutex` (Linux) or
    /// `bin\cutex.exe` (Windows).
    pub fn service_executable(&self) -> PathBuf {
        if cfg!(windows) {
            self.bin_dir().join(CUTEX_EXE)
        } else {
            self.current_link().join(CUTEX_EXE)
        }
    }

    pub fn release_executable(&self, release: &str) -> PathBuf {
        self.release_dir(release).join(CUTEX_EXE)
    }

    pub fn read_install(&self) -> anyhow::Result<InstallRecord> {
        let path = self.install_record_path();
        let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let record: InstallRecord =
            serde_json::from_slice(&bytes).with_context(|| format!("invalid {}", path.display()))?;
        ensure!(record.schema == INSTALL_SCHEMA, "unsupported install record schema `{}`", record.schema);
        Ok(record)
    }

    pub fn write_install(&self, record: &InstallRecord) -> anyhow::Result<()> {
        crate::config::atomic::write_private_pretty_json_atomic(
            &self.install_record_path(),
            record,
            "cutex install record",
        )
    }

    /// The release the services run now: the `current` link target on
    /// Unix (when present), else the install record.
    pub fn active_release(&self) -> anyhow::Result<String> {
        #[cfg(unix)]
        if let Ok(target) = std::fs::read_link(self.current_link()) {
            if let Some(name) = target.file_name().and_then(|n| n.to_str()) {
                return Ok(name.to_string());
            }
        }
        Ok(self.read_install()?.active_release)
    }

    /// Releases present under `releases/`, by name.
    pub fn installed_releases(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.releases_dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.path().join(CUTEX_EXE).is_file())
            .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
            .filter(|name| valid_label(name))
            .collect();
        names.sort();
        names
    }
}

/// Atomically point `current` at `releases/<release>` (Unix symlink:
/// `current.tmp` then rename over `current`).
#[cfg(unix)]
pub fn point_current(layout: &Layout, release: &str) -> anyhow::Result<()> {
    ensure_label(release, "release")?;
    let target = PathBuf::from("releases").join(release);
    ensure!(layout.base.join(&target).join(CUTEX_EXE).is_file(), "release {release} is not installed");
    let tmp = layout.base.join(format!("current.tmp-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(&target, &tmp).with_context(|| format!("create {}", tmp.display()))?;
    std::fs::rename(&tmp, layout.current_link()).context("swap current link")?;
    Ok(())
}

/// Copy `src` to `dst` through a temp file in `dst`'s directory (never a
/// partially written executable at `dst`); keeps the executable bit.
pub fn copy_file_atomic(src: &Path, dst: &Path) -> anyhow::Result<()> {
    let parent = dst.parent().context("destination without parent")?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        dst.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::copy(src, &tmp).with_context(|| format!("copy {} to {}", src.display(), tmp.display()))?;
    if let Err(error) = std::fs::rename(&tmp, dst) {
        let _ = std::fs::remove_file(&tmp);
        bail!("install {}: {error}", dst.display());
    }
    Ok(())
}

/// Windows fixed-path swap (RFC §4.4 / §10.10): the running `bin\cutex.exe`
/// cannot be overwritten or deleted but can be renamed. Moves `dst` aside to
/// `<dst>.old-<tag>` (an older leftover of the same name is replaced) and
/// copies `src` in. Returns the aside path when there was a file to move.
/// Portable (also used on Unix in tests).
pub fn rename_swap(src: &Path, dst: &Path, tag: &str) -> anyhow::Result<Option<PathBuf>> {
    ensure_label(tag, "swap tag")?;
    let aside = dst.with_file_name(format!(
        "{}.old-{tag}",
        dst.file_name().and_then(|n| n.to_str()).context("destination name")?
    ));
    let moved = if dst.exists() {
        if aside.exists() {
            // A leftover from an earlier swap; if it is still mapped by a
            // running process, keep it under a unique name instead.
            if std::fs::remove_file(&aside).is_err() {
                let unique = aside.with_extension(format!("stale-{}", uuid::Uuid::new_v4().simple()));
                std::fs::rename(&aside, unique).context("move stale swap leftover")?;
            }
        }
        std::fs::rename(dst, &aside).with_context(|| format!("rename {} aside", dst.display()))?;
        Some(aside)
    } else {
        None
    };
    if let Err(error) = copy_file_atomic(src, dst) {
        if let Some(aside) = &moved {
            let _ = std::fs::rename(aside, dst);
        }
        return Err(error);
    }
    Ok(moved)
}

/// Best-effort removal of earlier swap leftovers in `dir` (`<exe>.old-<tag>`
/// and `.stale-<uuid>` names from [`rename_swap`]) except `keep` (the files just
/// moved aside). A leftover still mapped by a running process cannot be
/// deleted on Windows; it stays and goes on a later swap. Rollback never needs
/// these (it re-activates from `releases/<r>/`). Returns how many were removed.
pub fn prune_swap_leftovers(dir: &Path, keep: &[PathBuf]) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        let leftover = [CUTEX_EXE, FACADE_EXE]
            .iter()
            .any(|exe| name.starts_with(&format!("{exe}.old-")) || name.starts_with(&format!("{exe}.stale-")));
        if !leftover || keep.iter().any(|k| k == &path) || !path.is_file() {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cutex-layout-{tag}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn record(release: &str) -> InstallRecord {
        InstallRecord {
            schema: INSTALL_SCHEMA.into(),
            os: std::env::consts::OS.into(),
            service_manager: "systemd".into(),
            channel: "release".into(),
            created_at: "2026-10-15T00:00:00Z".into(),
            active_release: release.into(),
            previous_release: None,
            unit_dir: None,
            task_name: None,
            extra: Default::default(),
        }
    }

    #[test]
    fn labels() {
        for ok in ["r240", "0.162.1-r53", "claude-r12", "dev-5f30a7c"] {
            assert!(valid_label(ok), "{ok}");
        }
        for bad in ["", ".", "..", "a/b", "a\\b", "r 1", &"x".repeat(65)] {
            assert!(!valid_label(bad), "{bad}");
        }
    }

    #[test]
    fn executable_positions_imply_the_base() {
        let base = PathBuf::from("/opt/x/cutex");
        assert_eq!(Layout::from_executable(&base.join("releases/r240/cutex")), Some(Layout::at(&base)));
        assert_eq!(Layout::from_executable(&base.join("current/cutex")), Some(Layout::at(&base)));
        assert_eq!(Layout::from_executable(&base.join("bin/cutex.exe")), Some(Layout::at(&base)));
        assert_eq!(Layout::from_executable(Path::new("/usr/local/lib/cutex")), None);
    }

    #[test]
    fn install_record_round_trips_unknown_fields() {
        let dir = temp("record");
        let layout = Layout::at(&dir);
        assert!(!layout.is_managed());
        let mut rec = record("r240");
        rec.extra.insert("futureField".into(), serde_json::json!({"a": 1}));
        layout.write_install(&rec).unwrap();
        assert!(layout.is_managed());
        let back = layout.read_install().unwrap();
        assert_eq!(back, rec);
        let text = std::fs::read_to_string(layout.install_record_path()).unwrap();
        assert!(text.contains("futureField"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn current_link_swaps_atomically() {
        let dir = temp("current");
        let layout = Layout::at(&dir);
        for release in ["r1", "r2"] {
            std::fs::create_dir_all(layout.release_dir(release)).unwrap();
            std::fs::write(layout.release_executable(release), release).unwrap();
        }
        layout.write_install(&record("r1")).unwrap();
        point_current(&layout, "r1").unwrap();
        assert_eq!(layout.active_release().unwrap(), "r1");
        point_current(&layout, "r2").unwrap();
        assert_eq!(layout.active_release().unwrap(), "r2");
        assert_eq!(std::fs::read_to_string(layout.service_executable()).unwrap(), "r2");
        assert!(point_current(&layout, "r3").is_err());
        assert!(point_current(&layout, "../x").is_err());
        assert_eq!(layout.installed_releases(), vec!["r1".to_string(), "r2".to_string()]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rename_swap_keeps_the_old_file_aside() {
        let dir = temp("swap");
        let new = dir.join("new");
        std::fs::write(&new, "new").unwrap();
        let dst = dir.join("bin/cutex.exe");
        assert_eq!(rename_swap(&new, &dst, "r1").unwrap(), None);
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "new");
        std::fs::write(&new, "newer").unwrap();
        let aside = rename_swap(&new, &dst, "r1").unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(&aside).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "newer");
        // A second swap with the same tag replaces the leftover.
        std::fs::write(&new, "newest").unwrap();
        rename_swap(&new, &dst, "r1").unwrap();
        assert_eq!(std::fs::read_to_string(&aside).unwrap(), "newer");
        assert!(rename_swap(&new, &dst, "../x").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn prune_removes_older_leftovers_but_keeps_the_new_aside() {
        let dir = temp("prune");
        let old1 = dir.join(format!("{CUTEX_EXE}.old-r1"));
        let stale = dir.join(format!("{FACADE_EXE}.stale-abc"));
        let keep = dir.join(format!("{CUTEX_EXE}.old-r2"));
        let other = dir.join("notes.old-r1");
        for path in [&old1, &stale, &keep, &other] {
            std::fs::write(path, "x").unwrap();
        }
        assert_eq!(prune_swap_leftovers(&dir, &[keep.clone()]), 2);
        assert!(!old1.exists() && !stale.exists());
        assert!(keep.exists() && other.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
