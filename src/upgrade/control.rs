//! Owner operations shared by the CLI and `/v2/upgrade*`: prepare an apply
//! or rollback run (every refusal happens here, before anything changes),
//! start its detached runner, cancel, and the runner's entry point
//! (`cutex upgrade run <runId>`, executed by the TARGET release's binary
//! for an apply and by the running release's for a rollback).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use serde_json::{json, Value};

use super::layout::Layout;
use super::manifest::{self, Migration, MigrationClass, Mode, ReleaseManifest};
use super::minisign::PublicKey;
use super::runner::{self, Plan};
use super::stage::{self, blocked, upgrade_error, LocalRuntimes, MANIFEST_FILE};
use super::state::{self, OperationLock, PackageRun, Phase, Run, RunKind, Settings, State, MAX_MAX_WAIT_SECS};

const READY_TIMEOUT: Duration = Duration::from_secs(180);
/// A queued run whose runner never took the lock within this is lost.
const RUNNER_START_GRACE_SECS: i64 = 120;
const SELECTION_BACKUP_KEY: &str = "selectionBackup";

#[derive(Clone, Debug, Default)]
pub struct ApplyRequest {
    /// Defaults to the staged release.
    pub release: Option<String>,
    /// Only raises the computed mode (`reload` → `full`).
    pub mode: Option<Mode>,
    pub max_wait_secs: Option<u64>,
    /// Must equal the release for a one-way upgrade.
    pub confirm_one_way: Option<String>,
    pub package_run: Option<PackageRun>,
    pub requested_by: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct RollbackRequest {
    /// Defaults to the install record's previous release.
    pub to: Option<String>,
    pub max_wait_secs: Option<u64>,
    pub dry_run: bool,
    pub requested_by: Option<String>,
}

fn managed(layout: &Layout) -> anyhow::Result<()> {
    if !layout.is_managed() {
        return Err(blocked(
            "unmanaged_install",
            "this daemon does not run from a managed install (no install.json); upgrade it by hand",
        ));
    }
    Ok(())
}

fn age_secs(timestamp: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .map(|at| (chrono::Utc::now() - at.with_timezone(&chrono::Utc)).num_seconds())
        .unwrap_or(i64::MAX)
}

/// A non-terminal run whose runner is gone (no operation lock): record it
/// as `failed` (nothing changed yet) or `needs-owner`. Returns the run.
pub fn reconcile_lost_run(layout: &Layout) -> Option<Run> {
    let mut run = State::load(layout).run?;
    if run.phase.terminal() || OperationLock::held(layout) {
        return Some(run);
    }
    if run.phase == Phase::Queued && age_secs(&run.created_at) < RUNNER_START_GRACE_SECS {
        return Some(run);
    }
    if run.phase.cancellable() && run.phase != Phase::Quiescing {
        run.fail(Phase::Failed, "runner_lost", "the upgrade runner exited before changing anything");
    } else {
        run.error.get_or_insert(state::RunError { code: "runner_lost".into(), message: "the upgrade runner exited unexpectedly".into() });
        run.needs_owner = Some(state::NeedsOwner {
            reason: format!("the upgrade runner exited during {}", run.phase.as_str()),
            commands: vec!["cutex service status".into(), "cutex upgrade status".into(), "cutex upgrade rollback".into()],
        });
        run.enter(Phase::NeedsOwner, Some("runner lost".into()));
    }
    let _ = state::record_run(layout, &run);
    Some(run)
}

fn ensure_no_run(layout: &Layout) -> anyhow::Result<()> {
    if let Some(run) = reconcile_lost_run(layout) {
        if !run.phase.terminal() {
            return Err(stage::UpgradeError::boxed(409, "upgrade_blocked", format!("run {} is {}", run.run_id, run.phase.as_str()), json!({
                "reason": "run_in_progress",
                "runId": run.run_id,
            })));
        }
    }
    if OperationLock::held(layout) {
        return Err(blocked("run_in_progress", "another upgrade operation is running"));
    }
    Ok(())
}

fn max_wait(requested: Option<u64>, settings: &Settings) -> anyhow::Result<u64> {
    let value = requested.unwrap_or(settings.max_wait_secs);
    if value > MAX_MAX_WAIT_SECS {
        return Err(upgrade_error(400, "invalid_request", format!("maxWaitSecs must be at most {MAX_MAX_WAIT_SECS}")));
    }
    Ok(value)
}

/// Validate an apply and record it as `queued`. Nothing else changes.
pub fn prepare_apply(layout: &Layout, keys: &[PublicKey], request: &ApplyRequest) -> anyhow::Result<Run> {
    managed(layout)?;
    ensure_no_run(layout)?;
    let settings = Settings::load(layout);
    let state = State::load(layout);
    let Some(staged) = state.staged.clone() else {
        return Err(upgrade_error(409, "not_staged", "no release is staged; run check and stage first"));
    };
    if let Some(release) = &request.release {
        if *release != staged.release {
            return Err(upgrade_error(409, "not_staged", format!("{release} is not staged (staged: {})", staged.release)));
        }
    }
    if !staged.preflight_ok {
        let failed: Vec<&str> = staged.preflight.iter().filter(|c| !c.ok).map(|c| c.name.as_str()).collect();
        return Err(blocked("preflight_failed", format!("preflight failed: {}", failed.join(", "))));
    }
    let target = stage::staged_manifest(layout, keys, &staged.release)?;
    let installed = stage::installed(layout);
    if target.sequence <= installed.sequence {
        return Err(upgrade_error(409, "downgrade_refused", format!("{} is not newer than {}", staged.release, installed.release)));
    }
    let local = LocalRuntimes::read();
    let classification = manifest::classify(&target, Some(&installed.release), stage::runtime_changes(&target, &local));
    if let Some(floor) = &classification.below_floor {
        return Err(blocked("below_floor", format!("{} cannot upgrade straight to {}; install {floor} first", installed.release, staged.release)));
    }
    if classification.class == MigrationClass::OneWay && request.confirm_one_way.as_deref() != Some(staged.release.as_str()) {
        return Err(stage::UpgradeError::boxed(
            409,
            "one_way_unconfirmed",
            format!("{} is one-way: no automatic rollback; confirm with confirmOneWay = \"{}\"", staged.release, staged.release),
            json!({ "migrations": classification.migrations }),
        ));
    }
    let mode = request.mode.map_or(classification.mode, |m| m.max(classification.mode));
    let mut run = Run::new(
        RunKind::Apply,
        &installed.release,
        &staged.release,
        mode,
        classification.class,
        max_wait(request.max_wait_secs, &settings)?,
        request.package_run.unwrap_or(settings.package_run),
    );
    run.confirm_one_way = request.confirm_one_way.clone();
    run.requested_by = request.requested_by.clone();
    state::record_run(layout, &run)?;
    Ok(run)
}

/// The sequence in `releases/<release>/manifest.json`; 0 without one (a
/// hand-installed build), like [`stage::installed`].
fn release_sequence(layout: &Layout, release: &str) -> u64 {
    std::fs::read(layout.release_dir(release).join(MANIFEST_FILE))
        .ok()
        .and_then(|bytes| ReleaseManifest::parse(&bytes).ok())
        .map_or(0, |m| m.sequence)
}

/// After a committed owner rollback `from` → `to`: the swap recorded `from`
/// as `previousRelease`, so a second default rollback would go FORWARD to
/// the release just rolled away from. Point it at `to`'s own predecessor
/// (the committed apply that installed `to`, if that release is still
/// installed and not newer), else clear it.
fn settle_rollback_target(layout: &Layout, run: &Run) -> anyhow::Result<()> {
    if run.kind != RunKind::Rollback || run.phase != Phase::Committed {
        return Ok(());
    }
    let mut record = layout.read_install()?;
    let to_sequence = release_sequence(layout, &run.to_release);
    let predecessor = state::history(layout, 200)
        .into_iter()
        .find(|r| r.kind == RunKind::Apply && r.phase == Phase::Committed && r.to_release == run.to_release)
        .map(|r| r.from_release)
        .filter(|p| *p != run.from_release && *p != run.to_release)
        .filter(|p| super::layout::valid_label(p) && layout.release_executable(p).is_file())
        .filter(|p| release_sequence(layout, p) <= to_sequence);
    if record.previous_release != predecessor {
        record.previous_release = predecessor;
        layout.write_install(&record)?;
    }
    Ok(())
}

/// The migrations a rollback from `from` to `to` undoes, from `from`'s manifest.
fn rollback_migrations(layout: &Layout, from: &str, to: &str) -> Vec<Migration> {
    std::fs::read(layout.release_dir(from).join(MANIFEST_FILE))
        .ok()
        .and_then(|bytes| ReleaseManifest::parse(&bytes).ok())
        .map(|m| manifest::classify(&m, Some(to), false).migrations)
        .unwrap_or_default()
}

/// The backup taken by the committed apply `to` → `from` (its selection files).
fn selection_backup(layout: &Layout, from: &str, to: &str) -> Option<PathBuf> {
    state::history(layout, 200)
        .into_iter()
        .find(|run| run.kind == RunKind::Apply && run.phase == Phase::Committed && run.to_release == from && run.from_release == to)
        .and_then(|run| run.backup_dir.map(PathBuf::from))
        .filter(|dir| dir.join("home-manifest.json").is_file())
}

/// Validate a rollback; `dry_run` returns the preview only.
pub fn prepare_rollback(layout: &Layout, request: &RollbackRequest) -> anyhow::Result<(Option<Run>, Value)> {
    managed(layout)?;
    if !request.dry_run {
        ensure_no_run(layout)?;
    }
    let record = layout.read_install()?;
    let from = layout.active_release().unwrap_or(record.active_release.clone());
    let Some(to) = request.to.clone().or(record.previous_release.clone()) else {
        return Err(upgrade_error(409, "no_rollback_target", "there is no previous release to roll back to"));
    };
    super::layout::ensure_label(&to, "release").map_err(|e| upgrade_error(400, "invalid_request", format!("{e:#}")))?;
    if to == from {
        return Err(upgrade_error(409, "no_rollback_target", format!("{to} is already active")));
    }
    if !layout.release_executable(&to).is_file() {
        return Err(upgrade_error(409, "no_rollback_target", format!("release {to} is not installed")));
    }
    // A rollback only goes back: a newer release is applied (staged,
    // verified, with its migrations), never "rolled back" to.
    let installed = stage::installed(layout);
    let target_sequence = release_sequence(layout, &to);
    if target_sequence > installed.sequence {
        return Err(stage::UpgradeError::boxed(
            409,
            "no_rollback_target",
            format!("{to} (sequence {target_sequence}) is newer than the installed {from} (sequence {}); apply it instead", installed.sequence),
            json!({ "reason": "newer_than_installed" }),
        ));
    }
    let migrations = rollback_migrations(layout, &from, &to);
    let class = migrations.iter().map(|m| m.class).max().unwrap_or(MigrationClass::ReloadSafe);
    let selection = selection_backup(layout, &from, &to);
    let new_stores: Vec<&String> = migrations.iter().flat_map(|m| m.new_stores.iter()).collect();
    let preview = json!({
        "from": from,
        "to": to,
        "class": class,
        "migrations": migrations,
        "movedAside": new_stores,
        "selectionBackup": selection.as_ref().map(|p| p.display().to_string()),
        "sessionDataRestored": false,
    });
    if class == MigrationClass::OneWay {
        return Err(stage::UpgradeError::boxed(
            409,
            "upgrade_blocked",
            format!("{from} → {to} crosses a one-way migration; restore from a backup by hand"),
            json!({ "reason": "one_way", "preview": preview }),
        ));
    }
    if request.dry_run {
        return Ok((None, preview));
    }
    let settings = Settings::load(layout);
    let mode = if selection.is_some() { Mode::Full } else { Mode::Reload };
    let mut run = Run::new(RunKind::Rollback, &from, &to, mode, class, max_wait(request.max_wait_secs, &settings)?, PackageRun::None);
    run.requested_by = request.requested_by.clone();
    if let Some(selection) = &selection {
        run.extra.insert(SELECTION_BACKUP_KEY.into(), json!(selection.display().to_string()));
    }
    state::record_run(layout, &run)?;
    Ok((Some(run), preview))
}

fn cancel_marker(layout: &Layout, run_id: &str) -> PathBuf {
    layout.run_dir(run_id).join("cancel-requested")
}

/// Ask the runner to stop while it waits (`cancellable` phases only).
pub fn cancel(layout: &Layout) -> anyhow::Result<Run> {
    let Some(mut run) = reconcile_lost_run(layout) else {
        return Err(upgrade_error(409, "no_run", "no upgrade run"));
    };
    if run.phase.terminal() {
        return Err(upgrade_error(409, "no_run", format!("run {} already finished ({})", run.run_id, run.phase.as_str())));
    }
    if !run.phase.cancellable() {
        return Err(upgrade_error(409, "not_cancellable", format!("run {} is {}; it can no longer be cancelled", run.run_id, run.phase.as_str())));
    }
    std::fs::create_dir_all(layout.run_dir(&run.run_id))?;
    std::fs::write(cancel_marker(layout, &run.run_id), b"")?;
    run.cancel_requested = true;
    if !OperationLock::held(layout) {
        // Queued and the runner has not started: cancel it here.
        run.enter(Phase::Cancelled, Some("cancelled by the owner".into()));
        state::record_run(layout, &run)?;
    }
    Ok(run)
}

/// Variables a runner started by `systemd-run --user` must inherit: it runs
/// in the user manager's environment, not the caller's, so without them it
/// would resolve another home (the default instance instead of a
/// `CUTEX_HOME` instance; debug builds: the real profile instead of a
/// private test home) and act on that home's daemon.
pub const RUNNER_ENV: &[&str] = &[
    crate::config::instance::HOME_ENV,
    "HOME",
    "CUTEX_TEST_PRIVATE_HOME",
    crate::config::paths::FORBID_REAL_HOME_ENV,
    super::keys::TEST_PUBLIC_KEY_ENV,
    super::service::TEST_SERVICE_NAME_ENV,
];

/// `--setenv=K=V` for every [`RUNNER_ENV`] variable set in `vars`.
pub fn runner_setenv_args(vars: impl Fn(&str) -> Option<std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    RUNNER_ENV
        .iter()
        .filter_map(|key| {
            let value = vars(key).filter(|v| !v.is_empty())?;
            let mut arg = std::ffi::OsString::from(format!("--setenv={key}="));
            arg.push(value);
            Some(arg)
        })
        .collect()
}

/// Start `cutex upgrade run <runId>` with `exe`, outside the service tree.
pub fn spawn_runner(layout: &Layout, exe: &Path, run_id: &str) -> anyhow::Result<u32> {
    let dir = layout.run_dir(run_id);
    std::fs::create_dir_all(&dir)?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("runner.log"))?;
    let systemd = layout.read_install().map(|r| r.service_manager == super::service_manager::SYSTEMD).unwrap_or(false);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let mut command = if systemd && which("systemd-run") {
            // Its own transient unit: stopping the management unit never reaches it.
            let mut command = std::process::Command::new("systemd-run");
            command
                .args(["--user", "--collect", "--quiet", &format!("--unit=cutex-upgrade-{run_id}")])
                .arg(format!("--setenv={}={}", super::layout::INSTALL_BASE_ENV, layout.base.display()))
                .args(runner_setenv_args(|key| std::env::var_os(key)))
                // The transient unit's output goes to the journal, not to
                // this process's stdio: send it to runner.log as documented.
                .arg(format!("--property=StandardOutput=append:{}", dir.join("runner.log").display()))
                .arg(format!("--property=StandardError=append:{}", dir.join("runner.log").display()))
                .arg(exe)
                .args(["upgrade", "run", run_id]);
            command
        } else {
            let mut command = std::process::Command::new(exe);
            command.args(["upgrade", "run", run_id]).env(super::layout::INSTALL_BASE_ENV, &layout.base);
            unsafe {
                command.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
            command
        };
        command.env_remove(super::service_manager::SERVICE_MANAGER_ENV);
        command.stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
        let child = command.spawn().with_context(|| format!("start the upgrade runner {}", exe.display()))?;
        let pid = child.id();
        std::thread::spawn(move || {
            let mut child = child;
            let _ = child.wait();
        });
        Ok(pid)
    }
    #[cfg(windows)]
    {
        let _ = systemd;
        let mut command = std::process::Command::new(exe);
        command
            .args(["upgrade", "run", run_id])
            .env(super::layout::INSTALL_BASE_ENV, &layout.base)
            .env_remove(super::service_manager::SERVICE_MANAGER_ENV);
        let child = crate::platform::windows_spawn::spawn_detached(&command, &log)?;
        Ok(child.id())
    }
}

