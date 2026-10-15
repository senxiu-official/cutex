//! Auto capture (cutex-desktop `docs/research/artifacts-api-v1.md` §Auto
//! capture): cutex records each project member's turn windows and, when a
//! member turn ends, scans the project's workspace root once for changed
//! files matching `artifacts.autoCapture.patterns`. A changed file becomes
//! the next version of the project artifact with its relative path as name
//! (`producer.kind: "auto"`), attributed to the member whose turn window
//! holds its mtime (`exclusive`), to several (`ambiguous`) or to nobody
//! (`unknown`).
//!
//! Off the hot path: the runtime event hook only parses `turn/started` /
//! `turn/completed` and hands them to one worker thread (management
//! process). Turn ends arriving together are scanned once per project.
//! Bounded: [`Budget`] (entries visited, wall time; the walk and the
//! processing of what it found each get the time budget); a hit is logged,
//! what was found is still processed, and the next scan resumes the walk
//! after the last entry handled (meta `captureCursor:`), so a workspace over
//! the budget is covered over several turn ends. Size + mtime first; sha256 only for
//! changed files up to `maxBlobBytes` (larger ones are registered as
//! `external` without hashing).
//!
//! Skipped: `.git` / `.cutex` directories, links to directories (Windows
//! junctions included), links to files outside the workspace root, the
//! cutex home and credential locations ([`super::paths::forbidden_reason`])
//! and whatever the root's `.cutexignore` excludes. Explicit publish wins:
//! content (sha256) already published explicitly in the project adds
//! nothing. The first scan of a project records files last changed before
//! its earliest turn window as the baseline instead of capturing them; the
//! baseline ends only after one pass covered and processed the whole
//! workspace.
//!
//! The capture state (file map, cursor, baseline) belongs to the root it was
//! taken for (meta `captureRoot:`). When the project's workspace root changes
//! to another path or is cleared — through `set_workspace_root` or while the
//! daemon was down — the state is dropped and the next scan of the new root
//! records every file it finds as the baseline (cutoff: that scan's time)
//! instead of comparing them with the old root's files. State written before
//! the root was recorded adopts the current root once, unchanged.

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::glob::{GlobSet, IgnoreRules};
use super::host::{ArtifactHost, ManagementHost};
use super::paths::{self, Resolved};
use super::publish::{self, GitCache, PublishError};
use super::store::{ArtifactStore, CaptureFile, NewVersion, TurnWindow};
use super::{Candidate, Producer, Scope};

/// A file's mtime may fall this far outside a turn window and still count.
pub const ATTRIBUTION_SLACK_MS: i64 = 2_000;
/// Turn windows are kept this long (attribution looks no further back).
pub const WINDOW_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// Turn ends this close together are scanned together.
const COALESCE: Duration = Duration::from_millis(500);
const PRUNE_EVERY_MS: i64 = 60 * 60 * 1000;
/// Meta key prefix: when a project was last scanned (absent = never; the
/// first scan baselines old files).
const SCANNED_KEY_PREFIX: &str = "captureScannedAt:";
/// Meta key prefix: the last entry a budget-stopped walk handled (the next
/// walk resumes after it).
const CURSOR_KEY_PREFIX: &str = "captureCursor:";
/// Meta key prefix: whether the scans of the current pass (since the walk
/// last started from the beginning) all processed what they found.
const COVERAGE_KEY_PREFIX: &str = "captureCoverage:";
/// Meta key prefix: the canonical workspace root the capture state was taken
/// for (absent = state from before this key existed: adopt the current root;
/// empty = reset because the root was cleared).
const ROOT_KEY_PREFIX: &str = "captureRoot:";
/// Meta key prefix: the baseline cutoff (ms) of a pass that started over
/// after a root change (absent = the project's earliest turn window).
const BASELINE_KEY_PREFIX: &str = "captureBaselineBefore:";
const FORBIDDEN_DIR_NAMES: &[&str] = &[".git", ".cutex"];

/// Limits of one project scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    /// Directory entries visited.
    pub max_entries: usize,
    /// Wall time of the walk plus hashing.
    pub max_time: Duration,
}

impl Default for Budget {
    fn default() -> Self {
        Self { max_entries: 20_000, max_time: Duration::from_secs(5) }
    }
}

/// A member turn started or ended (from a runtime notification).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnEvent {
    pub session: String,
    pub runtime_generation: u64,
    pub started: bool,
    pub turn_id: Option<String>,
    /// The notification's thread (sub-agent threads report their own turns).
    pub thread_id: Option<String>,
    pub at_ms: i64,
}

impl TurnEvent {
    pub fn from_notification(session: &str, runtime_generation: u64, method: &str, params: Option<&Value>, at_ms: i64) -> Option<Self> {
        let started = match method {
            "turn/started" => true,
            "turn/completed" => false,
            _ => return None,
        };
        let text = |value: Option<&Value>| value.and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty()).map(str::to_string);
        Some(Self {
            session: session.to_string(),
            runtime_generation,
            started,
            turn_id: text(params.and_then(|params| params.pointer("/turn/id").or_else(|| params.get("turnId")))),
            thread_id: text(params.and_then(|params| params.get("threadId"))),
            at_ms,
        })
    }
}

/// What cutex knows about the session when its turn starts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentFacts {
    pub project: Option<String>,
    pub display_name: Option<String>,
    pub profile: Option<String>,
    pub model: Option<String>,
    /// The session's main thread.
    pub thread_id: Option<String>,
    /// The delegation holding the session's admission lease.
    pub ask_id: Option<String>,
}

// --- hot path ---------------------------------------------------------------

/// Runtime notification hook (management process, every runtime): only
/// turn starts / ends are passed on, to the capture worker.
pub fn on_runtime_event(session: &str, runtime_generation: u64, method: &str, params: Option<&Value>) {
    if method != "turn/started" && method != "turn/completed" {
        return;
    }
    let Some(event) = TurnEvent::from_notification(session, runtime_generation, method, params, Utc::now().timestamp_millis()) else {
        return;
    };
    if let Some(sender) = worker() {
        let _ = sender.send(event);
    }
}

fn worker() -> Option<&'static mpsc::Sender<TurnEvent>> {
    static WORKER: OnceLock<Option<mpsc::Sender<TurnEvent>>> = OnceLock::new();
    WORKER
        .get_or_init(|| {
            if crate::config::paths::in_test_harness() {
                return None;
            }
            let (sender, receiver) = mpsc::channel();
            match std::thread::Builder::new().name("cutex-artifact-capture".into()).spawn(move || run(receiver)) {
                Ok(_) => Some(sender),
                Err(error) => {
                    eprintln!("artifacts: auto capture worker not started: {error}");
                    None
                }
            }
        })
        .as_ref()
}

