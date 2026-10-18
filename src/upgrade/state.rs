//! Upgrade settings, state, run records and history under `<base>/upgrade/`.
//!
//! - `settings.json` [`Settings`]: owner choices (channel, source, cadence,
//!   policies);
//! - `state.json` [`State`]: installed / available / staged release and the
//!   current or last run; written atomically by management (check / stage)
//!   and by the runner (apply / rollback) under `state.lock`;
//! - `runs/<runId>/run.json`: the full run record (+ `backup/`, `displaced/`,
//!   `runner.log`); `history.jsonl`: one line per finished run.
//!
//! Every reader ignores unknown fields and keeps them (`extra`) on rewrite.

use std::io::Write;
use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::layout::Layout;
use super::manifest::{Migration, MigrationClass, Mode};

pub const STATE_SCHEMA: &str = "cutex/upgrade-state/v1";
pub const SETTINGS_SCHEMA: &str = "cutex/upgrade-settings/v1";
pub const DEFAULT_MAX_WAIT_SECS: u64 = 3600;
pub const MAX_MAX_WAIT_SECS: u64 = 24 * 3600;
pub const DEFAULT_CHECK_INTERVAL_HOURS: u32 = 6;
pub const BACKUPS_KEPT: usize = 3;

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// When pinned agents move to the new runtime package (RFC §5.2, §10.4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageRun {
    /// After activation: idle agents in small batches (colleague default).
    #[default]
    After,
    /// During activation, while runtimes are offline (owner hosts' kit way).
    During,
    /// Never automatically.
    None,
}

impl PackageRun {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "after" => Some(Self::After),
            "during" => Some(Self::During),
            "none" => Some(Self::None),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::After => "after",
            Self::During => "during",
            Self::None => "none",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub schema: String,
    /// `release` | `main`; empty = the install record's channel.
    pub channel: String,
    /// Release source (CLI only); None = [`super::source::DEFAULT_SOURCE`].
    pub source: Option<String>,
    /// Background check cadence; 0 = only on request.
    pub check_interval_hours: u32,
    /// Download + verify + preflight a new release in the background.
    pub auto_stage: bool,
    /// Apply staged reload-safe / rollback-safe releases when idle (never
    /// one-way). Default off (RFC §10.3).
    pub auto_apply: bool,
    pub package_run: PackageRun,
    /// Agents per automatic package-run batch.
    pub package_run_batch: usize,
    pub max_wait_secs: u64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema: SETTINGS_SCHEMA.into(),
            channel: String::new(),
            source: None,
            check_interval_hours: DEFAULT_CHECK_INTERVAL_HOURS,
            auto_stage: true,
            auto_apply: false,
            package_run: PackageRun::After,
            package_run_batch: 5,
            max_wait_secs: DEFAULT_MAX_WAIT_SECS,
            extra: Map::new(),
        }
    }
}

impl Settings {
    pub fn load(layout: &Layout) -> Self {
        std::fs::read(layout.settings_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, layout: &Layout) -> anyhow::Result<()> {
        crate::config::atomic::write_private_pretty_json_atomic(&layout.settings_path(), self, "upgrade settings")
    }

    /// The effective channel: settings, else the install record, else this build's.
    pub fn channel(&self, layout: &Layout) -> &'static str {
        crate::build_info::normalize_channel(&self.channel)
            .or_else(|| layout.read_install().ok().and_then(|r| crate::build_info::normalize_channel(&r.channel)))
            .unwrap_or_else(crate::build_info::channel)
    }

    pub fn source(&self) -> anyhow::Result<super::source::Source> {
        super::source::Source::parse(self.source.as_deref().unwrap_or(super::source::DEFAULT_SOURCE))
    }

    /// The API view (no source URL edits through the API, but it is shown).
    pub fn view(&self, layout: &Layout) -> Value {
        self.view_with_channel(self.channel(layout))
    }

    /// The view for a daemon without a managed install (no layout to read).
    pub fn unmanaged_view(&self) -> Value {
        self.view_with_channel(crate::build_info::normalize_channel(&self.channel).unwrap_or_else(crate::build_info::channel))
    }