#[cfg(unix)]
fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

/// The binary that runs a prepared run: the target release (apply) or the
/// running one (rollback).
pub fn runner_executable(layout: &Layout, run: &Run) -> PathBuf {
    match run.kind {
        RunKind::Apply => layout.release_executable(&run.to_release),
        RunKind::Rollback => layout.release_executable(&run.from_release),
    }
}

/// The releases whose executables a run starts: the runner's (apply: the
/// target; rollback: the running one) and, for a rollback, the target the
/// services are switched to.
fn releases_started(run: &Run) -> Vec<&str> {
    match run.kind {
        RunKind::Apply => vec![run.to_release.as_str()],
        RunKind::Rollback => vec![run.from_release.as_str(), run.to_release.as_str()],
    }
}

/// Prepare + spawn, recording a spawn failure on the run. Every release
/// executable the run starts must match its release file record
/// (integrity.rs) first.
pub fn start(layout: &Layout, keys: &[PublicKey], mut run: Run) -> anyhow::Result<Run> {
    for release in releases_started(&run) {
        if let Err(error) = super::integrity::verify_executable(layout, keys, release) {
            run.fail(Phase::Failed, "release_unverified", format!("{error:#}"));
            state::record_run(layout, &run)?;
            return Err(error);
        }
    }
    let exe = runner_executable(layout, &run);
    match spawn_runner(layout, &exe, &run.run_id) {
        Ok(pid) => {
            run.runner_pid = Some(pid);
            // The runner may already have recorded progress; keep its copy.
            if let Some(mut current) = state::read_run(layout, &run.run_id) {
                if current.runner_pid.is_none() && current.phase == Phase::Queued {
                    current.runner_pid = Some(pid);
                    state::record_run(layout, &current)?;
                }
                return Ok(current);
            }
            Ok(run)
        }
        Err(error) => {
            run.fail(Phase::Failed, "runner_start_failed", format!("{error:#}"));
            state::record_run(layout, &run)?;
            Err(error)
        }
    }
}