// --- worker -----------------------------------------------------------------

fn run(receiver: mpsc::Receiver<TurnEvent>) {
    let host = ManagementHost;
    let mut last_prune_ms = 0;
    while let Ok(first) = receiver.recv() {
        let mut events = vec![first];
        let deadline = Instant::now() + COALESCE;
        while let Ok(event) = receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            events.push(event);
        }
        if !host.settings().auto_capture.enabled {
            continue;
        }
        let store = match host.store() {
            Ok(store) => store,
            Err(error) => {
                eprintln!("artifacts: auto capture: store unavailable: {error:#}");
                continue;
            }
        };
        let directory = crate::asks::directory::Directory::load().ok();
        let asks = crate::asks::store::AskStore::open_default().ok().and_then(|store| store.peek().ok().flatten());
        let mut projects = BTreeSet::new();
        for event in &events {
            let facts = facts_of(&host, directory.as_ref(), asks.as_ref(), &event.session);
            match record(&store, event, &facts) {
                Ok(Some(project)) => {
                    projects.insert(project);
                }
                Ok(None) => {}
                Err(error) => eprintln!("artifacts: auto capture: turn window of {}: {error:#}", event.session),
            }
        }
        for project in projects {
            match scan_project(&host, &store, &project, Utc::now(), Budget::default()) {
                Ok(report) if report.budget_hit => eprintln!(
                    "artifacts: auto capture of project {project} stopped at the scan budget ({} entries / {} s) after {} entries; the rest waits for the next turn end",
                    Budget::default().max_entries,
                    Budget::default().max_time.as_secs(),
                    report.visited
                ),
                Ok(_) => {}
                Err(error) => eprintln!("artifacts: auto capture of project {project}: {error:#}"),
            }
        }
        let now_ms = Utc::now().timestamp_millis();
        if now_ms - last_prune_ms >= PRUNE_EVERY_MS {
            last_prune_ms = now_ms;
            if let Err(error) = store.prune_windows(now_ms - WINDOW_RETENTION_MS) {
                eprintln!("artifacts: auto capture: pruning turn windows: {error:#}");
            }
        }
    }
}

fn facts_of(
    host: &dyn ArtifactHost,
    directory: Option<&crate::asks::directory::Directory>,
    asks: Option<&crate::asks::store::AskState>,
    session: &str,
) -> AgentFacts {
    let info = directory.and_then(|directory| directory.get(session));
    let ask_id = asks.and_then(|state| state.leases.get(session)).and_then(|lease| match &lease.holder {
        crate::asks::model::LeaseHolder::Ask { ask_id, .. } => Some(ask_id.clone()),
        _ => None,
    });
    AgentFacts {
        project: match info {
            Some(info) => info.project.clone(),
            None => host.session_project(session),
        },
        display_name: info.and_then(|info| info.name.clone()),
        profile: info.and_then(|info| info.profile.clone()),
        model: info.and_then(|info| info.model.clone()),
        thread_id: info.and_then(|info| info.thread_id.clone()),
        ask_id,
    }
}

/// Record a turn start / end. Returns the project to scan after a member
/// turn ended.
pub fn record(store: &ArtifactStore, event: &TurnEvent, facts: &AgentFacts) -> anyhow::Result<Option<String>> {
    // Sub-agent threads report their own turns; only the main thread's
    // turns open and close the member's window.
    if let (Some(thread), Some(main)) = (&event.thread_id, &facts.thread_id) {
        if thread != main {
            return Ok(None);
        }
    }
    if event.started {
        let Some(project) = facts.project.clone() else {
            return Ok(None);
        };
        store.window_started(&TurnWindow {
            session_id: event.session.clone(),
            turn_id: event.turn_id.clone(),
            project_id: Some(project),
            display_name: facts.display_name.clone(),
            ask_id: facts.ask_id.clone(),
            runtime_generation: Some(event.runtime_generation).filter(|generation| *generation > 0),
            profile: facts.profile.clone(),
            model: facts.model.clone(),
            started_at_ms: event.at_ms,
            ended_at_ms: None,
        })?;
        return Ok(None);
    }
    let project = store.window_ended(&event.session, event.at_ms)?;
    Ok(project.or_else(|| facts.project.clone()))
}

/// Who most likely wrote a file last changed at `mtime_ms`.
pub fn attribute(windows: &[TurnWindow], mtime_ms: i64, now_ms: i64) -> Producer {
    let mut hits: Vec<&TurnWindow> = Vec::new();
    for window in windows {
        let inside = window.started_at_ms - ATTRIBUTION_SLACK_MS <= mtime_ms
            && mtime_ms <= window.effective_end(now_ms) + ATTRIBUTION_SLACK_MS;
        if !inside {
            continue;
        }
        // One candidate per session: its latest window holding the mtime.
        match hits.iter_mut().find(|hit| hit.session_id == window.session_id) {
            Some(hit) if hit.started_at_ms < window.started_at_ms => *hit = window,
            Some(_) => {}
            None => hits.push(window),
        }
    }
    let candidate = |window: &TurnWindow| Candidate {
        session_id: window.session_id.clone(),
        display_name: window.display_name.clone(),
        turn_id: window.turn_id.clone(),
    };
    match hits.as_slice() {
        [] => Producer { kind: "auto".into(), attribution: Some("unknown".into()), ..Producer::default() },
        [window] => Producer {
            kind: "auto".into(),
            session_id: Some(window.session_id.clone()),
            display_name: window.display_name.clone(),
            runtime_generation: window.runtime_generation,
            turn_id: window.turn_id.clone(),
            ask_id: window.ask_id.clone(),
            profile: window.profile.clone(),
            model: window.model.clone(),
            attribution: Some("exclusive".into()),
            candidates: vec![candidate(window)],
        },
        several => Producer {
            kind: "auto".into(),
            attribution: Some("ambiguous".into()),
            candidates: several.iter().map(|window| candidate(window)).collect(),
            ..Producer::default()
        },
    }
}

/// What one scan did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// Directory entries visited.
    pub visited: usize,
    /// Names captured as new versions.
    pub captured: Vec<String>,
    /// Old files recorded as the first scan's baseline.
    pub baselined: usize,
    pub budget_hit: bool,
    pub quota_full: bool,
}

