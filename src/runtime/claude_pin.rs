//! Re-pinning Claude sessions to another adapter runtime.
//!
//! A Claude session copies the selected `ClaudeDeployment` at creation, so a
//! new adapter runtime (`cutex human claude install`) only reaches sessions
//! created afterwards. `repin` moves existing sessions:
//!
//! - offline sessions (no reachable adapter, no live adapter pid; the same
//!   test `Start` uses before it spawns one) change at once, under the
//!   session's controller lock;
//! - online sessions are never restarted or interrupted: the target is saved
//!   as `pending-deployment.json` in the session's state dir and applied by
//!   the next `Start` that spawns a fresh adapter.
//!
//! Only the executables change (python, adapter source, Claude CLI, TUI). The
//! thread id, state dir (adapter SQLite with the native session mapping) and
//! the Claude config home stay, so the next start resumes the same native
//! Claude session; a target with another `claude_home` is refused. The
//! previous pin is reported and saved as `previous-deployment.json`, which is
//! itself a valid `--deployment` target for undoing the move.
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::claude::{ClaudeDeployment, ClaudeSession};
use crate::config::atomic::write_private_pretty_json_atomic;
use crate::session::model::CutexSessionRecord;
use crate::session::store::{
    cutex_sessions_path, save_locked_session_store, with_locked_session_store,
};

pub const PENDING_FILE: &str = "pending-deployment.json";
pub const PREVIOUS_FILE: &str = "previous-deployment.json";

/// A pin change saved for the next fresh adapter start.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingPin {
    /// The pin the session had when the change was requested; a pending
    /// change whose session moved meanwhile is stale and dropped.
    pub previous: ClaudeDeployment,
    pub deployment: ClaudeDeployment,
    pub requested_at: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RepinOutcome {
    /// Already on the target (a stale pending change, if any, is dropped).
    Unchanged,
    /// Offline: the session now uses the target.
    Applied,
    /// Online: the target applies at the next fresh adapter start.
    Deferred,
    /// Not changed; see `reason`.
    Skipped,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepinReport {
    pub cutex_session_id: String,
    pub name: Option<String>,
    pub outcome: RepinOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<ClaudeDeployment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_file: Option<PathBuf>,
}

/// The session's next pin for `target`, or None when nothing would change.
/// The Claude config home is part of the conversation identity (native
/// session transcripts live there), so a different one is refused.
pub fn plan_pin(
    current: &ClaudeDeployment,
    target: &ClaudeDeployment,
) -> anyhow::Result<Option<ClaudeDeployment>> {
    ensure!(
        same_path(&current.claude_home, &target.claude_home),
        "target uses another Claude config home ({}; session has {}); re-pinning would lose the native Claude conversation",
        target.claude_home.display(),
        current.claude_home.display()
    );
    let next = ClaudeDeployment {
        claude_home: current.claude_home.clone(),
        ..target.clone()
    };
    Ok((next != *current).then_some(next))
}

/// The target as it applies to one session. A session on an added Claude
/// account (`claude_accounts`) keeps its account home whatever home the
/// target deployment names (a deployment always names the default home);
/// a session on the default home keeps the strict check of [`plan_pin`].
pub fn for_session(current: &ClaudeDeployment, target: &ClaudeDeployment) -> ClaudeDeployment {
    let registry = super::claude_accounts::load_cached();
    if super::claude_accounts::account_id_for_home(&registry, &current.claude_home)
        == super::claude_accounts::DEFAULT_ACCOUNT_ID
    {
        return target.clone();
    }
    ClaudeDeployment { claude_home: current.claude_home.clone(), ..target.clone() }
}

fn same_path(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        let norm = |p: &Path| p.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase();
        norm(a) == norm(b)
    } else {
        a == b
    }
}