    fn view_with_channel(&self, channel: &str) -> Value {
        serde_json::json!({
            "channel": channel,
            "source": self.source.clone().unwrap_or_else(|| super::source::DEFAULT_SOURCE.into()),
            "checkIntervalHours": self.check_interval_hours,
            "autoStage": self.auto_stage,
            "autoApply": self.auto_apply,
            "packageRun": self.package_run.as_str(),
            "packageRunBatch": self.package_run_batch,
            "maxWaitSecs": self.max_wait_secs,
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Installed {
    pub release: String,
    #[serde(default)]
    pub sequence: u64,
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub manifest_sha256: Option<String>,
    /// Capability upgrade-installed-notes: the installed release manifest's
    /// `notes` (language tag -> text) and `publishedAt`; None when unknown
    /// (no manifest under the release dir, or a record from before these
    /// fields; [`super::stage::with_manifest_details`] fills them in).
    #[serde(default)]
    pub notes: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default)]
    pub published_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Available {
    pub release: String,
    pub sequence: u64,
    pub tag: String,
    pub manifest_sha256: String,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub notes: std::collections::BTreeMap<String, String>,
    pub class: MigrationClass,
    pub mode: Mode,
    #[serde(default)]
    pub migrations: Vec<Migration>,
    #[serde(default)]
    pub below_floor: Option<String>,
    #[serde(default)]
    pub native_version: Option<String>,
    #[serde(default)]
    pub claude_runtime: Option<String>,
    #[serde(default)]
    pub capabilities_removed: Vec<String>,
    #[serde(default)]
    pub desktop_min_commit: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    pub name: String,
    pub ok: bool,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Staged {
    pub release: String,
    pub sequence: u64,
    pub staged_at: String,
    pub manifest_sha256: String,
    pub preflight_ok: bool,
    #[serde(default)]
    pub preflight: Vec<Check>,
    /// Native version unpacked under `natives/` for this release, if any.
    #[serde(default)]
    pub native_version: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Queued,
    WaitingIdle,
    Quiescing,
    BackingUp,
    Stopping,
    Swapping,
    Starting,
    WaitingReady,
    PackageRun,
    RestoringOnline,
    Verifying,
    RollingBack,
    /// Terminal: verify passed (the commit point).
    Committed,
    /// Terminal: a failure before the commit point was rolled back.
    RolledBack,
    /// Terminal: the runner stopped; the owner must act (`needsOwner`).
    NeedsOwner,
    /// Terminal: not idle within `maxWaitSecs`; nothing changed.
    Deferred,
    /// Terminal: cancelled while waiting; nothing changed.
    Cancelled,
    /// Terminal: refused or failed before anything changed.
    Failed,
}

impl Phase {
    pub fn terminal(self) -> bool {
        matches!(
            self,
            Self::Committed | Self::RolledBack | Self::NeedsOwner | Self::Deferred | Self::Cancelled | Self::Failed
        )
    }
    /// The services may be down (clients treat disconnects as expected).
    pub fn services_down(self) -> bool {
        matches!(self, Self::Stopping | Self::Swapping | Self::Starting | Self::WaitingReady | Self::RollingBack)
    }
    /// Cancel is still possible (nothing changed yet).
    pub fn cancellable(self) -> bool {
        matches!(self, Self::Queued | Self::WaitingIdle | Self::Quiescing)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::WaitingIdle => "waiting-idle",
            Self::Quiescing => "quiescing",
            Self::BackingUp => "backing-up",
            Self::Stopping => "stopping",
            Self::Swapping => "swapping",
            Self::Starting => "starting",
            Self::WaitingReady => "waiting-ready",
            Self::PackageRun => "package-run",
            Self::RestoringOnline => "restoring-online",
            Self::Verifying => "verifying",
            Self::RollingBack => "rolling-back",
            Self::Committed => "committed",
            Self::RolledBack => "rolled-back",
            Self::NeedsOwner => "needs-owner",
            Self::Deferred => "deferred",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunKind {
    Apply,
    Rollback,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimelineEntry {
    pub phase: Phase,
    pub at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Busy {
    /// `activeTurn` | `threadActive` | `askSubmitting` | `workflowRunning` |
    /// `packageRun` | `unreachable`.
    pub reason: String,
    #[serde(default)]
    pub cutex_session_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunError {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NeedsOwner {
    pub reason: String,
    /// Commands the owner can run (shown by the desktop).
    #[serde(default)]
    pub commands: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub run_id: String,
    pub kind: RunKind,
    pub from_release: String,
    pub to_release: String,
    pub mode: Mode,
    pub class: MigrationClass,
    pub phase: Phase,
    pub created_at: String,
    #[serde(default)]
    pub finished_at: Option<String>,
    pub max_wait_secs: u64,
    pub package_run: PackageRun,
    #[serde(default)]
    pub confirm_one_way: Option<String>,
    /// Who asked: `management-api`, `cli` or `auto-apply`.
    #[serde(default)]
    pub requested_by: Option<String>,
    #[serde(default)]
    pub waiting_for: Vec<Busy>,
    #[serde(default)]
    pub cancel_requested: bool,
    #[serde(default)]
    pub timeline: Vec<TimelineEntry>,
    /// Services stopped .. ready again.
    #[serde(default)]
    pub downtime_ms: Option<u64>,
    #[serde(default)]
    pub backup_dir: Option<String>,
    #[serde(default)]
    pub package_run_id: Option<String>,
    #[serde(default)]
    pub error: Option<RunError>,
    #[serde(default)]
    pub needs_owner: Option<NeedsOwner>,
    #[serde(default)]
    pub runner_pid: Option<u32>,
    /// Displaced (moved aside, never deleted) store paths of a rollback.
    #[serde(default)]
    pub displaced: Vec<String>,
    /// The cute-codex native version the target release carries (apply: the
    /// staged manifest's `native.version`; rollback: the target release's
    /// installed manifest), recorded when the run is prepared. None: the
    /// release carries no native, or a record from before this field.
    #[serde(default)]
    pub native_version: Option<String>,
    /// The native selected for new agents on this host when the run was
    /// prepared (apply), or the running release's native (rollback).
    #[serde(default)]
    pub previous_native_version: Option<String>,
    /// What the run does to the runtime for new agents. None: a record from
    /// before this field.
    #[serde(default)]
    pub runtime_change: Option<RuntimeChange>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// [`Run::runtime_change`]: what a run does to the runtime new agents get.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeChange {
    /// `full` mode and a different native version: a new native runtime.
    Native,
    /// `full` mode, same native: only the runtime package for new agents
    /// (bundle / facade / Claude runtime selection) changes.
    Package,
    /// `reload` mode: the runtime selection is not touched.
    None,
}

impl RuntimeChange {
    pub fn of(mode: Mode, native_version: Option<&str>, previous_native_version: Option<&str>) -> Self {
        match mode {
            Mode::Reload => Self::None,
            Mode::Full if native_version.is_some() && native_version != previous_native_version => Self::Native,
            Mode::Full => Self::Package,
        }
    }
}

impl Run {
    pub fn new(kind: RunKind, from: &str, to: &str, mode: Mode, class: MigrationClass, max_wait_secs: u64, package_run: PackageRun) -> Self {
        let created_at = now();
        Self {
            run_id: format!("upg-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]),
            kind,
            from_release: from.into(),
            to_release: to.into(),
            mode,
            class,
            phase: Phase::Queued,
            created_at: created_at.clone(),
            finished_at: None,
            max_wait_secs,
            package_run,
            confirm_one_way: None,
            requested_by: None,
            waiting_for: Vec::new(),
            cancel_requested: false,
            timeline: vec![TimelineEntry { phase: Phase::Queued, at: created_at, detail: None }],
            downtime_ms: None,
            backup_dir: None,
            package_run_id: None,
            error: None,
            needs_owner: None,
            runner_pid: None,
            displaced: Vec::new(),
            native_version: None,
            previous_native_version: None,
            runtime_change: None,
            extra: Map::new(),
        }
    }

    /// Record the native / runtime detail (at prepare time, from `mode`).
    pub fn set_runtime(&mut self, native_version: Option<String>, previous_native_version: Option<String>) {
        self.runtime_change = Some(RuntimeChange::of(self.mode, native_version.as_deref(), previous_native_version.as_deref()));
        self.native_version = native_version;
        self.previous_native_version = previous_native_version;
    }

    pub fn enter(&mut self, phase: Phase, detail: Option<String>) {
        self.phase = phase;
        self.timeline.push(TimelineEntry { phase, at: now(), detail });
        if phase.terminal() {
            self.finished_at = Some(now());
            self.waiting_for.clear();
        }
    }

    pub fn fail(&mut self, phase: Phase, code: &str, message: impl Into<String>) {
        let message = message.into();
        self.error = Some(RunError { code: code.into(), message: message.clone() });
        self.enter(phase, Some(message));
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FollowUp {
    /// The upgrade run that left pinned agents on the old package.
    pub upgrade_run_id: String,
    pub started_at: String,
    #[serde(default)]
    pub last_batch_at: Option<String>,
    #[serde(default)]
    pub package_run_ids: Vec<String>,
    #[serde(default)]
    pub done: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct State {
    pub schema: String,
    pub installed: Option<Installed>,
    /// Release a rollback returns to (the previous committed one).
    pub previous: Option<Installed>,
    pub last_check_at: Option<String>,
    pub last_check_error: Option<RunError>,
    pub available: Option<Available>,
    pub staged: Option<Staged>,
    pub run: Option<Run>,
    pub follow_up: Option<FollowUp>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            schema: STATE_SCHEMA.into(),
            installed: None,
            previous: None,
            last_check_at: None,
            last_check_error: None,
            available: None,
            staged: None,
            run: None,
            follow_up: None,
            extra: Map::new(),
        }
    }
}

/// Exclusive lock for a read-modify-write of `state.json` (short).
pub struct StateLock(std::fs::File);

impl StateLock {
    pub fn acquire(layout: &Layout) -> anyhow::Result<Self> {
        let dir = layout.upgrade_dir();
        std::fs::create_dir_all(&dir)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join("state.lock"))?;
        fs2::FileExt::lock_exclusive(&file).context("lock upgrade state")?;
        Ok(Self(file))
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

/// The long lock held by whoever runs check / stage / apply / rollback.
pub struct OperationLock(std::fs::File);

impl OperationLock {
    /// `None` when another operation holds it.
    pub fn try_acquire(layout: &Layout) -> anyhow::Result<Option<Self>> {
        std::fs::create_dir_all(layout.upgrade_dir())?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(layout.lock_path())?;
        Ok(fs2::FileExt::try_lock_exclusive(&file).ok().map(|_| Self(file)))
    }

    pub fn held(layout: &Layout) -> bool {
        matches!(Self::try_acquire(layout), Ok(None))
    }
}

impl Drop for OperationLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

impl State {
    pub fn load(layout: &Layout) -> Self {
        Self::read(&layout.state_path()).unwrap_or_default()
    }

    fn read(path: &Path) -> Option<Self> {
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    }

    pub fn save(&self, layout: &Layout) -> anyhow::Result<()> {
        crate::config::atomic::write_private_pretty_json_atomic(&layout.state_path(), self, "upgrade state")
    }

    /// Read-modify-write under [`StateLock`].
    pub fn update<T>(layout: &Layout, change: impl FnOnce(&mut State) -> T) -> anyhow::Result<T> {
        let _lock = StateLock::acquire(layout)?;
        let mut state = Self::load(layout);
        let out = change(&mut state);
        state.save(layout)?;
        Ok(out)
    }
}

/// Persist a run: `state.json` `run`, (once terminal) a `history.jsonl`
/// line, and `runs/<id>/run.json` last: `cutex upgrade apply` follows
/// `run.json`, so when it sees a phase the API (state + history) already
/// shows it too.
pub fn record_run(layout: &Layout, run: &Run) -> anyhow::Result<()> {
    let dir = layout.run_dir(&run.run_id);
    std::fs::create_dir_all(&dir)?;
    State::update(layout, |state| state.run = Some(run.clone()))?;
    if run.phase.terminal() {
        let marker = dir.join(".in-history");
        if !marker.exists() {
            let mut file = std::fs::OpenOptions::new().create(true).append(true).open(layout.history_path())?;
            let mut line = serde_json::to_vec(run)?;
            line.push(b'\n');
            file.write_all(&line)?;
            std::fs::write(marker, b"")?;
        }
    }
    crate::config::atomic::write_private_pretty_json_atomic(&dir.join("run.json"), run, "upgrade run")?;
    Ok(())
}

/// Apply failures where nothing changed and a later try may succeed
/// (the management API did not answer, the runner never started or died
/// before changing anything).
pub const TRANSIENT_APPLY_FAILURES: &[&str] = &["management_unreachable", "runner_lost", "runner_start_failed"];
/// Transient failures of one release after which the scheduler stops
/// auto-applying it. The scheduler tries at most once per due check
/// (`checkIntervalHours`), which spaces the retries.
pub const MAX_TRANSIENT_AUTO_APPLY_FAILURES: usize = 3;

/// Why the scheduler must not auto-stage / auto-apply `release` (the owner
/// still can), newest history first:
///
/// - `rolled-back-by-owner`: a committed owner rollback left it
///   (`fromRelease == release`), until the owner applies it by hand again;
/// - `apply-failed`: an apply to it rolled back, was left to the owner, or
///   failed for a non-transient reason;
/// - `apply-failed-repeatedly`: [`MAX_TRANSIENT_AUTO_APPLY_FAILURES`]
///   transient failures ([`TRANSIENT_APPLY_FAILURES`]).
///
/// A manual apply of `release` (requested by anyone but `auto-apply`) that
/// did not end badly ends the scan: what happened before no longer counts.
pub fn auto_upgrade_blocked(layout: &Layout, release: &str) -> Option<&'static str> {
    let mut transient = 0;
    for run in history(layout, usize::MAX) {
        match run.kind {
            RunKind::Rollback if run.phase == Phase::Committed && run.from_release == release => {
                return Some("rolled-back-by-owner");
            }
            RunKind::Apply if run.to_release == release => match run.phase {
                Phase::RolledBack | Phase::NeedsOwner => return Some("apply-failed"),
                Phase::Failed => {
                    let code = run.error.as_ref().map(|e| e.code.as_str()).unwrap_or_default();
                    if !TRANSIENT_APPLY_FAILURES.contains(&code) {
                        return Some("apply-failed");
                    }
                    transient += 1;
                    if transient >= MAX_TRANSIENT_AUTO_APPLY_FAILURES {
                        return Some("apply-failed-repeatedly");
                    }
                }
                _ if run.requested_by.as_deref() != Some("auto-apply") => return None,
                _ => {}
            },
            _ => {}
        }
    }
    None
}

pub fn read_run(layout: &Layout, run_id: &str) -> Option<Run> {
    if !super::layout::valid_label(run_id) {
        return None;
    }
    serde_json::from_slice(&std::fs::read(layout.run_dir(run_id).join("run.json")).ok()?).ok()
}

/// Finished runs, newest first.
pub fn history(layout: &Layout, limit: usize) -> Vec<Run> {
    let text = std::fs::read_to_string(layout.history_path()).unwrap_or_default();
    let mut runs: Vec<Run> = text.lines().filter_map(|line| serde_json::from_str(line).ok()).collect();
    runs.reverse();
    runs.truncate(limit);
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> Layout {
        let dir = std::env::temp_dir().join(format!("cutex-upgrade-state-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        Layout::at(dir)
    }

    #[test]
    fn settings_defaults_and_unknown_fields() {
        let layout = layout();
        let settings = Settings::load(&layout);
        assert_eq!(settings, Settings::default());
        assert!(settings.auto_stage && !settings.auto_apply);
        assert_eq!(settings.package_run, PackageRun::After);
        std::fs::create_dir_all(layout.upgrade_dir()).unwrap();
        std::fs::write(layout.settings_path(), br#"{"autoApply": true, "future": 1, "channel": "stable"}"#).unwrap();
        let settings = Settings::load(&layout);
        assert!(settings.auto_apply);
        assert_eq!(settings.channel(&layout), "release");
        assert_eq!(settings.check_interval_hours, DEFAULT_CHECK_INTERVAL_HOURS);
        settings.save(&layout).unwrap();
        assert!(std::fs::read_to_string(layout.settings_path()).unwrap().contains("future"));
        std::fs::remove_dir_all(&layout.base).unwrap();
    }

    #[test]
    fn runs_are_recorded_once_in_history() {
        let layout = layout();
        let mut run = Run::new(RunKind::Apply, "r239", "r240", Mode::Reload, MigrationClass::ReloadSafe, 60, PackageRun::After);
        record_run(&layout, &run).unwrap();
        assert!(history(&layout, 10).is_empty());
        assert_eq!(State::load(&layout).run.unwrap().phase, Phase::Queued);
        run.enter(Phase::WaitingIdle, None);
        run.enter(Phase::Committed, None);
        record_run(&layout, &run).unwrap();
        record_run(&layout, &run).unwrap();
        assert_eq!(history(&layout, 10).len(), 1);
        assert_eq!(read_run(&layout, &run.run_id).unwrap().phase, Phase::Committed);
        assert!(read_run(&layout, "../x").is_none());
        assert!(run.finished_at.is_some());
        let text = serde_json::to_string(&run).unwrap();
        assert!(text.contains("\"phase\":\"committed\"") && text.contains("\"mode\":\"reload\""), "{text}");
        std::fs::remove_dir_all(&layout.base).unwrap();
    }

    #[test]
    fn runtime_change_detail_and_old_records() {
        let full = |native: Option<&str>, previous: Option<&str>| RuntimeChange::of(Mode::Full, native, previous);
        assert_eq!(full(Some("0.162.1-r53"), Some("0.160.0-r51")), RuntimeChange::Native);
        assert_eq!(full(Some("0.162.1-r53"), None), RuntimeChange::Native);
        assert_eq!(full(Some("0.162.1-r53"), Some("0.162.1-r53")), RuntimeChange::Package);
        assert_eq!(full(None, Some("0.160.0-r51")), RuntimeChange::Package);
        assert_eq!(RuntimeChange::of(Mode::Reload, Some("0.162.1-r53"), None), RuntimeChange::None);
        let mut run = Run::new(RunKind::Apply, "r239", "r240", Mode::Full, MigrationClass::ReloadSafe, 60, PackageRun::After);
        run.set_runtime(Some("0.162.1-r53".into()), Some("0.160.0-r51".into()));
        let value = serde_json::to_value(&run).unwrap();
        assert_eq!((&value["nativeVersion"], &value["previousNativeVersion"], &value["runtimeChange"]),
            (&serde_json::json!("0.162.1-r53"), &serde_json::json!("0.160.0-r51"), &serde_json::json!("native")));
        // A record written before these fields reads them as null (and writes them as null).
        let mut old = value.as_object().unwrap().clone();
        for key in ["nativeVersion", "previousNativeVersion", "runtimeChange"] {
            old.remove(key);
        }
        let old: Run = serde_json::from_value(Value::Object(old)).unwrap();
        assert_eq!((&old.native_version, &old.previous_native_version, old.runtime_change), (&None, &None, None));
        assert!(old.extra.is_empty());
        let value = serde_json::to_value(&old).unwrap();
        assert!(value["nativeVersion"].is_null() && value["runtimeChange"].is_null() && value.get("runtimeChange").is_some());
    }

    fn finished(kind: RunKind, from: &str, to: &str, by: &str, phase: Phase, code: Option<&str>) -> Run {
        let mut run = Run::new(kind, from, to, Mode::Reload, MigrationClass::ReloadSafe, 60, PackageRun::After);
        run.requested_by = Some(by.into());
        match code {
            Some(code) => run.fail(phase, code, "x"),
            None => run.enter(phase, None),
        }
        run
    }

    #[test]
    fn a_failed_apply_is_remembered_per_release() {
        let layout = layout();
        assert_eq!(auto_upgrade_blocked(&layout, "r240"), None);
        record_run(&layout, &finished(RunKind::Apply, "r239", "r240", "auto-apply", Phase::Committed, None)).unwrap();
        assert_eq!(auto_upgrade_blocked(&layout, "r240"), None);
        record_run(&layout, &finished(RunKind::Apply, "r240", "r241", "auto-apply", Phase::RolledBack, Some("verify_failed"))).unwrap();
        assert_eq!(auto_upgrade_blocked(&layout, "r241"), Some("apply-failed"));
        assert_eq!(auto_upgrade_blocked(&layout, "r242"), None);
        record_run(&layout, &finished(RunKind::Apply, "r240", "r242", "auto-apply", Phase::Failed, Some("plan_failed"))).unwrap();
        assert_eq!(auto_upgrade_blocked(&layout, "r242"), Some("apply-failed"));
        std::fs::remove_dir_all(&layout.base).unwrap();
    }

    #[test]
    fn transient_failures_are_retried_up_to_a_cap() {
        let layout = layout();
        for (n, code) in TRANSIENT_APPLY_FAILURES.iter().enumerate() {
            let expected = if n + 1 < MAX_TRANSIENT_AUTO_APPLY_FAILURES { None } else { Some("apply-failed-repeatedly") };
            record_run(&layout, &finished(RunKind::Apply, "r239", "r240", "auto-apply", Phase::Failed, Some(code))).unwrap();
            assert_eq!(auto_upgrade_blocked(&layout, "r240"), expected, "after {code}");
        }
        // Deferred / cancelled runs neither block nor count.
        let other = layout.base.join("other");
        let other = Layout::at(other);
        record_run(&other, &finished(RunKind::Apply, "r239", "r240", "auto-apply", Phase::Deferred, None)).unwrap();
        record_run(&other, &finished(RunKind::Apply, "r239", "r240", "auto-apply", Phase::Failed, Some("runner_lost"))).unwrap();
        assert_eq!(auto_upgrade_blocked(&other, "r240"), None);
        std::fs::remove_dir_all(&layout.base).unwrap();
    }

    #[test]
    fn an_owner_rollback_blocks_auto_apply_until_a_manual_apply() {
        let layout = layout();
        record_run(&layout, &finished(RunKind::Apply, "r239", "r240", "auto-apply", Phase::Committed, None)).unwrap();
        record_run(&layout, &finished(RunKind::Rollback, "r240", "r239", "management-api", Phase::Committed, None)).unwrap();
        assert_eq!(auto_upgrade_blocked(&layout, "r240"), Some("rolled-back-by-owner"));
        assert_eq!(auto_upgrade_blocked(&layout, "r239"), None);
        // An auto run cannot lift it; an owner apply does.
        record_run(&layout, &finished(RunKind::Apply, "r239", "r240", "auto-apply", Phase::Deferred, None)).unwrap();
        assert_eq!(auto_upgrade_blocked(&layout, "r240"), Some("rolled-back-by-owner"));
        record_run(&layout, &finished(RunKind::Apply, "r239", "r240", "cli", Phase::Committed, None)).unwrap();
        assert_eq!(auto_upgrade_blocked(&layout, "r240"), None);
        std::fs::remove_dir_all(&layout.base).unwrap();
    }

    #[test]
    fn operation_lock_is_exclusive() {
        let layout = layout();
        let first = OperationLock::try_acquire(&layout).unwrap().unwrap();
        assert!(OperationLock::try_acquire(&layout).unwrap().is_none());
        drop(first);
        assert!(OperationLock::try_acquire(&layout).unwrap().is_some());
        std::fs::remove_dir_all(&layout.base).unwrap();
    }

    #[test]
    fn phases() {
        assert!(Phase::Committed.terminal() && !Phase::Verifying.terminal());
        assert!(Phase::WaitingIdle.cancellable() && !Phase::Stopping.cancellable());
        assert!(Phase::WaitingReady.services_down() && !Phase::Verifying.services_down());
        assert_eq!(serde_json::to_value(Phase::NeedsOwner).unwrap(), "needs-owner");
        assert_eq!(Phase::NeedsOwner.as_str(), "needs-owner");
    }
}