fn plan_for(layout: &Layout, keys: &[PublicKey], run: &Run) -> anyhow::Result<Plan> {
    let run_dir = layout.run_dir(&run.run_id);
    let max_wait = Duration::from_secs(run.max_wait_secs);
    match run.kind {
        RunKind::Apply => {
            let target = stage::staged_manifest(layout, keys, &run.to_release)?;
            let classification = manifest::classify(&target, Some(&run.from_release), false);
            Ok(Plan {
                to_release: run.to_release.clone(),
                to_commit: Some(target.cutex.commit.clone()),
                mode: run.mode,
                class: run.class,
                migrations: classification.migrations,
                capabilities_added: target.cutex.capabilities.added.clone(),
                capabilities_removed: target.cutex.capabilities.removed.clone(),
                package_run: if run.mode == Mode::Full { run.package_run } else { PackageRun::None },
                max_wait,
                ready_timeout: READY_TIMEOUT,
                run_dir,
                selection_backup: None,
            })
        }
        RunKind::Rollback => Ok(Plan {
            to_release: run.to_release.clone(),
            to_commit: None,
            mode: run.mode,
            class: run.class,
            migrations: rollback_migrations(layout, &run.from_release, &run.to_release),
            capabilities_added: Vec::new(),
            capabilities_removed: Vec::new(),
            package_run: PackageRun::None,
            max_wait,
            ready_timeout: READY_TIMEOUT,
            run_dir,
            selection_backup: run.extra.get(SELECTION_BACKUP_KEY).and_then(Value::as_str).map(PathBuf::from),
        }),
    }
}

