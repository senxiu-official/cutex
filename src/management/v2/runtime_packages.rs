//! Runtime package upgrade of explicit-launch (pinned) agents, owned by the
//! daemon (root credential only; capability `runtime-package-upgrade`).
//!
//! - `GET  /v2/runtime-packages` → `{hostId, selected, selectedError, agents,
//!   activeRun, lastRun, revertable}` (`revertable`: `{runId, agentIds}` of
//!   the newest upgrade run while it has agents left to revert, else null):
//!   every enabled, non-retired, persistent, local,
//!   host-backend explicit-launch agent whose pinned bundle differs from the
//!   selected deployment (`LocalDeployment::selected_package_differs`).
//! - `POST /v2/runtime-packages/runs` `{agentIds: [..]}` | `{all: true}` →
//!   202 `{run}`; 409 `run_in_progress` / `nothing_to_upgrade` /
//!   `selected_unavailable`.
//! - `GET  /v2/runtime-packages/runs?limit=N` → `{runs}` newest first.
//! - `GET  /v2/runtime-packages/runs/{runId}` → `{run}`; 404 `run_not_found`.
//! - `POST /v2/runtime-packages/runs/{runId}/revert` `{}` | `{agentIds}` →
//!   202 `{run}` (kind `revert`) for the selected moved agents that no
//!   earlier revert of this run moved back; 409 `run_in_progress` /
//!   `already_reverted` (all of them were) / `nothing_to_revert`.
//!
//! One run at a time (upgrade or revert), processed sequentially by a daemon
//! thread: busy agents are skipped, online idle agents go offline, are
//! reviewed / applied (`review_runtime_package_upgrade` /
//! `apply_runtime_package_upgrade`) or reverted
//! (`revert_runtime_package_upgrade`, under the same session-store lock as
//! apply, so no service has to be stopped) through the explicit-launch
//! action, and come back online. Run records live in
//! `<cutex home>/runtime/package-upgrade-runs/<runId>.json` (private) and are
//! rewritten after every change and pruned to the newest 20 (older upgrade
//! runs that can still be reverted are kept up to 90 days). Every change is
//! published on the host event stream as `cutex/runtimePackages/runUpdated
//! {runId, kind, status, revertOf, startedAt, finishedAt, currentAgentId,
//! counts, item?}` (only the changed item; clients read the full run with
//! `GET /v2/runtime-packages/runs/{runId}`). A record left `running` by an
//! earlier daemon process becomes `interrupted` the first time it is read.

use std::collections::{HashMap, HashSet};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::management::server::ManagementRequestContext;

pub const PATH: &str = "/v2/runtime-packages";
pub const RUN_UPDATED_EVENT: &str = "cutex/runtimePackages/runUpdated";
const RUNS_DIR: &str = "package-upgrade-runs";
const OFFLINE_WAIT: Duration = Duration::from_secs(90);
const ONLINE_WAIT: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_secs(1);
/// Review right after offline may still see the closing runtime's claim.
const REVIEW_CLAIM_RETRIES: usize = 5;
const MAX_REASON_CHARS: usize = 3000;
const DEFAULT_RUN_LIMIT: usize = 20;
const MAX_RUN_LIMIT: usize = 100;
const MAX_AGENT_IDS: usize = 1000;

// ---------------------------------------------------------------------------
// Shapes

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageSummary {
    pub manifest: Option<String>,
    pub native_path: Option<String>,
    pub native_version: Option<String>,
    pub native_release: Option<String>,
    pub facade_path: Option<String>,
    pub facade_release: Option<String>,
    pub readable: bool,
    /// Pinned package files that no longer exist (a deleted release
    /// directory): upgrading such an agent is a repair.
    #[serde(default)]
    pub missing: Vec<String>,
}

impl PackageSummary {
    /// Summary of a bundle manifest; `readable: false` when it is missing or
    /// not a bundle.
    pub fn from_manifest(
        manifest: &Path,
        versions: Option<&crate::launch::version_registry::NativeVersionRegistry>,
    ) -> Self {
        let mut summary = Self {
            // Display form: no Windows verbatim prefix (`\\?\`).
            manifest: Some(crate::agent_management::strip_verbatim_prefix(&manifest.to_string_lossy())),
            ..Self::default()
        };
        let Some(bundle) = std::fs::read(manifest)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        else {
            return summary;
        };
        let text = |pointer: &str| bundle.pointer(pointer).and_then(Value::as_str).map(str::to_string);
        let path = |pointer: &str| text(pointer).map(|path| crate::agent_management::strip_verbatim_prefix(&path));
        summary.native_path = path("/executable/path");
        summary.facade_path = path("/facade/path");
        summary.native_version = text("/executable/sha256")
            .and_then(|sha| versions.and_then(|registry| registry.runtime_version(&sha)).map(str::to_string));
        summary.native_release = native_release(
            summary.native_version.as_deref(),
            summary.native_path.as_deref(),
        );
        summary.facade_release = summary.facade_path.as_deref().and_then(release_dir_label);
        summary.readable = summary.native_path.is_some() && summary.facade_path.is_some();
        for pointer in ["/executable/path", "/cli/path", "/code_mode_host/path", "/facade/path", "/schema/path"] {
            if let Some(raw) = text(pointer) {
                if !Path::new(&raw).exists() {
                    summary.missing.push(crate::agent_management::strip_verbatim_prefix(&raw));
                }
            }
        }
        summary
    }
}

/// `r\d+` at the start of `name` (`"r209-desktop-20261008"` → `"r209"`).
fn release_prefix(name: &str) -> Option<String> {
    let rest = name.strip_prefix('r')?;
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    (digits > 0).then(|| format!("r{}", &rest[..digits]))
}

/// The nearest ancestor directory of `path` whose name starts with `r\d+`.
/// Both `/` and `\` separate components (records may come from either OS).
pub fn release_dir_label(path: &str) -> Option<String> {
    let mut components = path.split(['/', '\\']).filter(|part| !part.is_empty()).collect::<Vec<_>>();
    components.pop(); // the file itself
    components.into_iter().rev().find_map(release_prefix)
}