/// Scan a project's workspace root once (see the module docs).
pub fn scan_project(host: &dyn ArtifactHost, store: &ArtifactStore, project: &str, now: DateTime<Utc>, budget: Budget) -> anyhow::Result<ScanReport> {
    let mut report = ScanReport::default();
    let settings = host.settings();
    if !settings.auto_capture.enabled || settings.auto_capture.patterns.is_empty() {
        return Ok(report);
    }
    let now_ms = now.timestamp_millis();
    let configured = host.project_root(project);
    let Some(root) = configured.as_deref().and_then(paths::canonical) else {
        if configured.is_none() {
            root_cleared(store, project)?;
        }
        // A configured root that is missing right now keeps its state.
        return Ok(report);
    };
    if !root.is_dir() {
        return Ok(report);
    }
    sync_root(store, project, &root, now_ms)?;
    let started = Instant::now();
    let walk_over_budget = |visited: usize| visited > budget.max_entries || started.elapsed() > budget.max_time;
    let globs = GlobSet::new(&settings.auto_capture.patterns);
    let ignore = IgnoreRules::load(&root);
    let cursor_key = format!("{CURSOR_KEY_PREFIX}{project}");
    // Where the last budget-stopped walk ended: this walk resumes after it,
    // so a workspace over the entry budget is still covered over several
    // scans.
    let cursor = store.meta(&cursor_key)?;
    let skip = |rel: &str, is_dir: bool| match cursor.as_deref() {
        None => false,
        Some(cursor) => match rel_order(rel, cursor) {
            std::cmp::Ordering::Greater => false,
            // A directory holding the cursor (or the cursor itself) still
            // has entries after it.
            _ if is_dir => !(rel == cursor || rel.is_empty() || cursor.starts_with(&format!("{rel}/"))),
            _ => true,
        },
    };

    // Walk: the literal bases of the patterns, without following links to
    // directories, depth first in path order (one stack of entries, so the
    // order matches `rel_order` and the cursor).
    let mut found: Vec<(String, PathBuf)> = Vec::new();
    let mut stack: Vec<(PathBuf, String, bool)> = Vec::new();
    let mut bases = globs.walk_bases();
    bases.sort_by(|left, right| rel_order(left, right));
    for base in bases.into_iter().rev() {
        if base.is_empty() {
            stack.push((root.clone(), String::new(), true));
            continue;
        }
        let start = base.split('/').fold(root.clone(), |path, part| path.join(part));
        // A base reached through a link (or missing) is not walked.
        let walkable = paths::canonical(&start).is_some_and(|real| {
            real.is_dir()
                && paths::relative(&real, &root)
                    .is_some_and(|rel| paths::key(std::path::Path::new(&rel)) == paths::key(std::path::Path::new(&base)))
        });
        if !walkable || base.split('/').any(is_forbidden_dir) || ignore.is_ignored(&base, true) || skip(&base, true) {
            continue;
        }
        stack.push((start, base, true));
    }
    let mut last_done: Option<String> = None;
    while let Some((path, rel, expand)) = stack.pop() {
        if expand {
            // A walk base or a directory entry already counted.
            let Ok(entries) = std::fs::read_dir(&path) else {
                continue;
            };
            let mut entries: Vec<(String, std::fs::DirEntry)> =
                entries.flatten().map(|entry| (entry.file_name().to_string_lossy().to_string(), entry)).collect();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            for (name, entry) in entries.into_iter().rev() {
                let child = if rel.is_empty() { name } else { format!("{rel}/{name}") };
                stack.push((entry.path(), child, false));
            }
            continue;
        }
        let Ok(file_type) = std::fs::symlink_metadata(&path).map(|metadata| metadata.file_type()) else {
            continue;
        };
        // `is_dir` / `is_file` are false for links (junctions included).
        if skip(&rel, file_type.is_dir()) {
            continue;
        }
        report.visited += 1;
        if walk_over_budget(report.visited) {
            report.budget_hit = true;
            break;
        }
        last_done = Some(rel.clone());
        let name = rel.rsplit('/').next().unwrap_or(&rel);
        if file_type.is_dir() {
            if !is_forbidden_dir(name) && !ignore.is_ignored(&rel, true) {
                stack.push((path, rel, true));
            }
            continue;
        }
        let real = if file_type.is_symlink() {
            match paths::canonical(&path) {
                Some(real) if real.is_file() && paths::within(&real, &root) => real,
                _ => continue,
            }
        } else if file_type.is_file() {
            path
        } else {
            continue;
        };
        if globs.matches(&rel) && !ignore.is_ignored(&rel, false) {
            found.push((rel, real));
        }
    }
    let walk_complete = !report.budget_hit;
    let walked_everything = walk_complete && cursor.is_none();

    // Compare with the last scan.
    let previous = store.capture_files(project)?;
    let scanned_key = format!("{SCANNED_KEY_PREFIX}{project}");
    let baseline_key = format!("{BASELINE_KEY_PREFIX}{project}");
    let baseline_before = match store.meta(&scanned_key)? {
        Some(_) => None,
        None => match store.meta(&baseline_key)?.and_then(|cutoff| cutoff.parse::<i64>().ok()) {
            Some(cutoff) => Some(cutoff),
            None => Some(store.earliest_window_ms(project)?.unwrap_or(now_ms)),
        },
    };
    let windows = store.project_windows(project, now_ms - WINDOW_RETENTION_MS)?;
    let scope = Scope::project(project);
    let mut git = GitCache::default();
    let mut updates: Vec<(String, Option<CaptureFile>)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let processing = Instant::now();
    let mut processed_all = true;
    for (rel, _) in &found {
        seen.insert(rel.clone());
    }
    for (rel, real) in found {
        let Ok(metadata) = std::fs::metadata(&real) else {
            continue;
        };
        let mtime = paths::mtime_ms(&metadata);
        let state = CaptureFile { bytes: metadata.len(), mtime_ms: mtime.unwrap_or(0), sha256: None };
        let before = previous.get(&rel);
        if before.is_some_and(|before| before.bytes == state.bytes && before.mtime_ms == state.mtime_ms) {
            continue;
        }
        // Its own time budget: a walk that used its budget still processes
        // what it found.
        if processing.elapsed() > budget.max_time {
            report.budget_hit = true;
            processed_all = false;
            break;
        }
        if paths::forbidden_reason(&real).is_some() {
            continue;
        }
        if baseline_before.is_some_and(|before| state.mtime_ms < before) {
            updates.push((rel, Some(state)));
            report.baselined += 1;
            continue;
        }
        let name = paths::relative(&real, &root).filter(|name| !name.is_empty()).unwrap_or_else(|| rel.clone());
        if super::clean_name(&name).as_deref() != Some(name.as_str()) {
            continue;
        }
        let resolved = Resolved { path: real.clone(), bytes: state.bytes, mtime_ms: mtime };
        let (content, sample) = match publish::read_content(&resolved, &settings, false) {
            Ok(read) => read,
            Err(error) => {
                eprintln!("artifacts: auto capture: {error}");
                continue;
            }
        };
        let sha256 = content.sha256().map(str::to_string);
        let state = CaptureFile { sha256: sha256.clone(), ..state };
        let unchanged = sha256.is_some() && before.and_then(|before| before.sha256.as_deref()) == sha256.as_deref();
        if unchanged {
            updates.push((rel, Some(state)));
            continue;
        }
        if let Some(sha) = &sha256 {
            if store.explicit_has_sha(&scope, sha)? {
                updates.push((rel, Some(state)));
                continue;
            }
        }
        let new = NewVersion {
            scope: scope.clone(),
            default_kind: paths::default_kind(&name).to_string(),
            media_type: paths::media_type(&name, &sample),
            name: name.clone(),
            kind: None,
            content,
            source_path: real.display().to_string(),
            source_mtime_ms: mtime,
            note: None,
            producer: attribute(&windows, state.mtime_ms, now_ms),
            git: git.info(&real),
        };
        match publish::commit(host, store, new, now) {
            Ok(outcome) => {
                if outcome.new_version {
                    report.captured.push(name);
                }
                updates.push((rel, Some(state)));
            }
            Err(PublishError::QuotaFull { .. }) => {
                report.quota_full = true;
                break;
            }
            Err(error) => eprintln!("artifacts: auto capture of {name}: {error}"),
        }
    }
    let segment_ok = processed_all && !report.quota_full;
    // Files gone (or no longer matching) are forgotten after a full walk
    // from the start.
    if walked_everything && segment_ok {
        let gone: Vec<String> = previous.keys().filter(|rel| !seen.contains(*rel)).cloned().collect();
        updates.extend(gone.into_iter().map(|rel| (rel, None)));
    }
    store.set_capture_files(project, &updates)?;
    // The next walk resumes after this one (or starts over at the end).
    let next_cursor = if walk_complete { None } else { last_done.or_else(|| cursor.clone()) };
    store.set_meta(&cursor_key, next_cursor.as_deref())?;
    // The baseline holds until one pass (one walk from the start, possibly
    // over several budget-stopped scans) covered and processed the whole
    // workspace: marking a partial first pass as done would make every old
    // file it did not reach look new on a later scan.
    let coverage_key = format!("{COVERAGE_KEY_PREFIX}{project}");
    let pass_ok = segment_ok && (cursor.is_none() || store.meta(&coverage_key)?.as_deref() == Some("ok"));
    let complete = walk_complete && pass_ok;
    store.set_meta(&coverage_key, if walk_complete { None } else if pass_ok { Some("ok") } else { Some("broken") })?;
    if complete {
        store.set_meta(&scanned_key, Some(&crate::asks::now_rfc3339(now)))?;
        store.set_meta(&baseline_key, None)?;
    }
    Ok(report)
}