/// After a committed apply: installed / previous, clear staged / available,
/// prune backups, start the package follow-up when due.
fn after_commit(layout: &Layout, run: &Run, keys: &[PublicKey]) -> anyhow::Result<()> {
    let installed = stage::installed(layout);
    State::update(layout, |state| {
        if run.kind == RunKind::Apply {
            state.previous = state.installed.take();
        } else {
            state.previous = None;
        }
        state.installed = Some(installed.clone());
        if state.staged.as_ref().is_some_and(|s| s.sequence <= installed.sequence) {
            state.staged = None;
        }
        if state.available.as_ref().is_some_and(|a| a.sequence <= installed.sequence) {
            state.available = None;
        }
        if run.kind == RunKind::Apply && run.mode == Mode::Full && run.package_run == PackageRun::After {
            state.follow_up = Some(state::FollowUp {
                upgrade_run_id: run.run_id.clone(),
                started_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                last_batch_at: None,
                package_run_ids: Vec::new(),
                done: false,
            });
        } else if run.kind == RunKind::Rollback {
            state.follow_up = None;
        }
    })?;
    let _ = super::backup::prune(&layout.runs_dir(), state::BACKUPS_KEPT);
    let _ = keys;
    settle_rollback_target(layout, run)
}

/// After an apply that rolled itself back: the switch back to `from` made the
/// failed target the install's `previousRelease`, so a later owner
/// `rollback` (default target) would start the release that just failed
/// verification. Put back the rollback target from before the run.
pub fn restore_rollback_target(layout: &Layout, run: &Run, previous_before: Option<String>) -> anyhow::Result<()> {
    if run.kind != RunKind::Apply || run.phase != Phase::RolledBack {
        return Ok(());
    }
    let mut record = layout.read_install()?;
    let active = layout.active_release().unwrap_or_else(|_| record.active_release.clone());
    if active != run.from_release || record.previous_release == previous_before {
        return Ok(());
    }
    record.previous_release = previous_before;
    layout.write_install(&record)
}