/// `"0.160.0-r50"` → `"r50"`; otherwise the path's nearest `r\d+` directory.
pub fn native_release(version: Option<&str>, path: Option<&str>) -> Option<String> {
    version
        .and_then(|version| version.rsplit_once('-'))
        .and_then(|(_, suffix)| release_prefix(suffix).filter(|label| label.len() == suffix.len()))
        .or_else(|| path.and_then(release_dir_label))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemState {
    Pending,
    Running,
    Moved,
    Skipped,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Step {
    Offline,
    /// Clearing a runtime claim an offline agent kept from an exited runtime.
    Reconcile,
    Review,
    Apply,
    Revert,
    Online,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunKind {
    Upgrade,
    Revert,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunState {
    Running,
    Completed,
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunItem {
    pub cutex_session_id: String,
    pub name: String,
    pub state: ItemState,
    pub step: Option<Step>,
    pub code: Option<String>,
    pub reason: Option<String>,
    pub was_online: Option<bool>,
    pub back_online: Option<bool>,
    pub previous_runtime_version: Option<String>,
    pub target_runtime_version: Option<String>,
    pub previous_facade_release: Option<String>,
    pub target_facade_release: Option<String>,
    pub action_id: Option<String>,
    pub committed_at: Option<String>,
    pub previous_contract: Option<Value>,
    pub target_contract: Option<Value>,
    /// The pinned package was missing (deleted release): the upgrade was a
    /// repair onto the selected package and cannot be reverted.
    #[serde(default)]
    pub repair: bool,
}

impl RunItem {
    fn pending(cutex_session_id: String, name: String) -> Self {
        Self {
            cutex_session_id,
            name,
            state: ItemState::Pending,
            step: None,
            code: None,
            reason: None,
            was_online: None,
            back_online: None,
            previous_runtime_version: None,
            target_runtime_version: None,
            previous_facade_release: None,
            target_facade_release: None,
            action_id: None,
            committed_at: None,
            previous_contract: None,
            target_contract: None,
            repair: false,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    pub total: usize,
    /// Not finished yet (pending or running).
    pub pending: usize,
    pub moved: usize,
    pub skipped: usize,
    pub failed: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub run_id: String,
    pub kind: RunKind,
    pub revert_of: Option<String>,
    pub reverted_by: Vec<String>,
    pub state: RunState,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub selected: PackageSummary,
    pub counts: Counts,
    pub current_agent_id: Option<String>,
    pub items: Vec<RunItem>,
}

impl Run {
    fn new(kind: RunKind, revert_of: Option<String>, selected: PackageSummary, items: Vec<RunItem>) -> Self {
        let mut run = Self {
            run_id: format!("rpu-{}", uuid::Uuid::new_v4()),
            kind,
            revert_of,
            reverted_by: Vec::new(),
            state: RunState::Running,
            started_at: now(),
            finished_at: None,
            selected,
            counts: Counts::default(),
            current_agent_id: None,
            items,
        };
        run.refresh_counts();
        run
    }

    fn refresh_counts(&mut self) {
        let count = |state: ItemState| self.items.iter().filter(|item| item.state == state).count();
        self.counts = Counts {
            total: self.items.len(),
            pending: count(ItemState::Pending) + count(ItemState::Running),
            moved: count(ItemState::Moved),
            skipped: count(ItemState::Skipped),
            failed: count(ItemState::Failed),
        };
    }

    /// A record this process does not own was left `running` by an earlier
    /// daemon: the in-flight item fails `interrupted` (or stays moved when its
    /// contract was already committed), pending items stay pending.
    fn mark_interrupted(&mut self, at: &str) {
        self.state = RunState::Interrupted;
        self.finished_at = Some(at.to_string());
        self.current_agent_id = None;
        for item in self.items.iter_mut().filter(|item| item.state == ItemState::Running) {
            item.step = None;
            item.code = Some("interrupted".into());
            if item.committed_at.is_some() {
                item.state = ItemState::Moved;
                if item.was_online == Some(true) {
                    item.back_online = Some(false);
                }
                item.reason = Some("the daemon restarted before the agent was back online".into());
            } else {
                item.state = ItemState::Failed;
                item.reason = Some("the daemon restarted while this agent was being processed".into());
            }
        }
        self.refresh_counts();
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn clip(reason: impl Into<String>) -> String {
    let reason = reason.into();
    if reason.chars().count() <= MAX_REASON_CHARS {
        reason
    } else {
        reason.chars().take(MAX_REASON_CHARS).collect()
    }
}

// ---------------------------------------------------------------------------
// Per-item state machine (pure over `RunOps`)

/// What the runner needs to know about one agent right now.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentSnapshot {
    pub name: String,
    /// Enabled, non-retired, persistent, local, host-backend explicit launch.
    pub eligible: bool,
    /// Pinned bundle differs from the selected deployment.
    pub outdated: bool,
    pub contract: Option<Value>,
    pub contract_version: u32,
    pub current: PackageSummary,
    pub runtime_status: String,
    pub active_turn_id: Option<String>,
    pub thread_status: Option<String>,
    /// The record still carries a runtime claim (pid, binding, runtime agent,
    /// launch claim or pending launch).
    pub has_claim: bool,
}

impl AgentSnapshot {
    fn online(&self) -> bool {
        self.runtime_status != "offline"
    }

    pub fn busy_reason(&self) -> Option<&'static str> {
        if self.active_turn_id.is_some() {
            return Some("activeTurn");
        }
        let idle = matches!(self.thread_status.as_deref(), None | Some("idle") | Some("notLoaded"));
        (self.online() && !idle).then_some("threadActive")
    }
}

/// A file a bundle pins: path + sha256.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedFile {
    pub path: String,
    pub sha256: String,
}

/// The selected deployment captured at run start (the upgrade target).
#[derive(Clone, Debug, PartialEq)]
pub struct SelectedSnapshot {
    pub manifest: PathBuf,
    pub sha256: String,
    pub native: PinnedFile,
    pub facade: PinnedFile,
    pub summary: PackageSummary,
}

/// The parts of a `RuntimePackageUpgradeReview` the runner checks; `review`
/// is passed back to apply unchanged.
#[derive(Clone, Debug, PartialEq)]
pub struct ReviewFacts {
    pub review: Value,
    pub selected_manifest: PathBuf,
    pub selected_manifest_sha256: String,
    pub target_native: Option<PinnedFile>,
    pub target_facade: Option<PinnedFile>,
    pub current_contract: Value,
    pub target_contract: Value,
    pub current_runtime_version: Option<String>,
    pub target_runtime_version: Option<String>,
    /// Missing pinned files when the review is a repair.
    pub repair: Option<Vec<String>>,
}

/// Everything a run does to the outside world (injectable for tests).
pub trait RunOps {
    fn agent(&self, id: &str) -> Result<Option<AgentSnapshot>, String>;
    fn offline(&self, id: &str, request_id: &str) -> Result<(), String>;
    fn online(&self, id: &str, request_id: &str) -> Result<(), String>;
    /// Clear a stale runtime claim of an offline agent (only proven-dead
    /// processes); returns what was cleared.
    fn reconcile(&self, id: &str) -> Result<String, String>;
    fn review(&self, id: &str) -> Result<ReviewFacts, String>;
    /// Returns the receipt's `committed_at`.
    fn apply(&self, action_id: &str, review: &Value) -> Result<Option<String>, String>;
    /// Returns the receipt's `committed_at`.
    fn revert(&self, id: &str, expected: &Value, restore: &Value) -> Result<Option<String>, String>;
    fn sleep(&self, duration: Duration);
    /// Persist + publish the run after a change.
    fn record(&self, run: &Run);
}

fn update(run: &mut Run, index: usize, ops: &dyn RunOps, change: impl FnOnce(&mut RunItem)) {
    change(&mut run.items[index]);
    run.refresh_counts();
    ops.record(run);
}

fn finish_item(run: &mut Run, index: usize, ops: &dyn RunOps, state: ItemState, code: Option<&str>, reason: Option<String>) {
    update(run, index, ops, |item| {
        item.state = state;
        item.step = None;
        if code.is_some() {
            item.code = code.map(str::to_string);
        }
        if reason.is_some() {
            item.reason = reason.map(clip);
        }
    });
}

/// Poll the agent's runtime status until `want` holds (true) or the timeout.
fn wait_for(ops: &dyn RunOps, id: &str, timeout: Duration, want: impl Fn(&str) -> bool) -> bool {
    let rounds = (timeout.as_millis() / POLL.as_millis().max(1)).max(1);
    for round in 0..=rounds {
        if let Ok(Some(agent)) = ops.agent(id) {
            if want(&agent.runtime_status) {
                return true;
            }
        }
        if round < rounds {
            ops.sleep(POLL);
        }
    }
    false
}

/// Take an agent offline when it is not; `Err(reason)` on failure.
fn take_offline(run: &mut Run, index: usize, ops: &dyn RunOps, agent: &AgentSnapshot) -> Result<(), String> {
    if !agent.online() {
        return Ok(());
    }
    let id = run.items[index].cutex_session_id.clone();
    update(run, index, ops, |item| item.step = Some(Step::Offline));
    ops.offline(&id, &format!("{}-{index}:offline", run.run_id))?;
    if wait_for(ops, &id, OFFLINE_WAIT, |status| status != "online") {
        Ok(())
    } else {
        Err(format!("did not go offline within {} s", OFFLINE_WAIT.as_secs()))
    }
}

/// Bring a previously online agent back; records `backOnline`.
fn bring_back(run: &mut Run, index: usize, ops: &dyn RunOps, label: &str) -> Result<(), String> {
    if run.items[index].was_online != Some(true) {
        return Ok(());
    }
    let id = run.items[index].cutex_session_id.clone();
    update(run, index, ops, |item| item.step = Some(Step::Online));
    let outcome = ops
        .online(&id, &format!("{}-{index}:{label}", run.run_id))
        .and_then(|()| {
            wait_for(ops, &id, ONLINE_WAIT, |status| status == "online")
                .then_some(())
                .ok_or_else(|| format!("did not come back online within {} s", ONLINE_WAIT.as_secs()))
        });
    let back = outcome.is_ok();
    update(run, index, ops, |item| item.back_online = Some(back));
    outcome
}

/// Fail an item after it may have gone offline: bring it back first.
fn fail_and_restore(run: &mut Run, index: usize, ops: &dyn RunOps, code: &str, reason: String) {
    let reason = match bring_back(run, index, ops, "online-rollback") {
        Ok(()) => reason,
        Err(online) => format!("{reason}; back online failed: {online}"),
    };
    finish_item(run, index, ops, ItemState::Failed, Some(code), Some(reason));
}

fn start_item(run: &mut Run, index: usize, ops: &dyn RunOps) -> String {
    let id = run.items[index].cutex_session_id.clone();
    run.current_agent_id = Some(id.clone());
    update(run, index, ops, |item| item.state = ItemState::Running);
    id
}

/// Common entry checks; `Some(agent)` when the item may proceed.
fn admit(run: &mut Run, index: usize, ops: &dyn RunOps, id: &str) -> Option<AgentSnapshot> {
    match ops.agent(id) {
        Ok(Some(agent)) => {
            if !agent.name.is_empty() {
                run.items[index].name = agent.name.clone();
            }
            Some(agent)
        }
        Ok(None) => {
            finish_item(run, index, ops, ItemState::Skipped, Some("notFound"), Some("the agent no longer exists".into()));
            None
        }
        Err(error) => {
            finish_item(run, index, ops, ItemState::Skipped, Some("notFound"), Some(error));
            None
        }
    }
}

fn skip_busy(run: &mut Run, index: usize, ops: &dyn RunOps, agent: &AgentSnapshot) -> bool {
    let Some(reason) = agent.busy_reason() else {
        return false;
    };
    let detail = match (reason, agent.active_turn_id.as_deref()) {
        ("activeTurn", Some(turn)) => format!("a turn is running ({turn}); retry later"),
        _ => format!(
            "the thread is not idle ({}); retry later",
            agent.thread_status.as_deref().unwrap_or("unknown")
        ),
    };
    finish_item(run, index, ops, ItemState::Skipped, Some("busy"), Some(detail));
    true
}

fn review_with_retry(ops: &dyn RunOps, id: &str, after_offline: bool) -> Result<ReviewFacts, String> {
    let mut attempt = 0;
    loop {
        match ops.review(id) {
            Err(error) if after_offline && error.contains("offline/no claim") && attempt < REVIEW_CLAIM_RETRIES => {
                attempt += 1;
                ops.sleep(POLL);
            }
            outcome => return outcome,
        }
    }
}

fn upgrade_item(run: &mut Run, index: usize, selected: &SelectedSnapshot, ops: &dyn RunOps) {
    let id = start_item(run, index, ops);
    let Some(agent) = admit(run, index, ops, &id) else { return };
    if !agent.eligible {
        let reason = "the agent is no longer an enabled local explicit-launch agent".to_string();
        return finish_item(run, index, ops, ItemState::Skipped, Some("notEligible"), Some(reason));
    }
    if !agent.outdated {
        return finish_item(run, index, ops, ItemState::Skipped, Some("alreadyCurrent"), None);
    }
    if skip_busy(run, index, ops, &agent) {
        return;
    }
    let was_online = agent.runtime_status == "online";
    update(run, index, ops, |item| {
        item.was_online = Some(was_online);
        item.previous_runtime_version = agent.current.native_version.clone();
        item.previous_facade_release = agent.current.facade_release.clone();
        item.target_runtime_version = selected.summary.native_version.clone();
        item.target_facade_release = selected.summary.facade_release.clone();
    });
    if let Err(reason) = take_offline(run, index, ops, &agent) {
        return fail_and_restore(run, index, ops, "offlineFailed", reason);
    }
    // An offline agent can still carry the claim of a runtime that exited
    // (review refuses "offline/no claim"): reconcile it like a start would.
    if !agent.online() && agent.has_claim {
        update(run, index, ops, |item| item.step = Some(Step::Reconcile));
        if let Err(reason) = ops.reconcile(&id) {
            return fail_and_restore(run, index, ops, "staleClaim", reason);
        }
    }
    update(run, index, ops, |item| item.step = Some(Step::Review));
    let facts = match review_with_retry(ops, &id, agent.online()) {
        Ok(facts) => facts,
        Err(reason) => return fail_and_restore(run, index, ops, "reviewRefused", reason),
    };
    if facts.selected_manifest != selected.manifest || facts.selected_manifest_sha256 != selected.sha256 {
        let reason = format!(
            "the selected runtime package changed during the run (review: {} {}, run: {} {})",
            facts.selected_manifest.display(),
            facts.selected_manifest_sha256,
            selected.manifest.display(),
            selected.sha256
        );
        return fail_and_restore(run, index, ops, "selectedChanged", reason);
    }
    if facts.target_native.as_ref() != Some(&selected.native) || facts.target_facade.as_ref() != Some(&selected.facade) {
        let path = |file: &Option<PinnedFile>| file.as_ref().map_or("<none>".to_string(), |file| file.path.clone());
        let reason = format!(
            "review target is native {} + facade {}, not the selected {} + {}",
            path(&facts.target_native),
            path(&facts.target_facade),
            selected.native.path,
            selected.facade.path
        );
        return fail_and_restore(run, index, ops, "targetMismatch", reason);
    }
    let action_id = format!("{}-{index}", run.run_id);
    update(run, index, ops, |item| {
        item.step = Some(Step::Apply);
        if facts.current_runtime_version.is_some() {
            item.previous_runtime_version = facts.current_runtime_version.clone();
        }
        if facts.target_runtime_version.is_some() {
            item.target_runtime_version = facts.target_runtime_version.clone();
        }
        item.action_id = Some(action_id.clone());
        if let Some(missing) = &facts.repair {
            item.repair = true;
            item.reason = Some(clip(format!(
                "repair: the pinned runtime package is missing ({}); moved to the selected package, nothing to revert to",
                missing.join(", ")
            )));
        }
    });
    let committed_at = match ops.apply(&action_id, &facts.review) {
        Ok(committed_at) => committed_at.unwrap_or_else(now),
        Err(reason) => return fail_and_restore(run, index, ops, "applyFailed", reason),
    };
    // Committed: keep the revert input even if the daemon stops before online.
    update(run, index, ops, |item| {
        item.committed_at = Some(committed_at);
        item.previous_contract = Some(facts.current_contract.clone());
        item.target_contract = Some(facts.target_contract.clone());
    });
    complete_moved(run, index, ops);
}

fn complete_moved(run: &mut Run, index: usize, ops: &dyn RunOps) {
    match bring_back(run, index, ops, "online") {
        Ok(()) => finish_item(run, index, ops, ItemState::Moved, None, None),
        Err(reason) => finish_item(run, index, ops, ItemState::Moved, Some("onlineFailed"), Some(reason)),
    }
}

/// Revert input of one moved upgrade item.
#[derive(Clone, Debug, PartialEq)]
pub struct RevertSource {
    /// The contract the upgrade replaced (restored by the revert).
    pub previous_contract: Value,
    /// The contract the upgrade wrote (must still be current).
    pub target_contract: Value,
}

fn revert_item(run: &mut Run, index: usize, source: &RevertSource, ops: &dyn RunOps) {
    let id = start_item(run, index, ops);
    let Some(agent) = admit(run, index, ops, &id) else { return };
    if agent.contract.as_ref() != Some(&source.target_contract) {
        let reason = "the agent's launch contract changed after the upgrade".to_string();
        return finish_item(run, index, ops, ItemState::Skipped, Some("contractChanged"), Some(reason));
    }
    if skip_busy(run, index, ops, &agent) {
        return;
    }
    let was_online = agent.runtime_status == "online";
    update(run, index, ops, |item| item.was_online = Some(was_online));
    if let Err(reason) = take_offline(run, index, ops, &agent) {
        return fail_and_restore(run, index, ops, "offlineFailed", reason);
    }
    update(run, index, ops, |item| item.step = Some(Step::Revert));
    let committed_at = match ops.revert(&id, &source.target_contract, &source.previous_contract) {
        Ok(committed_at) => committed_at.unwrap_or_else(now),
        Err(reason) if reason.contains("contract_changed") => {
            let _ = bring_back(run, index, ops, "online-rollback");
            return finish_item(run, index, ops, ItemState::Skipped, Some("contractChanged"), Some(reason));
        }
        Err(reason) => return fail_and_restore(run, index, ops, "revertFailed", reason),
    };
    update(run, index, ops, |item| {
        item.committed_at = Some(committed_at);
        item.previous_contract = Some(source.target_contract.clone());
        item.target_contract = Some(source.previous_contract.clone());
    });
    complete_moved(run, index, ops);
}

fn finish_run(run: &mut Run, ops: &dyn RunOps) {
    run.current_agent_id = None;
    run.state = RunState::Completed;
    run.finished_at = Some(now());
    run.refresh_counts();
    ops.record(run);
}

pub fn run_upgrade(run: &mut Run, selected: &SelectedSnapshot, ops: &dyn RunOps) {
    for index in 0..run.items.len() {
        upgrade_item(run, index, selected, ops);
    }
    finish_run(run, ops);
}

pub fn run_revert(run: &mut Run, sources: &HashMap<String, RevertSource>, ops: &dyn RunOps) {
    for index in 0..run.items.len() {
        match sources.get(&run.items[index].cutex_session_id) {
            Some(source) => revert_item(run, index, source, ops),
            None => {
                start_item(run, index, ops);
                let reason = "the upgrade run has no revert input for this agent".to_string();
                finish_item(run, index, ops, ItemState::Skipped, Some("notEligible"), Some(reason));
            }
        }
    }
    finish_run(run, ops);
}

// ---------------------------------------------------------------------------
// Run records + one-run-at-a-time

pub struct Coordinator {
    dir: PathBuf,
    active: Mutex<Option<Run>>,
    publish: fn(&Value),
}

/// `cutex/runtimePackages/runUpdated` params: the run header (no items) and
/// at most the one item that changed. Clients read the full run with
/// `GET /v2/runtime-packages/runs/{runId}` (on start, finish or a gap).
pub fn run_event(run: &Run, item: Option<&RunItem>) -> Value {
    let mut event = json!({
        "runId": run.run_id,
        "kind": run.kind,
        "status": run.state,
        "revertOf": run.revert_of,
        "startedAt": run.started_at,
        "finishedAt": run.finished_at,
        "currentAgentId": run.current_agent_id,
        "counts": run.counts,
    });
    if let Some(item) = item {
        event["item"] = json!(item);
    }
    event
}

/// Items of `run` that differ from `before` (same run), in run order.
fn changed_items<'a>(before: Option<&Run>, run: &'a Run) -> Vec<&'a RunItem> {
    let Some(before) = before.filter(|before| before.run_id == run.run_id) else {
        return Vec::new();
    };
    run.items
        .iter()
        .filter(|item| {
            before
                .items
                .iter()
                .find(|old| old.cutex_session_id == item.cutex_session_id)
                .is_none_or(|old| old != *item)
        })
        .collect()
}

/// Records kept by [`Coordinator::prune`]: the newest this many runs ...
pub const KEEP_RUNS: usize = 20;
/// ... plus older upgrade runs that can still be reverted, up to this age.
pub const KEEP_REVERTABLE_DAYS: i64 = 90;

/// Agents of upgrade run `source` that a revert run of it (among `runs`)
/// already moved back.
pub fn reverted_agents(source: &Run, runs: &[Run]) -> HashSet<String> {
    runs.iter()
        .filter(|run| run.kind == RunKind::Revert && run.revert_of.as_deref() == Some(source.run_id.as_str()))
        .flat_map(|run| run.items.iter())
        .filter(|item| item.state == ItemState::Moved)
        .map(|item| item.cutex_session_id.clone())
        .collect()
}

/// A finished upgrade run, at most [`KEEP_REVERTABLE_DAYS`] old, with moved
/// items no revert run has moved back yet (`runs`: every record).
fn revertable(run: &Run, runs: &[Run], now: chrono::DateTime<chrono::Utc>) -> bool {
    run.kind == RunKind::Upgrade
        && run.state != RunState::Running
        && !revert_items(run, &reverted_agents(run, runs), &Selection::All).items.is_empty()
        && chrono::DateTime::parse_from_rfc3339(&run.started_at)
            .is_ok_and(|started| now.signed_duration_since(started) <= chrono::Duration::days(KEEP_REVERTABLE_DAYS))
}

impl Coordinator {
    pub fn new(dir: PathBuf, publish: fn(&Value)) -> Self {
        Self { dir, active: Mutex::new(None), publish }
    }

    fn path(&self, run_id: &str) -> PathBuf {
        self.dir.join(format!("{run_id}.json"))
    }

    fn persist(&self, run: &Run) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))?;
        }
        crate::config::atomic::write_private_pretty_json_atomic(
            &self.path(&run.run_id),
            run,
            "runtime package run",
        )
    }

    /// Persist and publish a change (best effort: the record is the source
    /// of truth for later reads, the event only tells clients to look).
    /// One event per changed item of the active run, else a header event.
    /// A call that changes nothing (e.g. `finish` after the run's own final
    /// record) writes and publishes nothing.
    pub fn update(&self, run: &Run) {
        self.update_with(run, false);
    }

    fn update_with(&self, run: &Run, always: bool) {
        let mut before = None;
        if let Ok(mut active) = self.active.lock() {
            if active.as_ref().is_some_and(|current| current.run_id == run.run_id) {
                before = active.replace(run.clone());
            }
        }
        if !always && before.as_ref() == Some(run) {
            return;
        }
        if let Err(error) = self.persist(run) {
            eprintln!("runtime packages: could not write run {}: {error:#}", run.run_id);
        }
        let changed = changed_items(before.as_ref(), run);
        if changed.is_empty() {
            (self.publish)(&run_event(run, None));
        }
        for item in changed {
            (self.publish)(&run_event(run, Some(item)));
        }
    }

    /// Claim the single run slot; `Err(active run ID)` when one is running.
    pub fn try_begin(&self, run: &Run) -> Result<(), String> {
        let mut active = self.active.lock().map_err(|_| String::new())?;
        if let Some(current) = active.as_ref().filter(|current| current.state == RunState::Running) {
            return Err(current.run_id.clone());
        }
        *active = Some(run.clone());
        drop(active);
        // Same run as the active copy: no item changed, a header event.
        self.update_with(run, true);
        Ok(())
    }

    pub fn finish(&self, run: &Run) {
        self.update(run);
        if let Ok(mut active) = self.active.lock() {
            if active.as_ref().is_some_and(|current| current.run_id == run.run_id) {
                *active = None;
            }
        }
        self.prune();
    }

    /// Delete finished records beyond the newest [`KEEP_RUNS`], except
    /// upgrade runs that are still revertable (moved items, no revert yet,
    /// at most [`KEEP_REVERTABLE_DAYS`] old).
    pub fn prune(&self) {
        let now = chrono::Utc::now();
        let runs = self.runs();
        for run in runs.iter().skip(KEEP_RUNS) {
            if run.state == RunState::Running || revertable(run, &runs, now) {
                continue;
            }
            if let Err(error) = std::fs::remove_file(self.path(&run.run_id)) {
                eprintln!("runtime packages: could not prune run {}: {error}", run.run_id);
            }
        }
    }

    pub fn active(&self) -> Option<Run> {
        self.active.lock().ok()?.clone().filter(|run| run.state == RunState::Running)
    }

    /// Every run record, newest first; stale `running` records (not owned by
    /// this process) are marked `interrupted` and rewritten.
    pub fn runs(&self) -> Vec<Run> {
        let active = self.active.lock().ok().and_then(|active| active.clone());
        let mut runs = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return active.into_iter().collect();
        };
        let mut seen_active = false;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Some(mut run) = std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Run>(&bytes).ok())
            else {
                continue;
            };
            if let Some(active) = active.as_ref().filter(|active| active.run_id == run.run_id) {
                seen_active = true;
                runs.push(active.clone());
                continue;
            }
            if run.state == RunState::Running {
                run.mark_interrupted(&now());
                self.update(&run);
            }
            runs.push(run);
        }
        if !seen_active {
            runs.extend(active);
        }
        runs.sort_by(|left, right| {
            right.started_at.cmp(&left.started_at).then_with(|| right.run_id.cmp(&left.run_id))
        });
        runs
    }

    pub fn get(&self, run_id: &str) -> Option<Run> {
        if let Some(active) = self.active.lock().ok().and_then(|active| active.clone()) {
            if active.run_id == run_id {
                return Some(active);
            }
        }
        let mut run: Run = serde_json::from_slice(&std::fs::read(self.path(run_id)).ok()?).ok()?;
        if run.state == RunState::Running {
            run.mark_interrupted(&now());
            self.update(&run);
        }
        Some(run)
    }
}