/// True when `Start` would spawn a fresh adapter for this session: none is
/// reachable and the recorded adapter pid (if any) is not running.
fn adapter_offline(record: &CutexSessionRecord, session: &ClaudeSession) -> bool {
    super::claude::connect(session).is_err()
        && !record
            .runtime_pid
            .is_some_and(crate::platform::process::process_is_running)
}

fn session_name(record: &CutexSessionRecord) -> Option<String> {
    record
        .formal_agent_name
        .clone()
        .or_else(|| record.thread_name.clone())
}

fn skipped(record: &CutexSessionRecord, reason: String) -> RepinReport {
    RepinReport {
        cutex_session_id: record.cutex_session_id.clone(),
        name: session_name(record),
        outcome: RepinOutcome::Skipped,
        reason: Some(reason),
        previous: record.claude_session.as_ref().map(|s| s.deployment.clone()),
        previous_file: None,
    }
}

fn remove_if_present(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
    }
}

/// Schema 2 is a one-way writer fence introduced by claude-r8. A pin changes
/// code, not conversation data, so it cannot undo that migration safely.
fn check_state_compatibility(session: &ClaudeSession, target: &ClaudeDeployment) -> anyhow::Result<()> {
    let path = session.state_dir.join("state.sqlite3");
    if !path.exists() {
        return Ok(());
    }
    let db = rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("read Claude state schema at {} before re-pinning", path.display()))?;
    let schema: Option<String> = {
        use rusqlite::OptionalExtension;
        db.query_row("SELECT value FROM meta WHERE key='schema'", [], |row| row.get(0)).optional()?
    };
    let Some(schema) = schema else {
        return Ok(());
    };
    if schema == "1" {
        return Ok(());
    }
    let release = std::fs::read(target.adapter_source.join("src/cute_claude/features.json"))
        .ok().and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
        .and_then(|features| features["release"].as_str().map(str::to_owned));
    let supports_two = release.as_deref().and_then(|release| release.strip_prefix("claude-r"))
        .and_then(|number| number.parse::<u64>().ok()).is_some_and(|number| number >= 8);
    ensure!(schema == "2" && supports_two,
        "Claude state schema {schema} cannot be re-pinned to this adapter (release {}); restore the pre-r8 state, session binding and native transcript backup together before re-pinning to r7 or earlier",
        release.as_deref().unwrap_or("unknown"));
    Ok(())
}

/// Commit `next` as the session's pin if its Claude binding is still
/// `expected`; saves the previous pin next to the adapter state.
fn commit_pin(
    id: &str,
    expected: &ClaudeSession,
    next: &ClaudeDeployment,
) -> anyhow::Result<PathBuf> {
    check_state_compatibility(expected, next)?;
    let previous_file = expected.state_dir.join(PREVIOUS_FILE);
    std::fs::create_dir_all(&expected.state_dir)?;
    write_private_pretty_json_atomic(&previous_file, &expected.deployment, "previous Claude deployment")?;
    let path = cutex_sessions_path()?;
    with_locked_session_store(&path, |store| {
        let current = store
            .sessions
            .get_mut(id)
            .context("session removed during re-pin")?;
        ensure!(
            current.claude_session.as_ref() == Some(expected),
            "Claude configuration changed during re-pin; retry"
        );
        current.claude_session.as_mut().unwrap().deployment = next.clone();
        current.updated_at = chrono::Utc::now().to_rfc3339();
        save_locked_session_store(&path, store)
    })?;
    remove_if_present(&expected.state_dir.join(PENDING_FILE))?;
    Ok(previous_file)
}

