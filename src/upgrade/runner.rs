//! The apply / rollback state machine (RFC §4.2, §4.5), pure over [`Host`].
//!
//! ```text
//! apply: wait-idle (≤ maxWait, else deferred; cancel possible)
//!        → [packageRun=during: quiescing] → stopping → backing-up → swapping
//!        → starting → waiting-ready → [during: package-run → restoring-online]
//!        → verifying ──ok──► committed
//!                    └─fail─► rolling-back → rolled-back | needs-owner
//! ```
//!
//! The commit point is a passed verify; nothing after it rolls back by
//! itself. A one-way release never rolls back automatically (needs-owner).
//! A rollback never restores session data: only the binaries, the
//! selection files and moving the interval's new stores aside.
//! Executed by the NEW binary (`<releases/r>/cutex upgrade run <id>`),
//! outside the service tree.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::manifest::{Migration, MigrationClass, Mode};
use super::state::{Busy, NeedsOwner, PackageRun, Phase, Run, RunKind};

const IDLE_POLL: Duration = Duration::from_secs(5);
/// How long verify waits for the runtimes that were online before to come
/// back (only when that is the sole remaining problem). Measured on real
/// deploys (r224 – r227, one Windows and one Linux host): 78 – 88 s for 20 – 22 runtimes,
/// i.e. ~4 s each after the daemons are ready. So: a 60 s base plus 6 s per
/// expected runtime (1.5x the observed per-runtime time), at least 180 s
/// (2x the slowest observed full recovery) and at most 15 min, so a deploy
/// whose runtimes never return still rolls back in bounded time.
const ONLINE_CONVERGE_BASE: Duration = Duration::from_secs(60);
const ONLINE_CONVERGE_PER_RUNTIME: Duration = Duration::from_secs(6);
const ONLINE_CONVERGE_MIN: Duration = Duration::from_secs(180);
const ONLINE_CONVERGE_MAX: Duration = Duration::from_secs(900);

fn online_converge(runtimes: usize) -> Duration {
    let per = ONLINE_CONVERGE_PER_RUNTIME.saturating_mul(u32::try_from(runtimes).unwrap_or(u32::MAX));
    ONLINE_CONVERGE_BASE.saturating_add(per).clamp(ONLINE_CONVERGE_MIN, ONLINE_CONVERGE_MAX)
}

/// What the runner observes through the Management API.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub release: Option<String>,
    pub commit: Option<String>,
    pub features: BTreeSet<String>,
    /// Codes of `error`-severity health findings.
    pub health_errors: BTreeSet<String>,
    /// Sessions whose runtime is online.
    pub online: BTreeSet<String>,
}

impl Snapshot {
    /// The daemon runs release `label`. A source build reports no release label, only its commit:
    /// it matches the managed install's `dev-<commit7>` label by commit prefix.
    pub fn runs_release(&self, label: &str) -> bool {
        match (self.release.as_deref(), label.strip_prefix("dev-"), self.commit.as_deref()) {
            (Some(release), _, _) => release == label,
            (None, Some(short), Some(commit)) => !short.is_empty() && commit.starts_with(short),
            _ => false,
        }
    }
}

pub trait Host {
    fn busy(&self) -> anyhow::Result<Vec<Busy>>;
    fn snapshot(&self) -> anyhow::Result<Snapshot>;
    fn set_offline(&self, cutex_session_id: &str) -> anyhow::Result<()>;
    fn set_online(&self, cutex_session_id: &str) -> anyhow::Result<()>;
    /// Start a runtime-package run for every outdated agent and wait for it;
    /// `None` when nothing was outdated.
    fn package_run_all(&self) -> anyhow::Result<Option<String>>;
    fn revert_package_run(&self, run_id: &str) -> anyhow::Result<()>;
    fn wait_ready(&self, timeout: Duration) -> anyhow::Result<()>;
    fn stop_services(&self) -> anyhow::Result<()>;
    fn start_services(&self) -> anyhow::Result<()>;
    fn backup(&self, dir: &Path) -> anyhow::Result<()>;
    /// Switch the services' executable; returns the previous release.
    fn activate(&self, release: &str) -> anyhow::Result<String>;
    /// Full mode: select the release's runtime package for new agents.
    fn select_runtime(&self, release: &str) -> anyhow::Result<bool>;
    fn restore_selection(&self, backup: &Path, displaced: &Path) -> anyhow::Result<Vec<String>>;
    fn move_aside(&self, store: &str, displaced: &Path) -> anyhow::Result<bool>;
    /// Rollback preparation of these migrations (run by the newer binary
    /// while it still runs): hazards that would lose data (empty = safe).
    fn rollback_prep(&self, migrations: &[Migration]) -> anyhow::Result<Vec<String>>;
    fn cancel_requested(&self, run_id: &str) -> bool;
    fn record(&self, run: &Run);
    fn sleep(&self, duration: Duration);
    fn now(&self) -> Instant;
    /// Swap phase (services stopped, upgrade.lock held): select the
    /// component versions the now-active release names for installed
    /// components (crate::components::select_for_release). Notes for the run;
    /// a component failure never fails the run.
    fn select_components(&self, _run_dir: &Path) -> Vec<String> {
        Vec::new()
    }
    /// Rollback: reselect the component versions selected before the swap.
    fn restore_components(&self, _run_dir: &Path) -> Vec<String> {
        Vec::new()
    }
    /// Rollback swap (services stopped): rewrite records release `target`
    /// cannot parse or would read with more rights (`super::older_stores`);
    /// notes of what changed.
    fn prepare_older_stores(&self, _displaced: &Path, _target: &str) -> anyhow::Result<Vec<String>> {
        Ok(Vec::new())
    }
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub to_release: String,
    pub to_commit: Option<String>,
    pub mode: Mode,
    pub class: MigrationClass,
    /// Migrations in (from, to] (apply) or (to, from] (rollback).
    pub migrations: Vec<Migration>,
    pub capabilities_added: Vec<String>,
    pub capabilities_removed: Vec<String>,
    pub package_run: PackageRun,
    pub max_wait: Duration,
    pub ready_timeout: Duration,
    pub run_dir: PathBuf,
    /// Rollback: the backup whose selection files come back.
    pub selection_backup: Option<PathBuf>,
}