fn publish_event(params: &Value) {
    let Ok(repository) = super::repository::management_v2_repository() else {
        return;
    };
    if let Err(error) = super::server::append_cutex_event(
        repository,
        super::services::HOST_EVENT_SESSION_ID,
        super::model::EventCorrelation::default(),
        RUN_UPDATED_EVENT,
        params.clone(),
    ) {
        eprintln!("runtime packages: could not publish event: {error:#}");
    }
}

static COORDINATOR: OnceLock<Arc<Coordinator>> = OnceLock::new();

fn coordinator() -> anyhow::Result<Arc<Coordinator>> {
    if let Some(coordinator) = COORDINATOR.get() {
        return Ok(coordinator.clone());
    }
    let dir = crate::config::paths::runtime_dir()?.join(RUNS_DIR);
    Ok(COORDINATOR.get_or_init(|| Arc::new(Coordinator::new(dir, publish_event))).clone())
}

// ---------------------------------------------------------------------------
// Request parsing

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    Overview,
    StartRun,
    ListRuns,
    GetRun(String),
    RevertRun(String),
}

pub fn is_runtime_packages_path(path: &str) -> bool {
    path == PATH || path.starts_with("/v2/runtime-packages/")
}

fn valid_run_id(id: &str) -> bool {
    id.starts_with("rpu-")
        && id.len() <= 64
        && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// `Err(true)` = known path, wrong method (405); `Err(false)` = no route (404).
pub fn parse_route(method: &str, path: &str) -> Result<Route, bool> {
    let route = match path {
        PATH => (method == "GET").then_some(Route::Overview),
        "/v2/runtime-packages/runs" => match method {
            "GET" => Some(Route::ListRuns),
            "POST" => Some(Route::StartRun),
            _ => None,
        },
        _ => {
            let rest = path.strip_prefix("/v2/runtime-packages/runs/").ok_or(false)?;
            let (id, action) = rest.split_once('/').map_or((rest, None), |(id, action)| (id, Some(action)));
            if !valid_run_id(id) {
                return Err(false);
            }
            match (method, action) {
                ("GET", None) => Some(Route::GetRun(id.to_string())),
                ("POST", Some("revert")) => Some(Route::RevertRun(id.to_string())),
                (_, None) | (_, Some("revert")) => None,
                _ => return Err(false),
            }
        }
    };
    route.ok_or(true)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selection {
    All,
    Ids(Vec<String>),
}

fn body_object(body: &[u8], allow_empty: bool) -> Result<serde_json::Map<String, Value>, String> {
    if allow_empty && body.iter().all(u8::is_ascii_whitespace) {
        return Ok(serde_json::Map::new());
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(object)) => Ok(object),
        _ => Err("a JSON object body is required".into()),
    }
}

fn parse_agent_ids(value: &Value) -> Result<Vec<String>, String> {
    let ids = value.as_array().ok_or("agentIds must be an array of agent IDs")?;
    if ids.is_empty() || ids.len() > MAX_AGENT_IDS {
        return Err(format!("agentIds must hold 1..={MAX_AGENT_IDS} agent IDs"));
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for id in ids {
        let id = id
            .as_str()
            .map(str::trim)
            .filter(|id| !id.is_empty() && id.len() <= 256)
            .ok_or("agentIds must be non-empty strings")?;
        if seen.insert(id.to_string()) {
            out.push(id.to_string());
        }
    }
    Ok(out)
}

/// `{"agentIds": [...]}` or `{"all": true}`, exactly one; nothing else.
pub fn parse_start_body(body: &[u8]) -> Result<Selection, String> {
    let object = body_object(body, false)?;
    if let Some(key) = object.keys().find(|key| !matches!(key.as_str(), "agentIds" | "all")) {
        return Err(format!("unsupported field: {key}"));
    }
    match (object.get("agentIds"), object.get("all")) {
        (Some(ids), None) => parse_agent_ids(ids).map(Selection::Ids),
        (None, Some(Value::Bool(true))) => Ok(Selection::All),
        (None, Some(_)) => Err("all must be true".into()),
        _ => Err("exactly one of agentIds or all is required".into()),
    }
}

/// `{}` (every moved item) or `{"agentIds": [...]}`.
pub fn parse_revert_body(body: &[u8]) -> Result<Selection, String> {
    let object = body_object(body, true)?;
    if let Some(key) = object.keys().find(|key| key.as_str() != "agentIds") {
        return Err(format!("unsupported field: {key}"));
    }
    match object.get("agentIds") {
        None => Ok(Selection::All),
        Some(ids) => parse_agent_ids(ids).map(Selection::Ids),
    }
}

fn parse_limit(path: &str) -> Result<usize, String> {
    match crate::http::query::query_value(path, "limit") {
        None => Ok(DEFAULT_RUN_LIMIT),
        Some(raw) => match raw.parse::<usize>() {
            Ok(0) | Err(_) => Err("limit must be a positive integer".into()),
            Ok(limit) => Ok(limit.min(MAX_RUN_LIMIT)),
        },
    }
}

/// Pending items of a new upgrade run: the requested IDs that are outdated,
/// in request order (all: candidate order).
pub fn select_items(candidates: &[(String, AgentSnapshot)], selection: &Selection) -> Vec<RunItem> {
    match selection {
        Selection::All => candidates
            .iter()
            .map(|(id, agent)| RunItem::pending(id.clone(), agent.name.clone()))
            .collect(),
        Selection::Ids(ids) => ids
            .iter()
            .filter_map(|id| candidates.iter().find(|(candidate, _)| candidate == id))
            .map(|(id, agent)| RunItem::pending(id.clone(), agent.name.clone()))
            .collect(),
    }
}

/// What a revert run of `source` would do.
#[derive(Debug, Default)]
pub struct RevertPlan {
    pub items: Vec<RunItem>,
    pub sources: HashMap<String, RevertSource>,
    /// Selected moved agents left out because an earlier revert moved them back.
    pub already_reverted: usize,
}

/// Items + revert inputs of a revert run of `source`: its moved items the
/// selection names, minus the `reverted` agents.
pub fn revert_items(source: &Run, reverted: &HashSet<String>, selection: &Selection) -> RevertPlan {
    let mut plan = RevertPlan::default();
    let RevertPlan { items, sources, already_reverted } = &mut plan;
    let wanted = |id: &str| match selection {
        Selection::All => true,
        Selection::Ids(ids) => ids.iter().any(|wanted| wanted == id),
    };
    for item in &source.items {
        let (ItemState::Moved, Some(previous), Some(target)) =
            (item.state, item.previous_contract.as_ref(), item.target_contract.as_ref())
        else {
            continue;
        };
        // A repair replaced a package that no longer exists.
        if item.repair {
            continue;
        }
        if !wanted(&item.cutex_session_id) || sources.contains_key(&item.cutex_session_id) {
            continue;
        }
        if reverted.contains(&item.cutex_session_id) {
            *already_reverted += 1;
            continue;
        }
        let mut pending = RunItem::pending(item.cutex_session_id.clone(), item.name.clone());
        pending.previous_runtime_version = item.target_runtime_version.clone();
        pending.target_runtime_version = item.previous_runtime_version.clone();
        pending.previous_facade_release = item.target_facade_release.clone();
        pending.target_facade_release = item.previous_facade_release.clone();
        items.push(pending);
        sources.insert(
            item.cutex_session_id.clone(),
            RevertSource { previous_contract: previous.clone(), target_contract: target.clone() },
        );
    }
    plan
}

/// The newest finished upgrade run while it still has agents to move back:
/// `{runId, agentIds}` for the overview, else null.
fn revertable_overview(runs: &[Run]) -> Value {
    let Some(run) = runs.iter().find(|run| run.kind == RunKind::Upgrade && run.state != RunState::Running) else {
        return Value::Null;
    };
    if !revertable(run, runs, chrono::Utc::now()) {
        return Value::Null;
    }
    let plan = revert_items(run, &reverted_agents(run, runs), &Selection::All);
    json!({
        "runId": run.run_id,
        "agentIds": plan.items.iter().map(|item| item.cutex_session_id.clone()).collect::<Vec<_>>(),
    })
}

// ---------------------------------------------------------------------------
// Daemon side

fn eligible(record: &crate::session::model::CutexSessionRecord) -> bool {
    record.explicit_launch.as_ref().is_some_and(|contract| matches!(contract.version, 3 | 4))
        && record.agent_enabled
        && !record.is_retired()
        && record.registration_class == crate::agent_bus::model::AgentRegistrationClass::Persistent
        && record.runtime_backend == crate::session::model::CutexSessionRuntimeBackend::Host
        && crate::runtime::lifecycle::cutex_session_host_is_local(
            &record.host_id,
            &crate::platform::host::current_host_name(),
        )
}

fn agent_name(record: &crate::session::model::CutexSessionRecord) -> String {
    record
        .formal_agent_name
        .clone()
        .or_else(|| record.thread_name.clone())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| record.cutex_session_id.clone())
}

fn thread_status_type(session: Option<&Value>) -> Option<String> {
    let status = session?.pointer("/runtime/threadStatus")?;
    status
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| status.as_str())
        .map(str::to_string)
}