/// Move one local Claude session to `target` (see the module docs).
pub fn repin(id: &str, target: &ClaudeDeployment, dry_run: bool) -> anyhow::Result<RepinReport> {
    let initial = super::claude::record(id)?;
    let session = initial.claude_session.clone().unwrap();
    let next = match plan_pin(&session.deployment, &for_session(&session.deployment, target)) {
        Ok(next) => next,
        Err(error) => return Ok(skipped(&initial, format!("{error:#}"))),
    };
    let mut report = RepinReport {
        cutex_session_id: initial.cutex_session_id.clone(),
        name: session_name(&initial),
        outcome: RepinOutcome::Unchanged,
        reason: None,
        previous: Some(session.deployment.clone()),
        previous_file: None,
    };
    let Some(next) = next else {
        if !dry_run {
            remove_if_present(&session.state_dir.join(PENDING_FILE))?;
        }
        return Ok(report);
    };
    if let Err(error) = check_state_compatibility(&session, &next) {
        return Ok(skipped(&initial, format!("{error:#}")));
    }
    if dry_run {
        report.outcome = if adapter_offline(&initial, &session) {
            RepinOutcome::Applied
        } else {
            report.reason = Some("online: applies at the next start".into());
            RepinOutcome::Deferred
        };
        return Ok(report);
    }
    let _lock = match super::claude::lock(&session) {
        Ok(lock) => lock,
        Err(error) => return Ok(skipped(&initial, format!("{error:#}"))),
    };
    // Re-read under the controller lock: Start/Close may have finished.
    let record = super::claude::record(id)?;
    let session = record.claude_session.clone().unwrap();
    report.previous = Some(session.deployment.clone());
    let Some(next) = plan_pin(&session.deployment, &for_session(&session.deployment, target))? else {
        remove_if_present(&session.state_dir.join(PENDING_FILE))?;
        return Ok(report);
    };
    if let Err(error) = check_state_compatibility(&session, &next) {
        return Ok(skipped(&record, format!("{error:#}")));
    }
    if adapter_offline(&record, &session) {
        report.previous_file = Some(commit_pin(id, &session, &next)?);
        report.outcome = RepinOutcome::Applied;
    } else {
        write_private_pretty_json_atomic(
            &session.state_dir.join(PENDING_FILE),
            &PendingPin {
                previous: session.deployment.clone(),
                deployment: next,
                requested_at: chrono::Utc::now().to_rfc3339(),
            },
            "pending Claude deployment",
        )?;
        report.outcome = RepinOutcome::Deferred;
        report.reason = Some("online: applies at the next start".into());
    }
    Ok(report)
}

/// Every local Claude session (archived ones included: they are offline and
/// keep their pin through Restore), in id order.
pub fn repin_all(target: &ClaudeDeployment, dry_run: bool) -> anyhow::Result<Vec<RepinReport>> {
    let store = crate::session::store::load_cutex_session_store()?;
    let host = crate::platform::host::current_host_name();
    let mut reports = Vec::new();
    for (id, record) in &store.sessions {
        if record.claude_session.is_none() {
            continue;
        }
        if !crate::runtime::lifecycle::cutex_session_host_is_local(&record.host_id, &host) {
            reports.push(skipped(record, format!("remote host {}", record.host_id)));
            continue;
        }
        reports.push(match repin(id, target, dry_run) {
            Ok(report) => report,
            Err(error) => skipped(record, format!("{error:#}")),
        });
    }
    reports.sort_by(|a, b| a.cutex_session_id.cmp(&b.cutex_session_id));
    Ok(reports)
}