fn quiesce_needed(plan: &Plan) -> bool {
    plan.mode == Mode::Full && plan.package_run == PackageRun::During
}

enum Wait {
    Idle,
    Stop,
}

/// wait-idle; `Stop` = the run reached a terminal phase (recorded).
fn wait_idle(run: &mut Run, plan: &Plan, host: &dyn Host, deadline: Instant) -> Wait {
    if run.phase != Phase::WaitingIdle {
        run.enter(Phase::WaitingIdle, None);
        host.record(run);
    }
    loop {
        if host.cancel_requested(&run.run_id) {
            run.enter(Phase::Cancelled, Some("cancelled by the owner".into()));
            host.record(run);
            return Wait::Stop;
        }
        match host.busy() {
            Ok(busy) if busy.is_empty() => {
                if !run.waiting_for.is_empty() {
                    run.waiting_for.clear();
                    host.record(run);
                }
                return Wait::Idle;
            }
            Ok(busy) => {
                if run.waiting_for != busy {
                    run.waiting_for = busy;
                    host.record(run);
                }
            }
            Err(error) => {
                run.fail(Phase::Failed, "management_unreachable", format!("cannot tell whether agents are idle: {error:#}"));
                host.record(run);
                return Wait::Stop;
            }
        }
        if host.now() >= deadline {
            let detail = format!("not idle within {}s; nothing changed, retry later", plan.max_wait.as_secs());
            run.error = Some(super::state::RunError { code: "not_idle".into(), message: detail.clone() });
            run.enter(Phase::Deferred, Some(detail));
            host.record(run);
            return Wait::Stop;
        }
        host.sleep(IDLE_POLL);
    }
}

/// Take the online runtimes offline one by one, re-checking before each;
/// a turn that starts brings the offlined ones back (`Err`).
fn quiesce(run: &mut Run, host: &dyn Host, online: &BTreeSet<String>) -> Result<Vec<String>, Vec<String>> {
    run.enter(Phase::Quiescing, Some(format!("{} runtimes", online.len())));
    host.record(run);
    let mut offlined: Vec<String> = Vec::new();
    for id in online {
        let idle = host.busy().map(|busy| busy.is_empty()).unwrap_or(false);
        let ok = idle && host.set_offline(id).is_ok();
        if !ok {
            for back in &offlined {
                let _ = host.set_online(back);
            }
            return Err(offlined);
        }
        offlined.push(id.clone());
    }
    Ok(offlined)
}

/// Capabilities derived from a live probe of an optional component (the
/// Job Service), not from the cutex build: a component restart or failure
/// around the swap must not read as "capabilities lost" and roll cutex back
/// (components contract §0.3). Excluded from the before / after diff.
pub const HOST_DEPENDENT_CAPABILITIES: &[&str] = &["job-external", "job-source-nci"];

fn host_dependent(feature: &str) -> bool {
    HOST_DEPENDENT_CAPABILITIES.contains(&feature)
}

fn verify(plan: &Plan, before: &Snapshot, host: &dyn Host, expect_online: &BTreeSet<String>) -> Result<(), String> {
    let deadline = host.now() + online_converge(expect_online.len());
    loop {
        let after = host.snapshot().map_err(|e| format!("snapshot after start: {e:#}"))?;
        let mut problems = Vec::new();
        if after.release.as_deref() != Some(plan.to_release.as_str()) {
            problems.push(format!("cutexBuild.release is {:?}, expected {}", after.release, plan.to_release));
        }
        if let (Some(expected), Some(actual)) = (&plan.to_commit, &after.commit) {
            let n = expected.len().min(actual.len()).min(40);
            if n < 7 || expected[..n] != actual[..n] {
                problems.push(format!("cutexBuild.commit is {actual}, expected {expected}"));
            }
        } else if plan.to_commit.is_some() {
            problems.push("cutexBuild.commit missing".into());
        }
        let removed: BTreeSet<&String> = plan.capabilities_removed.iter().collect();
        let lost: Vec<&String> =
            before.features.iter().filter(|f| !removed.contains(f) && !host_dependent(f) && !after.features.contains(*f)).collect();
        if !lost.is_empty() {
            problems.push(format!("capabilities lost: {lost:?}"));
        }
        let missing: Vec<&String> =
            plan.capabilities_added.iter().filter(|f| !host_dependent(f) && !after.features.contains(*f)).collect();
        if !missing.is_empty() {
            problems.push(format!("capabilities missing: {missing:?}"));
        }
        let new_errors: Vec<&String> = after.health_errors.difference(&before.health_errors).collect();
        if !new_errors.is_empty() {
            problems.push(format!("new error-level health findings: {new_errors:?}"));
        }
        let offline: Vec<&String> = expect_online.difference(&after.online).collect();
        let only_online = problems.is_empty() && !offline.is_empty();
        if problems.is_empty() && offline.is_empty() {
            return Ok(());
        }
        if !only_online || host.now() >= deadline {
            if !offline.is_empty() {
                problems.push(format!("not back online: {offline:?}"));
            }
            return Err(problems.join("; "));
        }
        host.sleep(Duration::from_secs(3));
    }
}