fn snapshot(
    record: &crate::session::model::CutexSessionRecord,
    session: Option<&Value>,
    versions: Option<&crate::launch::version_registry::NativeVersionRegistry>,
) -> AgentSnapshot {
    let contract = record.explicit_launch.as_ref();
    AgentSnapshot {
        name: agent_name(record),
        eligible: eligible(record),
        outdated: crate::launch::local_deployment::LocalDeployment::selected_package_differs(record),
        contract: contract.and_then(|contract| serde_json::to_value(contract).ok()),
        contract_version: contract.map_or(0, |contract| contract.version),
        current: contract
            .map(|contract| PackageSummary::from_manifest(&contract.bundle_manifest, versions))
            .unwrap_or_default(),
        runtime_status: session
            .and_then(|session| session.pointer("/runtime/status"))
            .and_then(Value::as_str)
            .unwrap_or("offline")
            .to_string(),
        active_turn_id: session
            .and_then(|session| session.pointer("/runtime/activeTurnId"))
            .and_then(Value::as_str)
            .map(str::to_string),
        thread_status: thread_status_type(session),
        has_claim: crate::session::archive::record_has_runtime_claim(record) || record.pending_launch_id.is_some(),
    }
}

fn outdated_agent_json(id: &str, agent: &AgentSnapshot) -> Value {
    let busy = agent.busy_reason();
    json!({
        "cutexSessionId": id,
        "name": agent.name,
        "contractVersion": agent.contract_version,
        "current": agent.current,
        "runtimeStatus": agent.runtime_status,
        "online": agent.online(),
        "busy": busy.is_some(),
        "busyReason": busy,
        "activeTurnId": agent.active_turn_id,
        "threadStatus": agent.thread_status,
        "repair": !agent.current.missing.is_empty(),
    })
}