/// `cutex upgrade run <runId>`: execute a prepared run. Returns the final run.
pub fn execute(layout: &Layout, run_id: &str) -> anyhow::Result<Run> {
    let keys = super::keys::trusted_keys();
    let Some(lock) = OperationLock::try_acquire(layout)? else {
        anyhow::bail!("another upgrade operation holds {}", layout.lock_path().display());
    };
    let mut run = state::read_run(layout, run_id).with_context(|| format!("no upgrade run {run_id}"))?;
    if run.phase.terminal() {
        return Ok(run);
    }
    run.runner_pid = Some(std::process::id());
    let plan = match plan_for(layout, &keys, &run) {
        Ok(plan) => plan,
        Err(error) => {
            run.fail(Phase::Failed, "plan_failed", format!("{error:#}"));
            state::record_run(layout, &run)?;
            return Ok(run);
        }
    };
    let host = super::host::RealHost::connect(layout, &run, cancel_marker(layout, run_id))?;
    let previous_before = layout.read_install().ok().and_then(|record| record.previous_release);
    match run.kind {
        RunKind::Apply => runner::apply(&mut run, &plan, &host),
        RunKind::Rollback => runner::rollback(&mut run, &plan, &host),
    }
    if let Err(error) = restore_rollback_target(layout, &run, previous_before) {
        eprintln!("cutex upgrade: could not restore the rollback target: {error:#}");
    }
    if run.phase == Phase::Committed {
        after_commit(layout, &run, &keys)?;
    }
    drop(lock);
    if run.phase == Phase::Committed && State::load(layout).follow_up.as_ref().is_some_and(|f| f.upgrade_run_id == run.run_id) {
        super::host::follow_up(layout, &host, &run.run_id);
    }
    Ok(run)
}

/// `cutex upgrade selftest --json` (run by preflight with the NEW binary
/// against the live home): read-only checks that this build can read the
/// stores it will open first. Never writes.
pub fn selftest() -> Value {
    let mut failures: Vec<Value> = Vec::new();
    let mut checks: Vec<&str> = Vec::new();
    let home = match crate::config::paths::config_dir() {
        Ok(home) => home,
        Err(error) => return json!({ "ok": false, "failures": [{ "check": "home", "error": format!("{error:#}") }] }),
    };
    let mut fail = |check: &str, error: String| failures.push(json!({ "check": check, "error": error }));
    for name in ["config.json", "accounts.json"] {
        checks.push(name);
        let path = home.join(name);
        if let Ok(bytes) = std::fs::read(&path) {
            if let Err(error) = serde_json::from_slice::<Value>(&bytes) {
                fail(name, format!("{error}"));
            }
        }
    }
    checks.push("codezConfig");
    if home.join("config.json").is_file() {
        if let Err(error) = crate::config::store::load_codez_config_checked() {
            fail("codezConfig", format!("{error:#}"));
        }
    }
    checks.push("sessionStore");
    let db = home.join("cutex-sessions.sqlite3");
    if db.is_file() {
        let opened = rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .and_then(|c| c.query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get::<_, i64>(0)));
        if let Err(error) = opened {
            fail("sessionStore", format!("{error}"));
        }
    }
    json!({
        "ok": failures.is_empty(),
        "failures": failures,
        "checks": checks,
        "build": crate::build_info::build_json(),
    })
}