fn needs_owner(run: &mut Run, host: &dyn Host, reason: String, commands: Vec<String>) {
    run.needs_owner = Some(NeedsOwner { reason: reason.clone(), commands });
    run.enter(Phase::NeedsOwner, Some(reason));
    host.record(run);
}

/// Steps back to `from` after a failure before the commit point. Every step
/// failure ends in needs-owner (never a second destructive attempt).
fn roll_back(
    run: &mut Run,
    plan: &Plan,
    host: &dyn Host,
    from: &str,
    backup: Option<&Path>,
    swapped: bool,
    offlined: &[String],
) {
    let displaced = plan.run_dir.join("displaced");
    run.enter(Phase::RollingBack, run.error.as_ref().map(|e| e.message.clone()));
    host.record(run);
    let manual = vec![
        "cutex upgrade status".to_string(),
        format!("cutex upgrade rollback --to {from}"),
        "cutex service status".to_string(),
    ];
    if plan.class == MigrationClass::OneWay && swapped {
        // The new binary may already have migrated state the old one cannot read.
        let _ = host.start_services();
        return needs_owner(
            run,
            host,
            format!("{} is a one-way upgrade and failed before its commit point; no automatic rollback (fix forward or restore from {})",
                plan.to_release, backup.map_or("the run backup".into(), |b| b.display().to_string())),
            manual,
        );
    }
    if let Some(id) = run.package_run_id.clone() {
        if let Err(error) = host.revert_package_run(&id) {
            return needs_owner(run, host, format!("could not revert runtime package run {id}: {error:#}"), manual);
        }
    }
    if swapped {
        match host.rollback_prep(&plan.migrations) {
            Ok(hazards) if hazards.is_empty() => {}
            Ok(hazards) => return needs_owner(run, host, format!("rollback would lose data: {}", hazards.join("; ")), manual),
            Err(error) => return needs_owner(run, host, format!("rollback preparation failed: {error:#}"), manual),
        }
        // Stopping is idempotent; the new release may be running.
        if let Err(error) = host.stop_services() {
            return needs_owner(run, host, format!("could not stop the services for the rollback: {error:#}"), manual);
        }
        if let Err(error) = host.activate(from) {
            return needs_owner(run, host, format!("could not switch back to {from}: {error:#}"), manual);
        }
        if plan.mode == Mode::Full {
            if let Some(backup) = backup {
                match host.restore_selection(backup, &displaced) {
                    Ok(items) => run.displaced.extend(items),
                    Err(error) => return needs_owner(run, host, format!("could not restore the runtime selection: {error:#}"), manual),
                }
            }
        }
        for store in plan.migrations.iter().flat_map(|m| m.new_stores.iter()) {
            match host.move_aside(store, &displaced) {
                Ok(true) => run.displaced.push(store.clone()),
                Ok(false) => {}
                Err(error) => return needs_owner(run, host, format!("could not move {store} aside: {error:#}"), manual),
            }
        }
        match host.prepare_older_stores(&displaced, from) {
            Ok(notes) => run.displaced.extend(notes),
            Err(error) => return needs_owner(run, host, format!("could not prepare the stores for {from}: {error:#}"), manual),
        }
        let _ = host.restore_components(&plan.run_dir);
    }
    if let Err(error) = host.start_services().and_then(|_| host.wait_ready(plan.ready_timeout)) {
        return needs_owner(run, host, format!("{from} did not come back: {error:#}"), manual);
    }
    for id in offlined {
        let _ = host.set_online(id);
    }
    match host.snapshot() {
        Ok(snapshot) if !swapped || snapshot.runs_release(from) || from == "unknown" => {
            run.enter(Phase::RolledBack, Some(format!("back on {from}")));
            host.record(run);
        }
        Ok(snapshot) => needs_owner(run, host, format!("after the rollback the daemon reports {:?}, expected {from}", snapshot.release), manual),
        Err(error) => needs_owner(run, host, format!("cannot read the daemon after the rollback: {error:#}"), manual),
    }
}