/// Outdated candidates, sorted by name.
fn outdated_agents(context: ManagementRequestContext) -> anyhow::Result<Vec<(String, AgentSnapshot)>> {
    let repository = super::repository::management_v2_repository()?;
    let store = crate::session::store::load_cutex_session_store()?;
    let registry = (context.load_registry)()?;
    let list = super::session::session_list_resource_with_hidden(
        &registry,
        context.load_runtime_status,
        repository,
        true,
    )?;
    let sessions = list
        .get("sessions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|session| Some((session.get("cutexSessionId")?.as_str()?.to_string(), session)))
        .collect::<HashMap<_, _>>();
    let versions = crate::launch::version_registry::load_relaxed();
    let mut seen = HashSet::new();
    let mut agents = store
        .sessions
        .values()
        .filter(|record| eligible(record) && seen.insert(record.cutex_session_id.clone()))
        .filter(|record| crate::launch::local_deployment::LocalDeployment::selected_package_differs(record))
        .map(|record| {
            let session = sessions.get(&record.cutex_session_id).copied();
            (record.cutex_session_id.clone(), snapshot(record, session, versions.as_ref()))
        })
        .collect::<Vec<_>>();
    agents.sort_by(|left, right| left.1.name.cmp(&right.1.name).then_with(|| left.0.cmp(&right.0)));
    Ok(agents)
}

fn selected_snapshot() -> anyhow::Result<SelectedSnapshot> {
    use anyhow::Context;
    let deployment = crate::launch::local_deployment::LocalDeployment::selected()?
        .context("selected runtime deployment required")?;
    let manifest = deployment.bundle_manifest.canonicalize()?;
    let sha256 = crate::agent_management::file_sha256(&manifest)?;
    let bundle: crate::launch::stock::StockBundle = serde_json::from_slice(&std::fs::read(&manifest)?)
        .context("selected runtime package manifest missing or invalid")?;
    let versions = crate::launch::version_registry::load_relaxed();
    Ok(SelectedSnapshot {
        summary: PackageSummary::from_manifest(&deployment.bundle_manifest, versions.as_ref()),
        manifest,
        sha256: sha256.as_str().to_string(),
        native: pinned(&bundle.executable),
        facade: pinned(&bundle.facade),
    })
}

fn pinned(file: &crate::launch::stock::VerifiedFile) -> PinnedFile {
    PinnedFile {
        path: file.path.to_string_lossy().into_owned(),
        sha256: file.sha256.as_str().to_string(),
    }
}

/// The selected summary for the overview: `(summary, error)`.
fn selected_overview() -> (Option<PackageSummary>, Option<String>) {
    match crate::launch::local_deployment::LocalDeployment::selected() {
        Ok(Some(deployment)) => {
            let versions = crate::launch::version_registry::load_relaxed();
            (Some(PackageSummary::from_manifest(&deployment.bundle_manifest, versions.as_ref())), None)
        }
        Ok(None) => (None, Some("selected runtime deployment required".into())),
        Err(error) => (None, Some(format!("{error:#}"))),
    }
}

struct DaemonOps {
    context: ManagementRequestContext,
    coordinator: Arc<Coordinator>,
}

fn principal() -> crate::management::control_plane::HumanManagementPrincipal {
    crate::management::control_plane::HumanManagementPrincipal::authenticated()
}

fn cutex_error(error: super::user_input::UserInputExecutionError) -> String {
    format!("{}: {}", error.code, error.message)
}

impl DaemonOps {
    fn session(&self, id: &str) -> Result<Option<Value>, String> {
        let repository = super::repository::management_v2_repository().map_err(|e| format!("{e:#}"))?;
        let registry = (self.context.load_registry)().map_err(|e| format!("{e:#}"))?;
        super::session::session_resource_including_hidden(id, &registry, self.context.load_runtime_status, repository)
            .map_err(|e| format!("{e:#}"))
    }

    fn runtime_mutation(&self, id: &str, request_id: &str, online: bool) -> Result<(), String> {
        let repository = super::repository::management_v2_repository().map_err(|e| format!("{e:#}"))?;
        let session = self.session(id)?.ok_or("the agent no longer exists")?;
        let generation = session.pointer("/runtime/runtimeGeneration").and_then(Value::as_u64).unwrap_or(0);
        let reason = if online { "runtime package run (back online)" } else { "runtime package run" };
        let mut params = json!({ "expectedRuntimeGeneration": generation, "reason": reason });
        if online {
            params["openVisibleTerminal"] = json!(false);
        }
        let request = super::server::ValidatedCutexRequest {
            request_id: request_id.to_string(),
            method: if online { "cutex/runtime/online" } else { "cutex/runtime/offline" }.to_string(),
            params,
        };
        let owner = principal();
        let result = if online {
            let online_session = self.context.online_session;
            super::server::dispatch_runtime_mutation(
                repository,
                |id, _method, params| online_session(Some(&owner), id, params),
                id,
                &session,
                &request,
            )
        } else {
            super::server::dispatch_runtime_mutation(repository, self.context.mutate_session, id, &session, &request)
        };
        result.map(|_| ()).map_err(cutex_error)
    }

    fn explicit(&self, request: &crate::agent_management::ExplicitLaunchRequest) -> Result<Value, String> {
        (self.context.explicit_launch_action)(&principal(), request).map_err(|error| error.to_string())
    }
}

fn contract(value: &Value) -> Result<crate::agent_management::ExplicitLaunchContract, String> {
    serde_json::from_value(value.clone()).map_err(|error| format!("invalid launch contract: {error}"))
}

fn session_id(id: &str) -> Result<crate::role_revision::CutexSessionId, String> {
    crate::role_revision::CutexSessionId::new(id.to_string()).map_err(|error| format!("invalid agent ID: {error:?}"))
}

fn committed_at(receipt: &Value) -> Option<String> {
    receipt.get("committed_at").and_then(Value::as_str).map(str::to_string)
}

impl RunOps for DaemonOps {
    fn agent(&self, id: &str) -> Result<Option<AgentSnapshot>, String> {
        let store = crate::session::store::load_cutex_session_store().map_err(|e| format!("{e:#}"))?;
        let Some(record) = store.sessions.values().find(|record| record.cutex_session_id == id) else {
            return Ok(None);
        };
        let Some(session) = self.session(id)? else {
            return Ok(None);
        };
        let versions = crate::launch::version_registry::load_relaxed();
        Ok(Some(snapshot(record, Some(&session), versions.as_ref())))
    }

    fn offline(&self, id: &str, request_id: &str) -> Result<(), String> {
        self.runtime_mutation(id, request_id, false)
    }

    fn online(&self, id: &str, request_id: &str) -> Result<(), String> {
        self.runtime_mutation(id, request_id, true)
    }

    fn reconcile(&self, id: &str) -> Result<String, String> {
        let value = self.explicit(&crate::agent_management::ExplicitLaunchRequest::ReconcileExitedRuntime {
            cutex_session_id: session_id(id)?,
        })?;
        Ok(value.get("detail").and_then(Value::as_str).unwrap_or_default().to_string())
    }

    fn review(&self, id: &str) -> Result<ReviewFacts, String> {
        let value = self.explicit(&crate::agent_management::ExplicitLaunchRequest::ReviewRuntimePackageUpgrade {
            cutex_session_id: session_id(id)?,
        })?;
        let review: crate::agent_management::RuntimePackageUpgradeReview =
            serde_json::from_value(value.clone()).map_err(|error| format!("unexpected review: {error}"))?;
        // The per-agent target manifest is written by apply; the review
        // projects it from the selected manifest, so that one is checked.
        let target = std::fs::read(&review.selected_manifest)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<crate::launch::stock::StockBundle>(&bytes).ok());
        Ok(ReviewFacts {
            selected_manifest: review.selected_manifest.clone(),
            selected_manifest_sha256: review.selected_manifest_sha256.as_str().to_string(),
            target_native: target.as_ref().map(|bundle| pinned(&bundle.executable)),
            target_facade: target.as_ref().map(|bundle| pinned(&bundle.facade)),
            current_contract: serde_json::to_value(&review.current_contract).map_err(|e| e.to_string())?,
            target_contract: serde_json::to_value(&review.target_contract).map_err(|e| e.to_string())?,
            current_runtime_version: review.current_runtime_version.clone(),
            target_runtime_version: review.target_runtime_version.clone(),
            repair: review.repair.as_ref().map(|repair| {
                repair
                    .missing_files
                    .iter()
                    .map(|path| crate::agent_management::strip_verbatim_prefix(&path.to_string_lossy()))
                    .collect()
            }),
            review: value,
        })
    }

    fn apply(&self, action_id: &str, review: &Value) -> Result<Option<String>, String> {
        let receipt = self.explicit(&crate::agent_management::ExplicitLaunchRequest::ApplyRuntimePackageUpgrade {
            action_id: crate::agent_management::AgentActionId::new(action_id.to_string())
                .map_err(|error| error.to_string())?,
            review: serde_json::from_value(review.clone()).map_err(|error| format!("invalid review: {error}"))?,
        })?;
        Ok(committed_at(&receipt))
    }

    fn revert(&self, id: &str, expected: &Value, restore: &Value) -> Result<Option<String>, String> {
        let receipt = self.explicit(&crate::agent_management::ExplicitLaunchRequest::RevertRuntimePackageUpgrade {
            cutex_session_id: session_id(id)?,
            expected_contract: contract(expected)?,
            restore_contract: contract(restore)?,
        })?;
        Ok(committed_at(&receipt))
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }

    fn record(&self, run: &Run) {
        self.coordinator.update(run);
    }
}