/// The capture state belongs to `root`: a state taken for another root is
/// dropped, and the next pass baselines everything last changed before
/// `now_ms`. A state without a recorded root (written before the root was
/// recorded) adopts `root` as it is.
fn sync_root(store: &ArtifactStore, project: &str, root: &std::path::Path, now_ms: i64) -> anyhow::Result<()> {
    let root_key = format!("{ROOT_KEY_PREFIX}{project}");
    let current = root.display().to_string();
    match store.meta(&root_key)? {
        None => {}
        Some(stored) if !stored.is_empty() && paths::key(std::path::Path::new(&stored)) == paths::key(root) => {
            if stored == current {
                return Ok(());
            }
        }
        Some(_) => reset_capture_state(store, project, Some(now_ms))?,
    }
    // Recorded last: a reset interrupted before this point runs again.
    store.set_meta(&root_key, Some(&current))
}

/// The project has no workspace root (any more): a state taken for a root is
/// dropped, so a root set later starts with a fresh baseline. A project that
/// never had any state keeps the first-scan behaviour.
fn root_cleared(store: &ArtifactStore, project: &str) -> anyhow::Result<()> {
    let root_key = format!("{ROOT_KEY_PREFIX}{project}");
    let had_state = match store.meta(&root_key)? {
        Some(stored) => !stored.is_empty(),
        None => store.meta(&format!("{SCANNED_KEY_PREFIX}{project}"))?.is_some() || !store.capture_files(project)?.is_empty(),
    };
    if !had_state {
        return Ok(());
    }
    reset_capture_state(store, project, None)?;
    store.set_meta(&root_key, Some(""))
}

fn reset_capture_state(store: &ArtifactStore, project: &str, baseline_before_ms: Option<i64>) -> anyhow::Result<()> {
    store.clear_capture_files(project)?;
    store.set_meta(&format!("{CURSOR_KEY_PREFIX}{project}"), None)?;
    store.set_meta(&format!("{COVERAGE_KEY_PREFIX}{project}"), None)?;
    store.set_meta(&format!("{SCANNED_KEY_PREFIX}{project}"), None)?;
    store.set_meta(&format!("{BASELINE_KEY_PREFIX}{project}"), baseline_before_ms.map(|ms| ms.to_string()).as_deref())
}

/// Path order of the walk: component by component.
fn rel_order(left: &str, right: &str) -> std::cmp::Ordering {
    let parts = |rel: &str| -> Vec<String> {
        if rel.is_empty() {
            Vec::new()
        } else {
            rel.split('/').map(str::to_string).collect()
        }
    };
    parts(left).cmp(&parts(right))
}