/// Apply `plan` (run.from_release → plan.to_release). `run` is recorded at
/// every transition.
pub fn apply(run: &mut Run, plan: &Plan, host: &dyn Host) {
    let started = host.now();
    let deadline = started + plan.max_wait;
    let from = run.from_release.clone();
    // wait-idle, then quiesce (full mode with a package run during activation)
    let (before, offlined) = loop {
        if let Wait::Stop = wait_idle(run, plan, host, deadline) {
            return;
        }
        let before = match host.snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                run.fail(Phase::Failed, "management_unreachable", format!("{error:#}"));
                host.record(run);
                return;
            }
        };
        if !quiesce_needed(plan) {
            break (before, Vec::new());
        }
        match quiesce(run, host, &before.online) {
            Ok(offlined) => break (before, offlined),
            Err(_) => {
                run.enter(Phase::WaitingIdle, Some("a turn started while quiescing; runtimes brought back".into()));
                host.record(run);
            }
        }
    };
    let expect_online: BTreeSet<String> = before.online.clone();
    // stop
    run.enter(Phase::Stopping, None);
    host.record(run);
    let stopped_at = host.now();
    if let Err(error) = host.stop_services() {
        let _ = host.start_services();
        for id in &offlined {
            let _ = host.set_online(id);
        }
        run.fail(Phase::Failed, "stop_failed", format!("could not stop the services: {error:#}"));
        host.record(run);
        return;
    }
    // backup (services stopped)
    run.enter(Phase::BackingUp, None);
    let backup = plan.run_dir.join("backup");
    run.backup_dir = Some(backup.display().to_string());
    host.record(run);
    if let Err(error) = host.backup(&backup) {
        run.fail(Phase::RollingBack, "backup_failed", format!("{error:#}"));
        return roll_back(run, plan, host, &from, None, false, &offlined);
    }
    // swap
    run.enter(Phase::Swapping, None);
    host.record(run);
    if let Err(error) = host.activate(&plan.to_release) {
        run.error = Some(super::state::RunError { code: "swap_failed".into(), message: format!("{error:#}") });
        // activate may have half-switched (Windows: one of two files).
        return roll_back(run, plan, host, &from, Some(&backup), true, &offlined);
    }
    if plan.mode == Mode::Full {
        if let Err(error) = host.select_runtime(&plan.to_release) {
            run.error = Some(super::state::RunError { code: "select_failed".into(), message: format!("{error:#}") });
            return roll_back(run, plan, host, &from, Some(&backup), true, &offlined);
        }
    }
    // Components follow the new release before any package run (contract §0.2).
    let component_notes = host.select_components(&plan.run_dir);
    // start
    run.enter(Phase::Starting, None);
    host.record(run);
    if let Err(error) = host.start_services() {
        run.error = Some(super::state::RunError { code: "start_failed".into(), message: format!("{error:#}") });
        return roll_back(run, plan, host, &from, Some(&backup), true, &offlined);
    }
    run.enter(Phase::WaitingReady, None);
    host.record(run);
    if let Err(error) = host.wait_ready(plan.ready_timeout) {
        run.error = Some(super::state::RunError { code: "not_ready".into(), message: format!("{error:#}") });
        return roll_back(run, plan, host, &from, Some(&backup), true, &offlined);
    }
    run.downtime_ms = Some((host.now() - stopped_at).as_millis() as u64);
    // package run while runtimes are offline (owner-host style)
    if quiesce_needed(plan) {
        run.enter(Phase::PackageRun, None);
        host.record(run);
        match host.package_run_all() {
            Ok(id) => run.package_run_id = id,
            Err(error) => {
                run.error = Some(super::state::RunError { code: "package_run_failed".into(), message: format!("{error:#}") });
                return roll_back(run, plan, host, &from, Some(&backup), true, &offlined);
            }
        }
        run.enter(Phase::RestoringOnline, None);
        host.record(run);
        for id in &offlined {
            let _ = host.set_online(id);
        }
    }
    // verify (the commit point)
    run.enter(Phase::Verifying, None);
    host.record(run);
    match verify(plan, &before, host, &expect_online) {
        Ok(()) => {
            let mut detail = format!("{from} → {}", plan.to_release);
            for note in &component_notes {
                detail.push_str("; ");
                detail.push_str(note);
            }
            run.enter(Phase::Committed, Some(detail));
            host.record(run);
        }
        Err(problems) => {
            run.error = Some(super::state::RunError { code: "verify_failed".into(), message: problems });
            roll_back(run, plan, host, &from, Some(&backup), true, &[]);
        }
    }
}

/// Owner rollback to `plan.to_release` (the previous committed release),
/// driven by the running (newer) binary.
pub fn rollback(run: &mut Run, plan: &Plan, host: &dyn Host) {
    let deadline = host.now() + plan.max_wait;
    if let Wait::Stop = wait_idle(run, plan, host, deadline) {
        return;
    }
    let manual = vec!["cutex upgrade status".to_string(), "cutex service status".to_string()];
    match host.rollback_prep(&plan.migrations) {
        Ok(hazards) if hazards.is_empty() => {}
        Ok(hazards) => {
            run.fail(Phase::Failed, "rollback_hazard", format!("rollback would lose data: {}", hazards.join("; ")));
            host.record(run);
            return;
        }
        Err(error) => {
            run.fail(Phase::Failed, "rollback_hazard", format!("rollback preparation failed: {error:#}"));
            host.record(run);
            return;
        }
    }
    run.enter(Phase::Stopping, None);
    host.record(run);
    let stopped_at = host.now();
    if let Err(error) = host.stop_services() {
        let _ = host.start_services();
        run.fail(Phase::Failed, "stop_failed", format!("{error:#}"));
        host.record(run);
        return;
    }
    run.enter(Phase::BackingUp, None);
    let backup = plan.run_dir.join("backup");
    run.backup_dir = Some(backup.display().to_string());
    host.record(run);
    if let Err(error) = host.backup(&backup) {
        let _ = host.start_services();
        run.fail(Phase::Failed, "backup_failed", format!("{error:#}"));
        host.record(run);
        return;
    }
    run.enter(Phase::Swapping, None);
    host.record(run);
    let displaced = plan.run_dir.join("displaced");
    let result = (|| -> anyhow::Result<()> {
        host.activate(&plan.to_release)?;
        if let Some(selection) = &plan.selection_backup {
            run.displaced.extend(host.restore_selection(selection, &displaced)?);
        }
        for store in plan.migrations.iter().flat_map(|m| m.new_stores.iter()) {
            if host.move_aside(store, &displaced)? {
                run.displaced.push(store.clone());
            }
        }
        run.displaced.extend(host.prepare_older_stores(&displaced, &plan.to_release)?);
        // The versions the rollback target names (a release without a
        // components section leaves them as they are).
        let _ = host.select_components(&plan.run_dir);
        Ok(())
    })();
    if let Err(error) = result {
        let _ = host.start_services();
        return needs_owner(run, host, format!("rollback switch failed: {error:#}"), manual);
    }
    run.enter(Phase::Starting, None);
    host.record(run);
    if let Err(error) = host.start_services().and_then(|_| host.wait_ready(plan.ready_timeout)) {
        return needs_owner(run, host, format!("{} did not start: {error:#}", plan.to_release), manual);
    }
    run.downtime_ms = Some((host.now() - stopped_at).as_millis() as u64);
    run.enter(Phase::Verifying, None);
    host.record(run);
    match host.snapshot() {
        Ok(snapshot) if snapshot.runs_release(&plan.to_release) => {
            run.enter(Phase::Committed, Some(format!("rolled back to {}", plan.to_release)));
            host.record(run);
        }
        Ok(snapshot) => needs_owner(run, host, format!("the daemon reports {:?}, expected {}", snapshot.release, plan.to_release), manual),
        Err(error) => needs_owner(run, host, format!("cannot read the daemon: {error:#}"), manual),
    }
}