// ---------------------------------------------------------------------------
// HTTP

fn error(stream: &mut TcpStream, status: u16, code: &str, message: &str, details: Value) -> anyhow::Result<()> {
    let reason = match status {
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        _ => "Service Unavailable",
    };
    super::server::write_v2_error(stream, status, reason, code, message, status == 503, details)
}

fn unavailable(stream: &mut TcpStream, failure: &anyhow::Error) -> anyhow::Result<()> {
    error(
        stream,
        503,
        "runtime_packages_unavailable",
        "runtime package state could not be read",
        json!({ "diagnostic": format!("{failure:#}") }),
    )
}

fn in_progress(stream: &mut TcpStream, run_id: &str) -> anyhow::Result<()> {
    error(
        stream,
        409,
        "run_in_progress",
        "a runtime package run is already in progress",
        json!({ "runId": run_id }),
    )
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest, context: ManagementRequestContext) -> anyhow::Result<()> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let route = match parse_route(&request.method, path) {
        Ok(route) => route,
        Err(true) => {
            return error(stream, 405, "method_not_allowed", "method not allowed on this runtime package route", json!({}))
        }
        Err(false) => {
            return error(stream, 404, "route_not_found", "management v2 route not found", json!({ "path": path }))
        }
    };
    let coordinator = match coordinator() {
        Ok(coordinator) => coordinator,
        Err(failure) => return unavailable(stream, &failure),
    };
    match route {
        Route::Overview => {
            let agents = match outdated_agents(context) {
                Ok(agents) => agents,
                Err(failure) => return unavailable(stream, &failure),
            };
            let (selected, selected_error) = selected_overview();
            let runs = coordinator.runs();
            let last_run = runs.iter().find(|run| run.state != RunState::Running);
            write_json_response(
                stream,
                200,
                "OK",
                &json!({
                    "hostId": crate::platform::host::current_host_name(),
                    "selected": selected,
                    "selectedError": selected_error,
                    "agents": agents.iter().map(|(id, agent)| outdated_agent_json(id, agent)).collect::<Vec<_>>(),
                    "activeRun": coordinator.active(),
                    "lastRun": last_run,
                    "revertable": revertable_overview(&runs),
                }),
            )
        }
        Route::ListRuns => {
            let limit = match parse_limit(&request.path) {
                Ok(limit) => limit,
                Err(message) => return error(stream, 400, "invalid_request", &message, json!({})),
            };
            let runs = coordinator.runs().into_iter().take(limit).collect::<Vec<_>>();
            write_json_response(stream, 200, "OK", &json!({ "runs": runs }))
        }
        Route::GetRun(run_id) => match coordinator.get(&run_id) {
            Some(run) => write_json_response(stream, 200, "OK", &json!({ "run": run })),
            None => run_not_found(stream, &run_id),
        },
        Route::StartRun => {
            let selection = match parse_start_body(&request.body) {
                Ok(selection) => selection,
                Err(message) => return error(stream, 400, "invalid_request", &message, json!({})),
            };
            if let Some(active) = coordinator.active() {
                return in_progress(stream, &active.run_id);
            }
            let selected = match selected_snapshot() {
                Ok(selected) => selected,
                Err(failure) => {
                    return error(
                        stream,
                        409,
                        "selected_unavailable",
                        "no readable selected runtime deployment",
                        json!({ "diagnostic": format!("{failure:#}") }),
                    )
                }
            };
            let candidates = match outdated_agents(context) {
                Ok(candidates) => candidates,
                Err(failure) => return unavailable(stream, &failure),
            };
            let items = select_items(&candidates, &selection);
            if items.is_empty() {
                return error(stream, 409, "nothing_to_upgrade", "no requested agent is outdated", json!({}));
            }
            let run = Run::new(RunKind::Upgrade, None, selected.summary.clone(), items);
            spawn(stream, coordinator, context, run, || {}, move |run, ops| run_upgrade(run, &selected, ops))
        }
        Route::RevertRun(run_id) => {
            let selection = match parse_revert_body(&request.body) {
                Ok(selection) => selection,
                Err(message) => return error(stream, 400, "invalid_request", &message, json!({})),
            };
            let Some(mut source) = coordinator.get(&run_id) else {
                return run_not_found(stream, &run_id);
            };
            if source.kind == RunKind::Revert {
                return error(stream, 400, "invalid_request", "a revert run cannot be reverted", json!({ "runId": run_id }));
            }
            if let Some(active) = coordinator.active() {
                return in_progress(stream, &active.run_id);
            }
            let RevertPlan { items, sources, already_reverted } =
                revert_items(&source, &reverted_agents(&source, &coordinator.runs()), &selection);
            if items.is_empty() && already_reverted > 0 {
                return error(
                    stream,
                    409,
                    "already_reverted",
                    "every matching moved agent was already reverted",
                    json!({ "runId": run_id, "revertedBy": source.reverted_by }),
                );
            }
            if items.is_empty() {
                return error(stream, 409, "nothing_to_revert", "the run has no matching moved agent", json!({ "runId": run_id }));
            }
            let selected = selected_overview().0.unwrap_or_default();
            let run = Run::new(RunKind::Revert, Some(run_id), selected, items);
            let revert_id = run.run_id.clone();
            let source_coordinator = coordinator.clone();
            let on_started = move || {
                source.reverted_by.push(revert_id);
                source_coordinator.update(&source);
            };
            spawn(stream, coordinator, context, run, on_started, move |run, ops| run_revert(run, &sources, ops))
        }
    }
}