/// Called by `Start` under the controller lock before it reuses or spawns an
/// adapter. Applies a pending pin only when a fresh adapter will be spawned,
/// and returns the (possibly re-pinned) record.
pub fn apply_pending_at_start(
    record: CutexSessionRecord,
) -> anyhow::Result<CutexSessionRecord> {
    let session = record.claude_session.clone().context("Claude session missing")?;
    let pending_path = session.state_dir.join(PENDING_FILE);
    let raw = match std::fs::read(&pending_path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(record),
        Err(e) => return Err(e).with_context(|| format!("read {}", pending_path.display())),
    };
    if !adapter_offline(&record, &session) {
        return Ok(record);
    }
    let pending: PendingPin = match serde_json::from_slice(&raw) {
        Ok(pending) => pending,
        Err(error) => {
            eprintln!(
                "Claude pending pin {} is unreadable ({error}); kept the current runtime",
                pending_path.display()
            );
            return Ok(record);
        }
    };
    if pending.previous != session.deployment {
        eprintln!(
            "Claude pending pin for {} is stale (session was re-pinned since); dropped",
            record.cutex_session_id
        );
        remove_if_present(&pending_path)?;
        return Ok(record);
    }
    let next = match plan_pin(&session.deployment, &pending.deployment)
        .and_then(|next| {
            if let Some(next) = &next {
                next.validate()?;
                check_state_compatibility(&session, next)?;
            }
            Ok(next)
        }) {
        Ok(Some(next)) => next,
        Ok(None) => {
            remove_if_present(&pending_path)?;
            return Ok(record);
        }
        Err(error) => {
            eprintln!(
                "Claude pending pin for {} not applied ({error:#}); started on the current runtime",
                record.cutex_session_id
            );
            return Ok(record);
        }
    };
    commit_pin(&record.cutex_session_id, &session, &next)?;
    super::claude::record(&record.cutex_session_id)
}

/// The pending change for a session, if any (for status output).
pub fn pending(session: &ClaudeSession) -> Option<Value> {
    let raw = std::fs::read(session.state_dir.join(PENDING_FILE)).ok()?;
    let pending: PendingPin = serde_json::from_slice(&raw).ok()?;
    Some(json!({"deployment": pending.deployment, "requestedAt": pending.requested_at}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deployment(root: &str, home: &str) -> ClaudeDeployment {
        ClaudeDeployment {
            python: format!("/{root}/venv/python").into(),
            adapter_source: format!("/{root}/adapter").into(),
            claude_cli: "/claude".into(),
            claude_home: home.into(),
            tui_binary: "/tui".into(),
        }
    }

    #[test]
    fn plan_pin_changes_executables_and_keeps_the_claude_home() {
        let r3 = deployment("claude-r3", "/home/.claude");
        let r4 = deployment("claude-r4", "/home/.claude");
        assert_eq!(plan_pin(&r3, &r3).unwrap(), None);
        assert_eq!(plan_pin(&r3, &r4).unwrap(), Some(r4.clone()));
        // Undo is the same operation with the previous pin as the target.
        assert_eq!(plan_pin(&r4, &r3).unwrap(), Some(r3.clone()));
        let mut cli = r3.clone();
        cli.claude_cli = "/other-claude".into();
        assert_eq!(plan_pin(&r3, &cli).unwrap(), Some(cli));
    }

    #[test]
    fn plan_pin_refuses_another_claude_home() {
        let r3 = deployment("claude-r3", "/home/.claude");
        let elsewhere = deployment("claude-r4", "/other/.claude");
        let error = plan_pin(&r3, &elsewhere).unwrap_err().to_string();
        assert!(error.contains("Claude config home"), "{error}");
    }

    #[cfg(windows)]
    #[test]
    fn claude_home_comparison_ignores_windows_case_and_separators() {
        assert!(same_path(
            Path::new(r"C:\Users\x\.cutex-claude-compatible\.claude"),
            Path::new("c:/users/x/.cutex-claude-compatible/.claude/")
        ));
    }

    #[test]
    fn pending_pin_round_trips_and_rejects_unknown_fields() {
        let pin = PendingPin {
            previous: deployment("claude-r3", "/h"),
            deployment: deployment("claude-r4", "/h"),
            requested_at: "2026-10-08T00:00:00Z".into(),
        };
        let raw = serde_json::to_value(&pin).unwrap();
        assert_eq!(serde_json::from_value::<PendingPin>(raw.clone()).unwrap(), pin);
        let mut extra = raw;
        extra["surprise"] = json!(1);
        assert!(serde_json::from_value::<PendingPin>(extra).is_err());
    }
}