pub fn new_run(kind: RunKind, from: &str, plan: &Plan) -> Run {
    Run::new(kind, from, &plan.to_release, plan.mode, plan.class, plan.max_wait.as_secs(), plan.package_run)
}

#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::cell::{Cell, RefCell};

    /// Scriptable host: `fail` names steps that error; `busy_script` yields
    /// busy lists for successive polls.
    pub struct FakeHost {
        pub log: RefCell<Vec<String>>,
        pub fail: RefCell<BTreeSet<&'static str>>,
        pub busy_script: RefCell<Vec<Vec<Busy>>>,
        pub running: RefCell<String>,
        pub commit: RefCell<String>,
        pub features: RefCell<BTreeSet<String>>,
        pub health_errors: RefCell<BTreeSet<String>>,
        pub online: RefCell<BTreeSet<String>>,
        pub clock: Cell<Duration>,
        pub start: Instant,
        pub records: RefCell<Vec<Run>>,
        pub hazards: RefCell<Vec<String>>,
        pub cancel: Cell<bool>,
        pub new_release_commit: String,
        /// A runtime that comes back online only at this fake time.
        pub online_late: RefCell<Option<(Duration, String)>>,
        /// Features that disappear when the new release is activated.
        pub drop_on_swap: RefCell<Vec<String>>,
    }

    impl FakeHost {
        pub fn new(running: &str) -> Self {
            Self {
                log: RefCell::new(Vec::new()),
                fail: RefCell::new(BTreeSet::new()),
                busy_script: RefCell::new(Vec::new()),
                running: RefCell::new(running.into()),
                commit: RefCell::new("1111111aaaa".into()),
                features: RefCell::new(["a", "b"].iter().map(|s| s.to_string()).collect()),
                health_errors: RefCell::new(BTreeSet::new()),
                online: RefCell::new(["cutex.x", "cutex.y"].iter().map(|s| s.to_string()).collect()),
                clock: Cell::new(Duration::ZERO),
                start: Instant::now(),
                records: RefCell::new(Vec::new()),
                hazards: RefCell::new(Vec::new()),
                cancel: Cell::new(false),
                new_release_commit: "5f30a7cad4dcd8435e797592a380349df509e35d".into(),
                online_late: RefCell::new(None),
                drop_on_swap: RefCell::new(Vec::new()),
            }
        }
        fn step(&self, name: &'static str) -> anyhow::Result<()> {
            self.log.borrow_mut().push(name.to_string());
            if self.fail.borrow().contains(name) {
                anyhow::bail!("{name} failed (fake)");
            }
            Ok(())
        }
        pub fn fail_features_after_swap(&self, features: &[&str]) {
            self.drop_on_swap.borrow_mut().extend(features.iter().map(|f| f.to_string()));
        }
        pub fn steps(&self) -> Vec<String> {
            self.log.borrow().clone()
        }
        pub fn phases(&self) -> Vec<Phase> {
            let mut phases: Vec<Phase> = Vec::new();
            for run in self.records.borrow().iter() {
                if phases.last() != Some(&run.phase) {
                    phases.push(run.phase);
                }
            }
            phases
        }
    }

    impl Host for FakeHost {
        fn busy(&self) -> anyhow::Result<Vec<Busy>> {
            self.step("busy")?;
            let mut script = self.busy_script.borrow_mut();
            Ok(if script.is_empty() { Vec::new() } else { script.remove(0) })
        }
        fn snapshot(&self) -> anyhow::Result<Snapshot> {
            self.step("snapshot")?;
            if let Some((at, id)) = self.online_late.borrow().clone() {
                if self.clock.get() >= at {
                    self.online.borrow_mut().insert(id);
                }
            }
            let running = self.running.borrow().clone();
            Ok(Snapshot {
                commit: Some(if running.starts_with("r240") { self.new_release_commit.clone() } else { self.commit.borrow().clone() }),
                release: Some(running),
                features: self.features.borrow().clone(),
                health_errors: self.health_errors.borrow().clone(),
                online: self.online.borrow().clone(),
            })
        }
        fn set_offline(&self, id: &str) -> anyhow::Result<()> {
            self.step("offline")?;
            self.online.borrow_mut().remove(id);
            Ok(())
        }
        fn set_online(&self, id: &str) -> anyhow::Result<()> {
            self.step("online")?;
            self.online.borrow_mut().insert(id.to_string());
            Ok(())
        }
        fn package_run_all(&self) -> anyhow::Result<Option<String>> {
            self.step("package_run")?;
            Ok(Some("rpu-1".into()))
        }
        fn revert_package_run(&self, _run_id: &str) -> anyhow::Result<()> {
            self.step("revert_package_run")
        }
        fn wait_ready(&self, _timeout: Duration) -> anyhow::Result<()> {
            self.step("wait_ready")
        }
        fn stop_services(&self) -> anyhow::Result<()> {
            self.step("stop")
        }
        fn start_services(&self) -> anyhow::Result<()> {
            self.step("start")
        }
        fn backup(&self, _dir: &Path) -> anyhow::Result<()> {
            self.step("backup")
        }
        fn activate(&self, release: &str) -> anyhow::Result<String> {
            self.step(if release.starts_with("r240") { "activate_new" } else { "activate_old" })?;
            if release.starts_with("r240") {
                for f in self.drop_on_swap.borrow().iter() {
                    self.features.borrow_mut().remove(f);
                }
            }
            Ok(self.running.replace(release.to_string()))
        }
        fn select_runtime(&self, _release: &str) -> anyhow::Result<bool> {
            self.step("select")?;
            Ok(true)
        }
        fn restore_selection(&self, _backup: &Path, _displaced: &Path) -> anyhow::Result<Vec<String>> {
            self.step("restore_selection")?;
            Ok(vec!["runtime/light/deployment.json".into()])
        }
        fn move_aside(&self, _store: &str, _displaced: &Path) -> anyhow::Result<bool> {
            self.step("move_aside")?;
            Ok(true)
        }
        fn rollback_prep(&self, _migrations: &[Migration]) -> anyhow::Result<Vec<String>> {
            self.step("rollback_prep")?;
            Ok(self.hazards.borrow().clone())
        }
        fn cancel_requested(&self, _run_id: &str) -> bool {
            self.cancel.get()
        }
        fn record(&self, run: &Run) {
            self.records.borrow_mut().push(run.clone());
        }
        fn sleep(&self, duration: Duration) {
            self.clock.set(self.clock.get() + duration);
        }
        fn now(&self) -> Instant {
            self.start + self.clock.get()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeHost;
    use super::*;
    use crate::upgrade::manifest::fixtures::migration;

    fn plan(mode: Mode, class: MigrationClass, package_run: PackageRun) -> Plan {
        Plan {
            to_release: "r240".into(),
            to_commit: Some("5f30a7cad4dcd8435e797592a380349df509e35d".into()),
            mode,
            class,
            migrations: vec![super::super::manifest::Migration {
                new_stores: vec!["runtime/new-store".into()],
                ..migration("m1", "r240", class)
            }],
            capabilities_added: vec!["c".into()],
            capabilities_removed: vec!["b".into()],
            package_run,
            max_wait: Duration::from_secs(60),
            ready_timeout: Duration::from_secs(120),
            run_dir: PathBuf::from("/nonexistent/run"),
            selection_backup: None,
        }
    }

    fn go(host: &FakeHost, plan: &Plan) -> Run {
        let mut run = new_run(RunKind::Apply, "r239", plan);
        // The new release advertises `c` and drops `b`.
        host.features.borrow_mut().insert("c".into());
        apply(&mut run, plan, host);
        run
    }

    #[test]
    fn a_source_build_matches_its_dev_label_by_commit() {
        let snap = |release: Option<&str>, commit: Option<&str>| Snapshot { release: release.map(Into::into), commit: commit.map(Into::into), ..Snapshot::default() };
        assert!(snap(None, Some("656cd91aa")).runs_release("dev-656cd91"));
        assert!(!snap(None, Some("1234567")).runs_release("dev-656cd91"));
        assert!(!snap(None, None).runs_release("dev-656cd91"));
        assert!(!snap(None, Some("656cd91")).runs_release("dev-"));
        assert!(!snap(None, Some("656cd91")).runs_release("0.1.0"));
        assert!(snap(Some("0.1.0"), Some("x")).runs_release("0.1.0"));
        assert!(!snap(Some("0.1.1"), Some("656cd91")).runs_release("dev-656cd91"));
    }

    #[test]
    fn reload_upgrade_commits_without_touching_the_selection() {
        let host = FakeHost::new("r239");
        let plan = Plan { migrations: Vec::new(), class: MigrationClass::ReloadSafe, ..plan(Mode::Reload, MigrationClass::ReloadSafe, PackageRun::After) };
        let run = go(&host, &plan);
        assert_eq!(run.phase, Phase::Committed, "{:?}", run.error);
        assert_eq!(host.steps(), vec!["busy", "snapshot", "stop", "backup", "activate_new", "start", "wait_ready", "snapshot"]);
        assert!(run.downtime_ms.is_some());
        assert_eq!(
            host.phases(),
            vec![Phase::WaitingIdle, Phase::Stopping, Phase::BackingUp, Phase::Swapping, Phase::Starting, Phase::WaitingReady, Phase::Verifying, Phase::Committed]
        );
    }

    #[test]
    fn full_upgrade_with_package_run_during_quiesces_and_restores() {
        let host = FakeHost::new("r239");
        let run = go(&host, &plan(Mode::Full, MigrationClass::RollbackSafe, PackageRun::During));
        assert_eq!(run.phase, Phase::Committed, "{:?}", run.error);
        let steps = host.steps();
        assert_eq!(steps.iter().filter(|s| *s == "offline").count(), 2);
        assert_eq!(steps.iter().filter(|s| *s == "online").count(), 2);
        assert!(steps.contains(&"select".to_string()) && steps.contains(&"package_run".to_string()));
        assert_eq!(run.package_run_id.as_deref(), Some("rpu-1"));
    }

    #[test]
    fn full_upgrade_with_package_run_after_keeps_runtimes_online() {
        let host = FakeHost::new("r239");
        let run = go(&host, &plan(Mode::Full, MigrationClass::RollbackSafe, PackageRun::After));
        assert_eq!(run.phase, Phase::Committed);
        assert!(!host.steps().iter().any(|s| s == "offline" || s == "package_run"));
        assert!(host.steps().contains(&"select".to_string()));
    }

    #[test]
    fn waits_for_idle_and_defers_without_changes() {
        let host = FakeHost::new("r239");
        let busy = Busy { reason: "activeTurn".into(), cutex_session_id: Some("cutex.x".into()), name: Some("X".into()), detail: None };
        *host.busy_script.borrow_mut() = vec![vec![busy.clone()]; 1000];
        let run = go(&host, &plan(Mode::Reload, MigrationClass::ReloadSafe, PackageRun::After));
        assert_eq!(run.phase, Phase::Deferred);
        assert_eq!(run.error.as_ref().unwrap().code, "not_idle");
        assert!(!host.steps().iter().any(|s| s == "stop"), "nothing changed");
        assert!(host.records.borrow().iter().any(|r| r.waiting_for == vec![busy.clone()]));
        // Becomes idle after two polls.
        let host = FakeHost::new("r239");
        *host.busy_script.borrow_mut() = vec![vec![busy.clone()], vec![busy]];
        let run = go(&host, &Plan { migrations: Vec::new(), ..plan(Mode::Reload, MigrationClass::ReloadSafe, PackageRun::After) });
        assert_eq!(run.phase, Phase::Committed);
        assert!(run.waiting_for.is_empty());
    }

    #[test]
    fn cancel_while_waiting() {
        let host = FakeHost::new("r239");
        host.cancel.set(true);
        let run = go(&host, &plan(Mode::Reload, MigrationClass::ReloadSafe, PackageRun::After));
        assert_eq!(run.phase, Phase::Cancelled);
        assert!(!host.steps().iter().any(|s| s == "stop"));
    }

    #[test]
    fn a_turn_starting_while_quiescing_brings_runtimes_back() {
        let host = FakeHost::new("r239");
        let busy = Busy { reason: "activeTurn".into(), cutex_session_id: Some("cutex.y".into()), name: None, detail: None };
        // idle (wait), idle (before x), busy (before y) -> back to waiting, then idle.
        *host.busy_script.borrow_mut() = vec![vec![], vec![], vec![busy]];
        let run = go(&host, &plan(Mode::Full, MigrationClass::RollbackSafe, PackageRun::During));
        assert_eq!(run.phase, Phase::Committed);
        let steps = host.steps();
        let first_online = steps.iter().position(|s| s == "online").unwrap();
        let stop = steps.iter().position(|s| s == "stop").unwrap();
        assert!(first_online < stop, "cutex.x brought back before the retry: {steps:?}");
        assert!(host.records.borrow().iter().any(|r| r.timeline.iter().any(|t| t.detail.as_deref().is_some_and(|d| d.contains("quiescing")))));
    }

    #[test]
    fn a_job_service_down_after_the_swap_does_not_roll_back() {
        let host = FakeHost::new("r239");
        for f in HOST_DEPENDENT_CAPABILITIES {
            host.features.borrow_mut().insert(f.to_string());
        }
        let plan = Plan { capabilities_added: vec!["c".into(), "job-external".into()], ..plan(Mode::Full, MigrationClass::RollbackSafe, PackageRun::After) };
        let mut run = new_run(RunKind::Apply, "r239", &plan);
        host.features.borrow_mut().insert("c".into());
        // The swap restarts the Job Service and it does not come back.
        host.fail_features_after_swap(HOST_DEPENDENT_CAPABILITIES);
        apply(&mut run, &plan, &host);
        assert_eq!(run.phase, Phase::Committed, "{:?}", run.error);
        assert!(!host.features.borrow().contains("job-external"), "the Job Service stayed down");
        // A cutex-built capability lost still fails.
        let before = Snapshot { features: host.features.borrow().clone(), ..host.snapshot().unwrap() };
        host.features.borrow_mut().remove("a");
        assert!(verify(&plan, &before, &host, &BTreeSet::new()).unwrap_err().contains("capabilities lost"));
    }

    #[test]
    fn verify_failure_rolls_back_binary_selection_and_new_stores() {
        let host = FakeHost::new("r239");
        host.health_errors.borrow_mut().clear();
        let plan = plan(Mode::Full, MigrationClass::RollbackSafe, PackageRun::After);
        let mut run = new_run(RunKind::Apply, "r239", &plan);
        // `c` never appears: verify fails.
        apply(&mut run, &plan, &host);
        assert_eq!(run.phase, Phase::RolledBack, "{:?}", run.needs_owner);
        assert_eq!(run.error.as_ref().unwrap().code, "verify_failed");
        assert!(run.error.as_ref().unwrap().message.contains("capabilities missing"));
        let steps = host.steps();
        let tail: Vec<&str> = steps.iter().skip_while(|s| *s != "rollback_prep").map(String::as_str).collect();
        assert_eq!(tail, vec!["rollback_prep", "stop", "activate_old", "restore_selection", "move_aside", "start", "wait_ready", "snapshot"]);
        assert_eq!(*host.running.borrow(), "r239");
        assert!(run.displaced.contains(&"runtime/new-store".to_string()));
    }

    #[test]
    fn verify_waits_long_enough_for_runtimes_to_come_back() {
        assert_eq!(online_converge(0), Duration::from_secs(180));
        assert_eq!(online_converge(22), Duration::from_secs(192));
        assert_eq!(online_converge(100), Duration::from_secs(660));
        assert_eq!(online_converge(usize::MAX), Duration::from_secs(900));
        let plan = Plan { capabilities_added: Vec::new(), capabilities_removed: Vec::new(), ..plan(Mode::Reload, MigrationClass::ReloadSafe, PackageRun::After) };
        let expect: BTreeSet<String> = ["cutex.x", "cutex.y"].iter().map(|s| s.to_string()).collect();
        for (back_at, ok) in [(Some(120), true), (None, false)] {
            let host = FakeHost::new("r240");
            let before = host.snapshot().unwrap();
            host.online.borrow_mut().remove("cutex.y");
            *host.online_late.borrow_mut() = back_at.map(|s| (Duration::from_secs(s), "cutex.y".to_string()));
            let result = verify(&plan, &before, &host, &expect);
            assert_eq!(result.is_ok(), ok, "{result:?}");
            if !ok {
                assert!(result.unwrap_err().contains("not back online"));
                assert!(host.clock.get() >= Duration::from_secs(180) && host.clock.get() < Duration::from_secs(190));
            }
        }
    }

    #[test]
    fn start_failure_rolls_back_and_new_health_errors_fail_verify() {
        let host = FakeHost::new("r239");
        host.fail.borrow_mut().insert("wait_ready");
        let plan = Plan { migrations: Vec::new(), ..plan(Mode::Reload, MigrationClass::ReloadSafe, PackageRun::After) };
        let mut run = new_run(RunKind::Apply, "r239", &plan);
        apply(&mut run, &plan, &host);
        // wait_ready also fails for the old release: needs-owner, no third attempt.
        assert_eq!(run.phase, Phase::NeedsOwner);
        assert!(run.needs_owner.as_ref().unwrap().commands.iter().any(|c| c.contains("rollback")));
        assert_eq!(host.steps().iter().filter(|s| *s == "activate_old").count(), 1);
    }

    #[test]
    fn one_way_failure_needs_the_owner() {
        let host = FakeHost::new("r239");
        let plan = plan(Mode::Full, MigrationClass::OneWay, PackageRun::After);
        let mut run = new_run(RunKind::Apply, "r239", &plan);
        apply(&mut run, &plan, &host);
        assert_eq!(run.phase, Phase::NeedsOwner);
        assert!(!host.steps().iter().any(|s| s == "activate_old"));
        assert!(run.needs_owner.unwrap().reason.contains("one-way"));
    }

    #[test]
    fn rollback_hazards_stop_before_switching() {
        let host = FakeHost::new("r239");
        host.hazards.borrow_mut().push("2 non-owner tokens would stop working".into());
        let plan = plan(Mode::Full, MigrationClass::RollbackSafe, PackageRun::After);
        let mut run = new_run(RunKind::Apply, "r239", &plan);
        apply(&mut run, &plan, &host);
        assert_eq!(run.phase, Phase::NeedsOwner);
        assert!(run.needs_owner.unwrap().reason.contains("non-owner tokens"));
        assert!(!host.steps().iter().any(|s| s == "activate_old"));
    }

    #[test]
    fn stop_failure_changes_nothing() {
        let host = FakeHost::new("r239");
        host.fail.borrow_mut().insert("stop");
        let run = go(&host, &plan(Mode::Reload, MigrationClass::ReloadSafe, PackageRun::After));
        assert_eq!(run.phase, Phase::Failed);
        assert!(!host.steps().iter().any(|s| s.starts_with("activate")));
    }

    #[test]
    fn owner_rollback() {
        let host = FakeHost::new("r240");
        let plan = Plan {
            to_release: "r239".into(),
            to_commit: None,
            selection_backup: Some(PathBuf::from("/x/backup")),
            ..plan(Mode::Full, MigrationClass::RollbackSafe, PackageRun::After)
        };
        let mut run = new_run(RunKind::Rollback, "r240", &plan);
        rollback(&mut run, &plan, &host);
        assert_eq!(run.phase, Phase::Committed, "{:?}", run.needs_owner);
        assert_eq!(*host.running.borrow(), "r239");
        assert!(host.steps().contains(&"restore_selection".to_string()));
        let host = FakeHost::new("r240");
        host.hazards.borrow_mut().push("loses data".into());
        let mut run = new_run(RunKind::Rollback, "r240", &plan);
        rollback(&mut run, &plan, &host);
        assert_eq!(run.phase, Phase::Failed);
        assert_eq!(run.error.unwrap().code, "rollback_hazard");
    }
}