fn run_not_found(stream: &mut TcpStream, run_id: &str) -> anyhow::Result<()> {
    error(stream, 404, "run_not_found", "no runtime package run has this id", json!({ "runId": run_id }))
}

/// Claim the run slot, answer 202 and process the run on a daemon thread.
fn spawn(
    stream: &mut TcpStream,
    coordinator: Arc<Coordinator>,
    context: ManagementRequestContext,
    run: Run,
    on_started: impl FnOnce(),
    job: impl FnOnce(&mut Run, &dyn RunOps) + Send + 'static,
) -> anyhow::Result<()> {
    if let Err(active) = coordinator.try_begin(&run) {
        return in_progress(stream, &active);
    }
    on_started();
    let thread_coordinator = coordinator.clone();
    let mut thread_run = run.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("cutex-runtime-packages-{}", run.run_id))
        .spawn(move || {
            let ops = DaemonOps { context, coordinator: thread_coordinator.clone() };
            job(&mut thread_run, &ops);
            thread_coordinator.finish(&thread_run);
        });
    if let Err(failure) = spawned {
        let mut failed = run.clone();
        failed.mark_interrupted(&now());
        coordinator.finish(&failed);
        return unavailable(stream, &anyhow::anyhow!("could not start the run thread: {failure}"));
    }
    write_json_response(stream, 202, "Accepted", &json!({ "run": run }))
}

#[cfg(test)]
#[path = "runtime_packages_tests.rs"]
mod tests;