fn is_forbidden_dir(name: &str) -> bool {
    FORBIDDEN_DIR_NAMES.iter().any(|forbidden| forbidden.eq_ignore_ascii_case(name))
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::host::tests::TestHost;
    use crate::artifacts::paths::tests::make_dir_link;
    use crate::artifacts::publish::PublishRequest;
    use crate::artifacts::store::ListFilter;
    use crate::management::v2::schema_export::assert_valid;
    use serde_json::json;
    use std::path::Path;

    fn set_mtime(path: &Path, ms: i64) {
        let at = std::time::UNIX_EPOCH + Duration::from_millis(ms as u64);
        std::fs::File::options().write(true).open(path).unwrap().set_modified(at).unwrap();
    }

    fn write(root: &Path, rel: &str, bytes: &[u8], mtime_ms: i64) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        set_mtime(&path, mtime_ms);
    }

    fn window(session: &str, turn: &str, start: i64, end: Option<i64>) -> TurnWindow {
        TurnWindow {
            session_id: session.into(),
            turn_id: Some(turn.into()),
            project_id: Some("p".into()),
            display_name: Some(format!("{session}-name")),
            ask_id: Some(format!("ask-{turn}")),
            runtime_generation: Some(2),
            profile: Some("prof".into()),
            model: Some("gpt".into()),
            started_at_ms: start,
            ended_at_ms: end,
        }
    }

    fn summaries(store: &ArtifactStore) -> Vec<crate::artifacts::ArtifactSummary> {
        store.list(&ListFilter { scopes: Some(vec![Scope::project("p")]), limit: 100, ..ListFilter::default() }).unwrap().0
    }

    fn named(store: &ArtifactStore, name: &str) -> crate::artifacts::ArtifactSummary {
        summaries(store).into_iter().find(|item| item.name == name).unwrap_or_else(|| panic!("no artifact {name}"))
    }

    fn sorted(mut names: Vec<String>) -> Vec<String> {
        names.sort();
        names
    }

    #[test]
    fn turn_events_open_and_close_member_windows() {
        let host = TestHost::new("capture-turns");
        let store = host.store().unwrap();
        assert!(TurnEvent::from_notification("s", 1, "item/completed", None, 0).is_none());
        let start = TurnEvent::from_notification("cutex.a", 3, "turn/started", Some(&json!({"threadId": "th-a", "turn": {"id": "turn-1"}})), 1_000).unwrap();
        assert_eq!((start.turn_id.as_deref(), start.thread_id.as_deref(), start.started), (Some("turn-1"), Some("th-a"), true));
        let end = TurnEvent::from_notification("cutex.a", 3, "turn/completed", Some(&json!({"threadId": "th-a", "turnId": "turn-1"})), 5_000).unwrap();
        assert_eq!((end.turn_id.as_deref(), end.started), (Some("turn-1"), false));
        let facts = AgentFacts {
            project: Some("p".into()),
            display_name: Some("Alice".into()),
            thread_id: Some("th-a".into()),
            ask_id: Some("ask-1".into()),
            ..AgentFacts::default()
        };
        assert_eq!(record(&store, &start, &facts).unwrap(), None);
        // A sub-agent thread's turn leaves the member's window alone.
        let child = TurnEvent { thread_id: Some("th-child".into()), started: false, at_ms: 2_000, ..start.clone() };
        assert_eq!(record(&store, &child, &facts).unwrap(), None);
        assert_eq!(store.project_windows("p", 0).unwrap()[0].ended_at_ms, None);
        assert_eq!(record(&store, &end, &facts).unwrap().as_deref(), Some("p"));
        let windows = store.project_windows("p", 0).unwrap();
        assert_eq!(windows.len(), 1);
        let recorded = &windows[0];
        assert_eq!(
            (recorded.session_id.as_str(), recorded.turn_id.as_deref(), recorded.ask_id.as_deref(), recorded.display_name.as_deref()),
            ("cutex.a", Some("turn-1"), Some("ask-1"), Some("Alice"))
        );
        assert_eq!((recorded.runtime_generation, recorded.started_at_ms, recorded.ended_at_ms), (Some(3), 1_000, Some(5_000)));
        // Sessions outside a project record nothing and trigger no scan.
        let loner = AgentFacts::default();
        assert_eq!(record(&store, &TurnEvent { session: "cutex.b".into(), ..start.clone() }, &loner).unwrap(), None);
        assert_eq!(record(&store, &TurnEvent { session: "cutex.b".into(), ..end.clone() }, &loner).unwrap(), None);
        // A member turn end without a recorded start still scans its project.
        let member = AgentFacts { project: Some("q".into()), ..AgentFacts::default() };
        assert_eq!(record(&store, &TurnEvent { session: "cutex.c".into(), ..end }, &member).unwrap().as_deref(), Some("q"));
    }

    #[test]
    fn attribution_is_exclusive_ambiguous_or_unknown() {
        let windows = vec![window("a", "t1", 10_000, Some(20_000)), window("b", "t2", 15_000, Some(30_000)), window("a", "t3", 40_000, None)];
        let now = 50_000;
        let only_a = attribute(&windows, 12_000, now);
        assert_valid("artifacts", "producer", &json!(only_a));
        assert_eq!((only_a.kind.as_str(), only_a.attribution.as_deref()), ("auto", Some("exclusive")));
        assert_eq!(
            (only_a.session_id.as_deref(), only_a.turn_id.as_deref(), only_a.ask_id.as_deref(), only_a.profile.as_deref(), only_a.model.as_deref()),
            (Some("a"), Some("t1"), Some("ask-t1"), Some("prof"), Some("gpt"))
        );
        assert_eq!((only_a.runtime_generation, only_a.display_name.as_deref()), (Some(2), Some("a-name")));
        assert_eq!(only_a.candidates, vec![Candidate { session_id: "a".into(), display_name: Some("a-name".into()), turn_id: Some("t1".into()) }]);
        // Slack before a start; outside it nobody.
        assert_eq!(attribute(&windows, 8_500, now).turn_id.as_deref(), Some("t1"));
        let nobody = attribute(&windows, 7_000, now);
        assert_valid("artifacts", "producer", &json!(nobody));
        assert_eq!((nobody.attribution.as_deref(), nobody.session_id.as_deref(), nobody.candidates.len()), (Some("unknown"), None, 0));
        // Overlapping members.
        let both = attribute(&windows, 18_000, now);
        assert_valid("artifacts", "producer", &json!(both));
        assert_eq!((both.attribution.as_deref(), both.session_id.as_deref(), both.turn_id.as_deref()), (Some("ambiguous"), None, None));
        assert_eq!(both.candidates.iter().map(|c| c.session_id.as_str()).collect::<Vec<_>>(), vec!["a", "b"]);
        // An open window lasts until now; the gap between turns is nobody's.
        assert_eq!(attribute(&windows, 45_000, now).turn_id.as_deref(), Some("t3"));
        assert_eq!(attribute(&windows, 35_000, now).attribution.as_deref(), Some("unknown"));
        // One session in two overlapping windows is one candidate (its latest).
        let twice = vec![window("a", "t1", 10_000, Some(20_000)), window("a", "t2", 19_000, Some(25_000))];
        let same = attribute(&twice, 19_500, now);
        assert_eq!((same.attribution.as_deref(), same.turn_id.as_deref()), (Some("exclusive"), Some("t2")));
    }

    #[test]
    fn scans_capture_changed_files_with_member_attribution() {
        let host = TestHost::new("capture-scan");
        let root = host.workspace("proj");
        let outside = host.workspace("outside");
        std::fs::write(outside.join("secret.md"), "s").unwrap();
        host.set_project_root("p", &root);
        let store = host.store().unwrap();
        let now = Utc::now();
        let now_ms = now.timestamp_millis();
        let png = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n', 0, 0];
        // Older than every turn window: the first scan's baseline.
        write(&root, "reports/old.md", b"old", now_ms - 3_600_000);
        store.window_started(&window("cutex.a", "t1", now_ms - 60_000, None)).unwrap();
        store.window_ended("cutex.a", now_ms - 30_000).unwrap();
        write(&root, "reports/r.md", b"# R\n", now_ms - 45_000);
        write(&root, "figures/f.png", &png, now_ms - 45_000);
        write(&root, "src/main.rs", b"fn main() {}", now_ms - 45_000);
        write(&root, "reports/.env", b"TOKEN=x", now_ms - 45_000);
        write(&root, "reports/.git/config", b"[core]", now_ms - 45_000);
        write(&root, "reports/private/p.md", b"p", now_ms - 45_000);
        write(&root, "reports/scratch.tmp", b"t", now_ms - 45_000);
        std::fs::write(root.join(".cutexignore"), "reports/private/\n*.tmp\n").unwrap();
        // Edited after the member's turn: nobody's.
        write(&root, "results/owner.csv", b"a,b\n", now_ms - 10_000);
        // Links leaving the root: inside a pattern base, and as the base.
        assert!(make_dir_link(&outside, &root.join("reports").join("out-link")), "directory link");
        assert!(make_dir_link(&outside, &root.join("outputs")), "directory link");

        let first = scan_project(&host, &store, "p", now, Budget::default()).unwrap();
        assert_eq!(sorted(first.captured.clone()), vec!["figures/f.png", "reports/r.md", "results/owner.csv"]);
        assert_eq!((first.baselined, first.budget_hit, first.quota_full), (1, false, false));
        assert_eq!(host.methods(), vec![crate::artifacts::EVENT_PUBLISHED; 3]);
        let report = named(&store, "reports/r.md");
        assert_eq!((report.kind.as_str(), report.scope.clone()), ("report", Scope::project("p")));
        let producer = &report.latest.producer;
        assert_eq!((producer.kind.as_str(), producer.attribution.as_deref()), ("auto", Some("exclusive")));
        assert_eq!((producer.session_id.as_deref(), producer.turn_id.as_deref(), producer.ask_id.as_deref()), (Some("cutex.a"), Some("t1"), Some("ask-t1")));
        assert_eq!(report.latest.source_path, root.join("reports").join("r.md").display().to_string());
        let figure = named(&store, "figures/f.png");
        assert_eq!((figure.kind.as_str(), figure.latest.media_type.as_str()), ("figure", "image/png"));
        let owner = named(&store, "results/owner.csv");
        assert_eq!((owner.kind.as_str(), owner.latest.producer.attribution.as_deref(), owner.latest.producer.session_id.as_deref()), ("table", Some("unknown"), None));

        // Nothing changed; then a touch without a content change.
        assert!(scan_project(&host, &store, "p", now, Budget::default()).unwrap().captured.is_empty());
        set_mtime(&root.join("reports/r.md"), now_ms - 20_000);
        assert!(scan_project(&host, &store, "p", now, Budget::default()).unwrap().captured.is_empty());
        assert_eq!(named(&store, "reports/r.md").latest_version, 1);

        // Two members at work: the next version is ambiguous; the old file
        // changed now is captured too.
        store.window_started(&window("cutex.b", "t2", now_ms - 15_000, None)).unwrap();
        store.window_started(&window("cutex.c", "t3", now_ms - 14_000, None)).unwrap();
        write(&root, "reports/r.md", b"# R v2\n", now_ms - 5_000);
        write(&root, "reports/old.md", b"new", now_ms - 5_000);
        let second = scan_project(&host, &store, "p", now, Budget::default()).unwrap();
        assert_eq!(sorted(second.captured), vec!["reports/old.md", "reports/r.md"]);
        let report = named(&store, "reports/r.md");
        assert_eq!(report.latest_version, 2);
        assert_eq!((report.latest.producer.attribution.as_deref(), report.latest.producer.session_id.as_deref()), (Some("ambiguous"), None));
        assert_eq!(report.latest.producer.candidates.len(), 2);

        // Explicit publish wins on the same content.
        write(&root, "handoff/h.md", b"handoff", now_ms - 5_000);
        let request = PublishRequest {
            scope: Scope::project("p"),
            path: "handoff/h.md".into(),
            base: Some(root.clone()),
            roots: vec![root.clone()],
            name_bases: vec![root.clone()],
            name: None,
            kind: None,
            note: None,
            producer: Producer::owner(),
        };
        publish::publish(&host, &request, now).unwrap();
        assert!(scan_project(&host, &store, "p", now, Budget::default()).unwrap().captured.is_empty());
        let handoff = named(&store, "handoff/h.md");
        assert_eq!((handoff.latest_version, handoff.latest.producer.kind.as_str()), (1, "owner"));

        // A deleted file is forgotten.
        std::fs::remove_file(root.join("reports/r.md")).unwrap();
        scan_project(&host, &store, "p", now, Budget::default()).unwrap();
        assert!(!store.capture_files("p").unwrap().contains_key("reports/r.md"));
        assert!(store.capture_files("p").unwrap().contains_key("reports/old.md"));
        assert!(summaries(&store).iter().all(|item| !item.name.contains("secret") && !item.name.contains("private") && !item.name.ends_with(".tmp")));
        #[cfg(windows)]
        {
            let _ = std::fs::remove_dir(root.join("reports").join("out-link"));
            let _ = std::fs::remove_dir(root.join("outputs"));
        }
    }

    #[test]
    fn large_files_budget_and_switch() {
        let host = TestHost::new("capture-limits");
        let root = host.workspace("proj");
        host.set_project_root("p", &root);
        let store = host.store().unwrap();
        let now = Utc::now();
        let now_ms = now.timestamp_millis();
        store.window_started(&window("cutex.a", "t1", now_ms - 60_000, Some(now_ms - 1_000))).unwrap();
        host.settings.borrow_mut().max_blob_bytes = 16;
        write(&root, "outputs/big.bin", &[7u8; 32], now_ms - 30_000);
        write(&root, "outputs/small.txt", b"small", now_ms - 30_000);
        let report = scan_project(&host, &store, "p", now, Budget::default()).unwrap();
        assert_eq!(sorted(report.captured), vec!["outputs/big.bin", "outputs/small.txt"]);
        let big = named(&store, "outputs/big.bin");
        assert_eq!((big.latest.storage.as_str(), big.latest.sha256.as_deref(), big.latest.bytes), ("external", None, 32));
        // External: a new mtime is a new version (never hashed during capture).
        set_mtime(&root.join("outputs/big.bin"), now_ms - 20_000);
        assert_eq!(scan_project(&host, &store, "p", now, Budget::default()).unwrap().captured, vec!["outputs/big.bin"]);
        assert_eq!(named(&store, "outputs/big.bin").latest_version, 2);

        // The budget stops the walk; nothing is forgotten then.
        for index in 0..5 {
            write(&root, &format!("outputs/many/{index}.txt"), format!("{index}").as_bytes(), now_ms - 30_000);
        }
        let tight = Budget { max_entries: 3, max_time: Duration::from_secs(5) };
        let stopped = scan_project(&host, &store, "p", now, tight).unwrap();
        assert!(stopped.budget_hit);
        // What the stopped walk found is still processed.
        assert_eq!(stopped.captured, vec!["outputs/many/0.txt"]);
        assert!(store.capture_files("p").unwrap().contains_key("outputs/big.bin"));
        // The next walk resumes after the cursor.
        let rest = scan_project(&host, &store, "p", now, Budget::default()).unwrap();
        assert!(!rest.budget_hit);
        assert_eq!(sorted(rest.captured), vec!["outputs/many/1.txt", "outputs/many/2.txt", "outputs/many/3.txt", "outputs/many/4.txt"]);

        assert_eq!(store.meta("captureCursor:p").unwrap(), None);

        // Switched off: no scan.
        host.settings.borrow_mut().auto_capture.enabled = false;
        write(&root, "outputs/later.txt", b"later", now_ms - 30_000);
        assert_eq!(scan_project(&host, &store, "p", now, Budget::default()).unwrap(), ScanReport::default());
    }

    #[test]
    fn workspaces_over_the_entry_budget_are_covered_without_losing_the_baseline() {
        let host = TestHost::new("capture-starve");
        let root = host.workspace("proj");
        host.set_project_root("p", &root);
        let store = host.store().unwrap();
        let now = Utc::now();
        let now_ms = now.timestamp_millis();
        store.window_started(&window("cutex.a", "t1", now_ms - 60_000, Some(now_ms - 1_000))).unwrap();
        // Old files (the baseline) fill more than one budget; the member's
        // file sorts last.
        for index in 0..12 {
            write(&root, &format!("outputs/a{index:02}.txt"), b"old", now_ms - 3_600_000);
        }
        write(&root, "outputs/z/new.txt", b"new", now_ms - 30_000);
        let tight = Budget { max_entries: 5, max_time: Duration::from_secs(5) };
        let mut captured = Vec::new();
        let mut baselined = 0;
        for _ in 0..6 {
            let report = scan_project(&host, &store, "p", now, tight).unwrap();
            captured.extend(report.captured);
            baselined += report.baselined;
            if !report.budget_hit && store.meta("captureScannedAt:p").unwrap().is_some() {
                break;
            }
        }
        assert_eq!(captured, vec!["outputs/z/new.txt"]);
        assert_eq!(baselined, 12);
        // Only a scan over the whole workspace ends the baseline.
        assert!(store.meta("captureScannedAt:p").unwrap().is_some());
        assert!(summaries(&store).iter().all(|item| !item.name.starts_with("outputs/a")));
        // Later edits anywhere are captured.
        write(&root, "outputs/a00.txt", b"edited", now_ms - 2_000);
        let mut later = Vec::new();
        for _ in 0..6 {
            let report = scan_project(&host, &store, "p", now, tight).unwrap();
            later.extend(report.captured);
            if !report.budget_hit {
                break;
            }
        }
        assert_eq!(later, vec!["outputs/a00.txt"]);
    }

    #[test]
    fn a_new_workspace_root_starts_with_a_fresh_baseline() {
        let host = TestHost::new("capture-root-change");
        let old_root = host.workspace("old");
        let new_root = host.workspace("new");
        host.set_project_root("p", &old_root);
        let store = host.store().unwrap();
        let now = Utc::now();
        let now_ms = now.timestamp_millis();
        store.window_started(&window("cutex.a", "t1", now_ms - 60_000, Some(now_ms - 1_000))).unwrap();
        write(&old_root, "reports/r.md", b"old root", now_ms - 30_000);
        write(&old_root, "reports/only-old.md", b"only old", now_ms - 30_000);
        let first = scan_project(&host, &store, "p", now, Budget::default()).unwrap();
        assert_eq!(sorted(first.captured), vec!["reports/only-old.md", "reports/r.md"]);
        assert_eq!(store.meta("captureRoot:p").unwrap(), Some(paths::canonical(&old_root).unwrap().display().to_string()));

        // The owner points the project at another directory: same-named and
        // new files there, some written inside the member's turn window.
        write(&new_root, "reports/r.md", b"new root", now_ms - 30_000);
        write(&new_root, "reports/only-new.md", b"only new", now_ms - 2_000);
        write(&new_root, "results/d.csv", b"a\n", now_ms - 3_600_000);
        host.set_project_root("p", &new_root);
        let switched = scan_project(&host, &store, "p", now, Budget::default()).unwrap();
        assert!(switched.captured.is_empty(), "{:?}", switched.captured);
        assert_eq!(switched.baselined, 3);
        assert_eq!(
            sorted(store.capture_files("p").unwrap().into_keys().collect()),
            vec!["reports/only-new.md", "reports/r.md", "results/d.csv"]
        );
        assert_eq!(store.meta("captureRoot:p").unwrap(), Some(paths::canonical(&new_root).unwrap().display().to_string()));
        assert!(store.meta("captureScannedAt:p").unwrap().is_some());
        assert_eq!(store.meta("captureBaselineBefore:p").unwrap(), None);
        assert_eq!(named(&store, "reports/r.md").latest_version, 1);

        // A real change in the new root afterwards is captured.
        let later = now + chrono::Duration::seconds(10);
        store.window_started(&window("cutex.a", "t2", now_ms + 2_000, Some(now_ms + 8_000))).unwrap();
        write(&new_root, "reports/r.md", b"new root v2", now_ms + 5_000);
        let next = scan_project(&host, &store, "p", later, Budget::default()).unwrap();
        assert_eq!(next.captured, vec!["reports/r.md"]);
        let report = named(&store, "reports/r.md");
        assert_eq!(report.latest_version, 2);
        assert_eq!(report.latest.producer.turn_id.as_deref(), Some("t2"));
        assert_eq!(report.latest.source_path, paths::canonical(&new_root.join("reports").join("r.md")).unwrap().display().to_string());
        // Old versions keep their old absolute source path.
        let unchanged = scan_project(&host, &store, "p", later, Budget::default()).unwrap();
        assert!(unchanged.captured.is_empty());
    }

    #[test]
    fn a_rebaseline_pass_over_the_budget_keeps_its_cutoff() {
        let host = TestHost::new("capture-root-budget");
        let old_root = host.workspace("old");
        let new_root = host.workspace("new");
        host.set_project_root("p", &old_root);
        let store = host.store().unwrap();
        let now = Utc::now();
        let now_ms = now.timestamp_millis();
        store.window_started(&window("cutex.a", "t1", now_ms - 60_000, Some(now_ms - 1_000))).unwrap();
        write(&old_root, "outputs/x.txt", b"x", now_ms - 3_600_000);
        scan_project(&host, &store, "p", now, Budget::default()).unwrap();
        for index in 0..8 {
            write(&new_root, &format!("outputs/a{index:02}.txt"), b"a", now_ms - 30_000);
        }
        host.set_project_root("p", &new_root);
        let tight = Budget { max_entries: 4, max_time: Duration::from_secs(5) };
        let mut captured = Vec::new();
        let mut baselined = 0;
        for step in 0..6 {
            let at = now + chrono::Duration::seconds(step);
            let report = scan_project(&host, &store, "p", at, tight).unwrap();
            captured.extend(report.captured);
            baselined += report.baselined;
            if !report.budget_hit && store.meta("captureScannedAt:p").unwrap().is_some() {
                break;
            }
            // The cutoff of the pass stays the first scan's time.
            assert_eq!(store.meta("captureBaselineBefore:p").unwrap(), Some(now_ms.to_string()));
        }
        assert!(captured.is_empty(), "{captured:?}");
        assert_eq!(baselined, 8);
        assert_eq!(store.meta("captureBaselineBefore:p").unwrap(), None);
    }

    #[test]
    fn clearing_the_workspace_root_drops_the_capture_state() {
        let host = TestHost::new("capture-root-clear");
        let root = host.workspace("proj");
        host.set_project_root("p", &root);
        let store = host.store().unwrap();
        let now = Utc::now();
        let now_ms = now.timestamp_millis();
        store.window_started(&window("cutex.a", "t1", now_ms - 60_000, Some(now_ms - 1_000))).unwrap();
        write(&root, "reports/r.md", b"v1", now_ms - 30_000);
        assert_eq!(scan_project(&host, &store, "p", now, Budget::default()).unwrap().captured, vec!["reports/r.md"]);

        host.roots.borrow_mut().remove("p");
        assert_eq!(scan_project(&host, &store, "p", now, Budget::default()).unwrap(), ScanReport::default());
        assert!(store.capture_files("p").unwrap().is_empty());
        assert_eq!(store.meta("captureRoot:p").unwrap().as_deref(), Some(""));
        assert_eq!(store.meta("captureScannedAt:p").unwrap(), None);
        // Again without a root: nothing more to do.
        assert_eq!(scan_project(&host, &store, "p", now, Budget::default()).unwrap(), ScanReport::default());
        assert_eq!(store.meta("captureRoot:p").unwrap().as_deref(), Some(""));

        // The root comes back (even the same directory): what changed there
        // in between is the new baseline, not a capture.
        write(&root, "reports/r.md", b"v2 while unmanaged", now_ms - 20_000);
        host.set_project_root("p", &root);
        let back = scan_project(&host, &store, "p", now, Budget::default()).unwrap();
        assert!(back.captured.is_empty(), "{:?}", back.captured);
        assert_eq!(back.baselined, 1);
        assert_eq!(named(&store, "reports/r.md").latest_version, 1);
        assert_eq!(store.meta("captureRoot:p").unwrap(), Some(paths::canonical(&root).unwrap().display().to_string()));
    }

    #[test]
    fn state_without_a_recorded_root_adopts_the_current_root() {
        let host = TestHost::new("capture-root-legacy");
        let root = host.workspace("proj");
        host.set_project_root("p", &root);
        let store = host.store().unwrap();
        let now = Utc::now();
        let now_ms = now.timestamp_millis();
        store.window_started(&window("cutex.a", "t1", now_ms - 60_000, Some(now_ms - 1_000))).unwrap();
        write(&root, "reports/r.md", b"v1", now_ms - 30_000);
        write(&root, "reports/s.md", b"s", now_ms - 30_000);
        scan_project(&host, &store, "p", now, Budget::default()).unwrap();
        // A store written before the root was recorded.
        store.set_meta("captureRoot:p", None).unwrap();
        write(&root, "reports/r.md", b"v2", now_ms - 20_000);
        let upgraded = scan_project(&host, &store, "p", now, Budget::default()).unwrap();
        assert_eq!((upgraded.captured, upgraded.baselined), (vec!["reports/r.md".to_string()], 0));
        assert_eq!(named(&store, "reports/r.md").latest_version, 2);
        assert_eq!(store.meta("captureRoot:p").unwrap(), Some(paths::canonical(&root).unwrap().display().to_string()));
        assert!(store.meta("captureScannedAt:p").unwrap().is_some());

        // A project without a root and without any state keeps the first-scan
        // behaviour once it gets one: member files of its turn windows count.
        let fresh = host.workspace("fresh");
        let windows_q = TurnWindow { project_id: Some("q".into()), ..window("cutex.b", "t9", now_ms - 60_000, Some(now_ms - 1_000)) };
        store.window_started(&windows_q).unwrap();
        assert_eq!(scan_project(&host, &store, "q", now, Budget::default()).unwrap(), ScanReport::default());
        assert_eq!(store.meta("captureRoot:q").unwrap(), None);
        write(&fresh, "reports/q.md", b"q", now_ms - 30_000);
        write(&fresh, "reports/older.md", b"o", now_ms - 3_600_000);
        host.set_project_root("q", &fresh);
        let first = scan_project(&host, &store, "q", now, Budget::default()).unwrap();
        assert_eq!((first.captured, first.baselined), (vec!["reports/q.md".to_string()], 1));
    }
}