/// `GET /v2/upgrade` / `cutex upgrade status` body for a daemon without a
/// managed install: nothing on disk is read.
pub fn unmanaged_status_view() -> Value {
    json!({
        "managed": false,
        "base": null,
        "serviceManager": super::service_manager::detect().kind,
        "build": crate::build_info::build_json(),
        "installed": null,
        "previousRelease": null,
        "installedReleases": [],
        "lastCheckAt": null,
        "lastCheckError": null,
        "available": null,
        "staged": null,
        "staging": null,
        "run": null,
        "followUp": null,
        "settings": Settings::default().unmanaged_view(),
        "trustedKeys": super::keys::trusted_keys().iter().map(|k| k.key_id_hex()).collect::<Vec<_>>(),
    })
}

/// `GET /v2/upgrade` / `cutex upgrade status` body.
pub fn status_view(layout: &Layout) -> Value {
    if !layout.is_managed() {
        return unmanaged_status_view();
    }
    let run = reconcile_lost_run(layout);
    let state = State::load(layout);
    let settings = Settings::load(layout);
    let record = layout.read_install().ok();
    json!({
        "managed": true,
        "base": layout.base.display().to_string(),
        "serviceManager": super::service_manager::detect().kind,
        "build": crate::build_info::build_json(),
        "installed": state.installed.clone().unwrap_or_else(|| stage::installed(layout)),
        "previousRelease": record.as_ref().and_then(|r| r.previous_release.clone()),
        "installedReleases": layout.installed_releases(),
        "lastCheckAt": state.last_check_at,
        "lastCheckError": state.last_check_error,
        "available": state.available,
        "staged": state.staged,
        // Background stage started through the API (`running` / `failed`).
        "staging": state.extra.get("staging").cloned(),
        "run": run,
        "followUp": state.follow_up,
        "settings": settings.view(layout),
        "trustedKeys": super::keys::trusted_keys().iter().map(|k| k.key_id_hex()).collect::<Vec<_>>(),
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::upgrade::manifest::fixtures::migration;
    use crate::upgrade::minisign::test_key::TestKey;
    use crate::upgrade::stage::fixtures::{installed_layout, publish, temp};

    fn staged_setup(class: MigrationClass) -> (Layout, Vec<PublicKey>, PathBuf) {
        let key = TestKey::new(7);
        let keys = vec![PublicKey::parse(&key.public_text()).unwrap()];
        let layout = installed_layout("r239", 40);
        let root = temp("control-src");
        publish(&root, &key, "r240", 41, None, |m| {
            m.cutex.migrations.push(Migration { new_stores: vec!["runtime/x".into()], ..migration("m1", "r240", class) });
        });
        let source = super::super::source::Source::parse(&root.display().to_string()).unwrap();
        stage::stage(&layout, &source, &keys, "release", "r240", &LocalRuntimes::default()).unwrap();
        (layout, keys, root)
    }

    #[test]
    fn apply_refusals_and_queueing() {
        let (layout, keys, root) = staged_setup(MigrationClass::OneWay);
        let error = prepare_apply(&layout, &keys, &ApplyRequest::default()).unwrap_err();
        assert_eq!(error.downcast_ref::<stage::UpgradeError>().unwrap().code, "one_way_unconfirmed");
        let wrong = ApplyRequest { release: Some("r241".into()), ..Default::default() };
        assert_eq!(prepare_apply(&layout, &keys, &wrong).unwrap_err().downcast_ref::<stage::UpgradeError>().unwrap().code, "not_staged");
        let request = ApplyRequest { confirm_one_way: Some("r240".into()), max_wait_secs: Some(5), ..Default::default() };
        let run = prepare_apply(&layout, &keys, &request).unwrap();
        assert_eq!((run.phase, run.mode, run.class), (Phase::Queued, Mode::Full, MigrationClass::OneWay));
        assert_eq!(run.from_release, "r239");
        // A second apply while the first is queued.
        let error = prepare_apply(&layout, &keys, &request).unwrap_err();
        let error = error.downcast_ref::<stage::UpgradeError>().unwrap();
        assert_eq!((error.code, error.details["reason"].as_str()), ("upgrade_blocked", Some("run_in_progress")));
        // Cancel while queued and no runner: cancelled here.
        assert_eq!(cancel(&layout).unwrap().phase, Phase::Cancelled);
        assert_eq!(cancel(&layout).unwrap_err().downcast_ref::<stage::UpgradeError>().unwrap().code, "no_run");
        assert_eq!(state::history(&layout, 5).len(), 1);
        std::fs::remove_dir_all(&layout.base).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn lost_runner_is_reconciled() {
        let (layout, keys, root) = staged_setup(MigrationClass::RollbackSafe);
        let mut run = prepare_apply(&layout, &keys, &ApplyRequest::default()).unwrap();
        run.enter(Phase::Swapping, None);
        state::record_run(&layout, &run).unwrap();
        let run = reconcile_lost_run(&layout).unwrap();
        assert_eq!(run.phase, Phase::NeedsOwner);
        assert!(prepare_apply(&layout, &keys, &ApplyRequest::default()).is_ok(), "a terminal run does not block");
        std::fs::remove_dir_all(&layout.base).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_rolled_back_apply_keeps_the_old_rollback_target() {
        let (layout, keys, root) = staged_setup(MigrationClass::RollbackSafe);
        let mut record = layout.read_install().unwrap();
        record.previous_release = Some("r238".into());
        layout.write_install(&record).unwrap();
        std::fs::create_dir_all(layout.release_dir("r238")).unwrap();
        std::fs::copy(layout.release_executable("r239"), layout.release_executable("r238")).unwrap();
        let mut run = prepare_apply(&layout, &keys, &ApplyRequest::default()).unwrap();
        // What the runner does on a failed verify: switch to r240, then back.
        crate::upgrade::swap::activate(&layout, "r240").unwrap();
        crate::upgrade::swap::activate(&layout, "r239").unwrap();
        assert_eq!(layout.read_install().unwrap().previous_release.as_deref(), Some("r240"));
        run.enter(Phase::RolledBack, None);
        restore_rollback_target(&layout, &run, Some("r238".into())).unwrap();
        assert_eq!(layout.read_install().unwrap().previous_release.as_deref(), Some("r238"));
        let (_, preview) = prepare_rollback(&layout, &RollbackRequest { dry_run: true, ..Default::default() }).unwrap();
        assert_eq!(preview["to"], "r238");
        // A committed apply keeps the swap's record.
        crate::upgrade::swap::activate(&layout, "r240").unwrap();
        run.enter(Phase::Committed, None);
        restore_rollback_target(&layout, &run, Some("r238".into())).unwrap();
        assert_eq!(layout.read_install().unwrap().previous_release.as_deref(), Some("r239"));
        std::fs::remove_dir_all(&layout.base).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_tampered_release_executable_is_never_started() {
        let (layout, keys, root) = staged_setup(MigrationClass::RollbackSafe);
        let run = prepare_apply(&layout, &keys, &ApplyRequest::default()).unwrap();
        // Replaced between staging and apply: refused before any spawn.
        let marker = layout.base.join("spawned");
        let original = std::fs::read(layout.release_executable("r240")).unwrap();
        std::fs::write(layout.release_executable("r240"), format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
        let error = start(&layout, &keys, run.clone()).unwrap_err();
        assert_eq!(error.downcast_ref::<stage::UpgradeError>().unwrap().code, "release_unverified");
        let recorded = state::read_run(&layout, &run.run_id).unwrap();
        assert_eq!((recorded.phase, recorded.error.unwrap().code.as_str()), (Phase::Failed, "release_unverified"));
        std::thread::sleep(Duration::from_millis(200));
        assert!(!marker.exists());
        // A rollback checks its target too (r239 here, recorded at install).
        std::fs::write(layout.release_executable("r240"), original).unwrap();
        crate::upgrade::swap::activate(&layout, "r240").unwrap();
        std::fs::write(layout.release_executable("r239"), b"#!/bin/sh\nexit 0\n").unwrap();
        let rollback = Run::new(RunKind::Rollback, "r240", "r239", Mode::Reload, MigrationClass::ReloadSafe, 0, PackageRun::None);
        assert_eq!(releases_started(&rollback), vec!["r240", "r239"]);
        let error = start(&layout, &keys, rollback).unwrap_err();
        assert!(format!("{error:#}").contains("release r239"), "{error:#}");
        std::fs::remove_dir_all(&layout.base).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_second_rollback_never_goes_forward() {
        let (layout, keys, root) = staged_setup(MigrationClass::RollbackSafe);
        // r239 → r240 committed.
        let mut apply = prepare_apply(&layout, &keys, &ApplyRequest::default()).unwrap();
        crate::upgrade::swap::activate(&layout, "r240").unwrap();
        apply.enter(Phase::Committed, None);
        state::record_run(&layout, &apply).unwrap();
        after_commit(&layout, &apply, &keys).unwrap();
        assert_eq!(layout.read_install().unwrap().previous_release.as_deref(), Some("r239"));
        // Owner rollback r240 → r239 committed.
        let (rollback, _) = prepare_rollback(&layout, &RollbackRequest::default()).unwrap();
        let mut rollback = rollback.unwrap();
        assert_eq!(rollback.to_release, "r239");
        crate::upgrade::swap::activate(&layout, "r239").unwrap();
        assert_eq!(layout.read_install().unwrap().previous_release.as_deref(), Some("r240"), "what the swap records");
        rollback.enter(Phase::Committed, None);
        state::record_run(&layout, &rollback).unwrap();
        after_commit(&layout, &rollback, &keys).unwrap();
        // r239 has no known predecessor: nothing to roll back to.
        assert_eq!(layout.read_install().unwrap().previous_release, None);
        let error = prepare_rollback(&layout, &RollbackRequest { dry_run: true, ..Default::default() }).unwrap_err();
        assert_eq!(error.downcast_ref::<stage::UpgradeError>().unwrap().code, "no_rollback_target");
        // An explicit target newer than the installed release is refused.
        let error = prepare_rollback(&layout, &RollbackRequest { to: Some("r240".into()), dry_run: true, ..Default::default() }).unwrap_err();
        let error = error.downcast_ref::<stage::UpgradeError>().unwrap();
        assert_eq!((error.code, &error.details["reason"]), ("no_rollback_target", &json!("newer_than_installed")));
        std::fs::remove_dir_all(&layout.base).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_committed_rollback_points_at_the_targets_own_predecessor() {
        let (layout, keys, root) = staged_setup(MigrationClass::RollbackSafe);
        // r238 (older, installed) → r239 was applied earlier.
        std::fs::create_dir_all(layout.release_dir("r238")).unwrap();
        std::fs::copy(layout.release_executable("r239"), layout.release_executable("r238")).unwrap();
        let mut older = Run::new(RunKind::Apply, "r238", "r239", Mode::Reload, MigrationClass::ReloadSafe, 0, PackageRun::None);
        older.enter(Phase::Committed, None);
        state::record_run(&layout, &older).unwrap();
        crate::upgrade::swap::activate(&layout, "r240").unwrap();
        let mut rollback = Run::new(RunKind::Rollback, "r240", "r239", Mode::Reload, MigrationClass::ReloadSafe, 0, PackageRun::None);
        crate::upgrade::swap::activate(&layout, "r239").unwrap();
        rollback.enter(Phase::Committed, None);
        state::record_run(&layout, &rollback).unwrap();
        after_commit(&layout, &rollback, &keys).unwrap();
        assert_eq!(layout.read_install().unwrap().previous_release.as_deref(), Some("r238"));
        std::fs::remove_dir_all(&layout.base).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn systemd_runner_inherits_the_home_selection() {
        let vars = |key: &str| match key {
            "CUTEX_HOME" => Some("/srv/inst".into()),
            "HOME" => Some("/tmp/h".into()),
            "CUTEX_TEST_PRIVATE_HOME" => Some("".into()),
            _ => None,
        };
        let args: Vec<String> = runner_setenv_args(vars).into_iter().map(|a| a.into_string().unwrap()).collect();
        assert_eq!(args, vec!["--setenv=CUTEX_HOME=/srv/inst".to_string(), "--setenv=HOME=/tmp/h".to_string()]);
    }

    #[test]
    fn unmanaged_install_is_blocked() {
        let layout = Layout::at(temp("control-unmanaged"));
        let error = prepare_apply(&layout, &[], &ApplyRequest::default()).unwrap_err();
        let error = error.downcast_ref::<stage::UpgradeError>().unwrap();
        assert_eq!((error.status, error.code, error.details["reason"].as_str()), (409, "upgrade_blocked", Some("unmanaged_install")));
        assert!(prepare_rollback(&layout, &RollbackRequest { dry_run: true, ..Default::default() }).is_err());
        assert_eq!(status_view(&layout)["managed"], false);
        let _ = std::fs::remove_dir_all(&layout.base);
    }

    #[test]
    fn rollback_preview_and_one_way_refusal() {
        let (layout, _keys, root) = staged_setup(MigrationClass::RollbackSafe);
        // Nothing before r239.
        let error = prepare_rollback(&layout, &RollbackRequest { dry_run: true, ..Default::default() }).unwrap_err();
        assert_eq!(error.downcast_ref::<stage::UpgradeError>().unwrap().code, "no_rollback_target");
        crate::upgrade::swap::activate(&layout, "r240").unwrap();
        let (run, preview) = prepare_rollback(&layout, &RollbackRequest { dry_run: true, ..Default::default() }).unwrap();
        assert!(run.is_none());
        assert_eq!((preview["from"].as_str(), preview["to"].as_str()), (Some("r240"), Some("r239")));
        assert_eq!(preview["movedAside"], json!(["runtime/x"]));
        let (run, _) = prepare_rollback(&layout, &RollbackRequest::default()).unwrap();
        let run = run.unwrap();
        assert_eq!((run.kind, run.mode), (RunKind::Rollback, Mode::Reload));
        assert_eq!(runner_executable(&layout, &run), layout.release_executable("r240"));
        std::fs::remove_dir_all(&layout.base).unwrap();
        std::fs::remove_dir_all(&root).unwrap();

        let (layout, _keys, root) = staged_setup(MigrationClass::OneWay);
        crate::upgrade::swap::activate(&layout, "r240").unwrap();
        let error = prepare_rollback(&layout, &RollbackRequest { dry_run: true, ..Default::default() }).unwrap_err();
        assert_eq!(error.downcast_ref::<stage::UpgradeError>().unwrap().details["reason"], "one_way");
        std::fs::remove_dir_all(&layout.base).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
    }
}
