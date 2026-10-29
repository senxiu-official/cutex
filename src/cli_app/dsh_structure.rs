//! Structural operations on DSH agents: owner delete (state moved into the
//! delete recovery directory), history revert (`cutex/session/history/revert`)
//! and fork (`cutex/session/fork`). Same Management contracts as Codex/Claude.
//!
//! DSH 0.2.1-rc.3 has no ACP fork, rewind or delete
//! (docs/development/r265-dsh-history-notes.md §3). Both history operations are
//! built on the native JSONL log by the pinned adapter's one-shot
//! `session-ops.mjs`:
//!
//! - revert: with the runtime stopped, the agent's own log is cut after the end
//!   of the last kept turn (same native session, same kept turn IDs) and a
//!   fresh timeline is bound to it; cutex swaps both in (recoverable intent),
//!   keeps the previous log and timeline in `runtime/history-reverts/<action>/`
//!   and restarts a runtime that was online. Conversation only: DSH keeps no
//!   workspace file checkpoints, so files are never restored (`rewindFiles` is
//!   refused).
//! - fork: a closed prefix of the source log (through a completed turn) is
//!   copied under a new native ID into the NEW agent's own DSH home, with its
//!   attachments; the source runtime, online or not, is not touched. The fork
//!   keeps the source Profile, model, effort and pinned DSH deployment.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{ensure, Context};
use cutex::management::control_plane::HumanManagementPrincipal;
use cutex::management::v2::session_fork::{default_fork_name, SessionForkRequest};
use cutex::management::v2::user_input::UserInputExecutionError;
use cutex::runtime::dsh::{AcpFlavor, DshSession};
use cutex::session::model::{CutexSessionRecord, CutexSessionRuntimeBackend};
use serde_json::{json, Value};

const OPS_FILE: &str = "session-ops.mjs";
const OPS_TIMEOUT: Duration = Duration::from_secs(300);
/// A revert result lists every kept and removed turn ID (~50 bytes each).
const OPS_MAX_RESULT: usize = 64 * 1024 * 1024;
const OPS_MAX_STDERR: usize = 64 * 1024;
const REVERT_INTENT: &str = cutex::runtime::dsh::REVERT_INTENT_FILE;
pub(crate) const REVERT_METHOD: &str = "dsh/thread/revert";

fn error(code: &str, message: impl Into<String>, retryable: bool, details: Value) -> UserInputExecutionError {
    UserInputExecutionError {
        stage: "route".into(),
        code: code.into(),
        message: message.into(),
        retryable,
        details,
        outcome_unknown: false,
    }
}

/// A refusal of the adapter's session operation, with its stable code.
#[derive(Debug)]
struct OpsRefusal {
    code: String,
    message: String,
}
impl std::fmt::Display for OpsRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for OpsRefusal {}

/// Whether the session's pinned DSH runtime ships the structural operations.
pub(crate) fn ops_supported(session: &DshSession) -> bool {
    !session.spike && session.adapter.with_file_name(OPS_FILE).is_file()
}

fn sessions_root(session: &DshSession) -> PathBuf {
    match session.flavor {
        AcpFlavor::Dsh => session.state_dir.join("home").join("sessions"),
        AcpFlavor::Kimi => cutex::runtime::kimi::sessions_root(session),
    }
}

fn timeline(session: &DshSession) -> PathBuf {
    session.state_dir.join("timeline.sqlite")
}

/// Run one `session-ops.mjs` operation with an explicit private config and a
/// cleared environment; returns its result object or an [`OpsRefusal`].
fn run_ops(session: &DshSession, config: Value) -> anyhow::Result<Value> {
    run_ops_within(session, config, OPS_TIMEOUT)
}

fn run_ops_within(session: &DshSession, config: Value, timeout: Duration) -> anyhow::Result<Value> {
    let script = session.adapter.with_file_name(OPS_FILE);
    ensure!(script.is_file(), "the agent's DSH runtime predates session operations; install the current dsh-runtime component");
    let file = session.state_dir.join(format!("session-ops.{}.json", uuid::Uuid::new_v4().simple()));
    cutex::config::atomic::write_private_pretty_json_atomic(&file, &config, "DSH session operation")?;
    let outcome = (|| -> anyhow::Result<Value> {
        let mut command = std::process::Command::new(&session.node);
        // Secrets travel only in this short-lived child's environment.
        command.arg(&script).env_clear().env("DSH_CUTEX_SESSION_OPS_CONFIG", &file)
            .env("DSH_CUTEX_REDACT", serde_json::to_string(&redaction_secrets(session))?).current_dir(&session.state_dir)
            .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
        if let Some(sources) = redaction_sources(session) {
            command.env("KIMI_CUTEX_REDACT_SOURCES", sources);
        }
        for key in ["PATH", "Path", "SystemRoot", "WINDIR", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        let child = command.spawn().context("starting the DSH session operation")?;
        let output = wait_for_ops(child, timeout, OPS_MAX_RESULT)?;
        let text = String::from_utf8_lossy(&output.stdout);
        let line = text.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or_default();
        let value: Value = serde_json::from_str(line).with_context(|| {
            format!("the DSH session operation gave no result ({}): {}", output.status,
                String::from_utf8_lossy(&output.stderr).chars().take(500).collect::<String>())
        })?;
        if value["ok"] != true {
            return Err(OpsRefusal {
                code: value["code"].as_str().unwrap_or("session_operation_failed").into(),
                message: value["message"].as_str().unwrap_or("DSH session operation failed").into(),
            }.into());
        }
        Ok(value)
    })();
    let _ = std::fs::remove_file(&file);
    outcome
}

/// Wait for a session operation while both pipes are drained concurrently:
/// its result (kept / removed turn IDs of a long history) may be far larger
/// than a pipe buffer, and a child blocked on a full pipe never exits. The
/// result is bounded (`max_result`; above it the child is killed), stderr
/// keeps its first `OPS_MAX_STDERR` bytes and drains the rest. On timeout
/// the child is killed and reaped.
fn wait_for_ops(mut child: std::process::Child, timeout: Duration, max_result: usize) -> anyhow::Result<std::process::Output> {
    use std::io::Read;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};
    fn drain(mut pipe: impl Read + Send + 'static, keep: usize, overflow: Arc<AtomicBool>) -> mpsc::Receiver<std::io::Result<Vec<u8>>> {
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut kept = Vec::new();
            let mut buffer = vec![0u8; 64 * 1024];
            let result = loop {
                match pipe.read(&mut buffer) {
                    Ok(0) => break Ok(kept),
                    Ok(read) => {
                        let room = keep.saturating_sub(kept.len());
                        kept.extend_from_slice(&buffer[..read.min(room)]);
                        if read > room {
                            overflow.store(true, Ordering::SeqCst);
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) => break Err(error),
                }
            };
            let _ = sender.send(result);
        });
        receiver
    }
    let reap = |child: &mut std::process::Child| {
        let _ = child.kill();
        let _ = child.wait();
    };
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        reap(&mut child);
        anyhow::bail!("the DSH session operation has no output pipes");
    };
    let too_large = Arc::new(AtomicBool::new(false));
    let stdout = drain(stdout, max_result, too_large.clone());
    let stderr = drain(stderr, OPS_MAX_STDERR, Arc::new(AtomicBool::new(false)));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if too_large.load(Ordering::SeqCst) {
            reap(&mut child);
            anyhow::bail!("the DSH session operation's result exceeds {max_result} bytes");
        }
        if Instant::now() >= deadline {
            reap(&mut child);
            anyhow::bail!("the DSH session operation timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // The pipes close with the child; a leftover holder of them does not
    // keep this past the deadline.
    let collect = |receiver: mpsc::Receiver<std::io::Result<Vec<u8>>>| -> anyhow::Result<Vec<u8>> {
        let wait = deadline.saturating_duration_since(Instant::now()).max(Duration::from_secs(1));
        Ok(receiver.recv_timeout(wait).context("the DSH session operation's output did not end")??)
    };
    let stdout = collect(stdout)?;
    ensure!(!too_large.load(Ordering::SeqCst), "the DSH session operation's result exceeds {max_result} bytes");
    let stderr = collect(stderr)?;
    Ok(std::process::Output { status, stdout, stderr })
}

fn secret_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    ["TOKEN", "SECRET", "PASSWORD", "API_KEY"].iter().any(|part| upper.contains(part))
}

/// What the runtime's own projection redacts (`server.mjs` secrets): the
/// provider key and the secret-named values of the agent's last launch. A
/// timeline session-ops pre-projects must not keep them either, nor may the
/// offline history reader return them (`session_history::dsh_history_reader`).
pub(crate) fn redaction_secrets(session: &DshSession) -> Vec<String> {
    let mut secrets = Vec::new();
    // The key the agent launches with: a stored profile key (r265, C3's
    // resolver through the agent's frozen binding) or its env variable.
    let record = cutex::session::store::load_cutex_session_store().ok().and_then(|store| {
        store.sessions.into_values().find(|record| record.dsh_session.as_ref().is_some_and(|dsh| dsh.state_dir == session.state_dir))
    });
    // Retired keys of the profile too: a log written before a rotation.
    secrets.extend(record.as_ref().map(cutex::runtime::dsh::retired_credentials_for_redaction).unwrap_or_default());
    if let Some(value) = record.as_ref().and_then(cutex::runtime::dsh::credential_for_redaction) {
        secrets.push(value);
    } else if !session.api_key_env.is_empty() && session.api_key_env != cutex::profiles::stored_keys::DSH_STORED_KEY_ENV {
        if let Ok(value) = std::env::var(&session.api_key_env) {
            secrets.push(value);
        }
    }
    if let Ok(bytes) = std::fs::read(session.state_dir.join("launch.json")) {
        if let Ok(launch) = serde_json::from_slice::<Value>(&bytes) {
            secrets.extend(launch["token"].as_str().map(str::to_owned));
            for (name, value) in launch["env"].as_object().into_iter().flatten() {
                if secret_name(name) {
                    secrets.extend(value.as_str().map(str::to_owned));
                }
            }
            for server in launch["mcpServers"].as_array().into_iter().flatten() {
                for entry in server["env"].as_array().into_iter().flatten() {
                    if entry["name"].as_str().is_some_and(secret_name) {
                        secrets.extend(entry["value"].as_str().map(str::to_owned));
                    }
                }
            }
        }
    }
    secrets.retain(|secret| !secret.trim().is_empty());
    secrets.sort();
    secrets.dedup();
    secrets
}

/// A Kimi account agent's redaction sources for a reader or session operation
/// (`KIMI_CUTEX_REDACT_SOURCES`): the agent's linked `credentials/` (current
/// tokens, read by the reader itself) and the account's fingerprint ledger
/// (rotated ones). Paths only; `None` for other agents.
pub(crate) fn redaction_sources(session: &DshSession) -> Option<String> {
    if session.flavor != cutex::runtime::dsh::AcpFlavor::Kimi {
        return None;
    }
    let record = cutex::session::store::load_cutex_session_store().ok().and_then(|store| {
        store.sessions.into_values().find(|record| record.dsh_session.as_ref().is_some_and(|dsh| dsh.state_dir == session.state_dir))
    })?;
    let account = cutex::runtime::kimi::credential::bound_account(&record)?;
    let paths = cutex::runtime::kimi::redaction::RedactionPaths {
        credentials_dir: Some(cutex::runtime::kimi::kimi_home(session).join("credentials")).filter(|dir| dir.exists()).or_else(|| Some(account.home.join("credentials"))),
        ledger: cutex::runtime::kimi::redaction::ledger_path(&account.account_id).ok(),
    };
    serde_json::to_string(&paths).ok()
}

fn refusal_code(error: &anyhow::Error) -> Option<&str> {
    error.downcast_ref::<OpsRefusal>().map(|refusal| refusal.code.as_str())
}

// ---------------------------------------------------------------- delete

/// `cutex/session/delete`: everything a DSH agent owns lives in its private
/// state directory (timeline, native DSH home with sessions and attachments,
/// launch files). Moved whole into the recovery directory; the runtime was
/// stopped by the caller. Returns whether anything was moved.
pub(crate) fn move_state(record: &CutexSessionRecord, directory: &Path) -> anyhow::Result<bool> {
    let Some(session) = &record.dsh_session else { return Ok(false) };
    if !session.state_dir.exists() {
        return Ok(false);
    }
    let expected = cutex::config::paths::runtime_dir()?.join(session.flavor.state_root()).join(&record.cutex_session_id);
    ensure!(session.state_dir == expected, "the DSH state directory is not the agent's own ({})", session.state_dir.display());
    finish_pending_revert(session)?;
    move_dir(&session.state_dir, &directory.join("dsh-state"))
        .with_context(|| format!("moving the DSH state {}", session.state_dir.display()))?;
    Ok(true)
}

/// Rename, or copy and remove across file systems. Never follows symlinks.
fn move_dir(from: &Path, to: &Path) -> anyhow::Result<()> {
    ensure!(!to.exists(), "{} already exists", to.display());
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(rename_error) => {
            copy_tree(from, to, false).with_context(|| format!("rename failed ({rename_error}); copy failed"))?;
            std::fs::remove_dir_all(from)?;
            Ok(())
        }
    }
}

/// Recursive copy of regular files and directories; symlinks are skipped.
/// `link` tries a hard link first (attachments are content-addressed and
/// immutable, so sharing their inode is safe).
fn copy_tree(from: &Path, to: &Path, link: bool) -> anyhow::Result<u64> {
    let mut bytes = 0;
    std::fs::create_dir_all(to)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(to, std::fs::Permissions::from_mode(0o700))?;
    }
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            bytes += copy_tree(&entry.path(), &target, link)?;
        } else if kind.is_file() {
            if target.exists() {
                continue;
            }
            if !(link && std::fs::hard_link(entry.path(), &target).is_ok()) {
                std::fs::copy(entry.path(), &target)?;
            }
            bytes += entry.metadata()?.len();
        }
    }
    Ok(bytes)
}

// ---------------------------------------------------------------- revert

/// The swap of a prepared revert, persisted before the first rename so a
/// crash in between is finished on the next structural operation or start.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
struct RevertIntent {
    action_id: String,
    moves: Vec<(PathBuf, PathBuf)>,
    done: usize,
}

fn intent_path(session: &DshSession) -> PathBuf {
    session.state_dir.join(REVERT_INTENT)
}

#[cfg(test)]
thread_local! {
    /// Fault injection: fail the swap after this many moves / fail the
    /// final receipt write of a committed revert.
    static FAIL_SWAP_AFTER: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    static FAIL_FINAL_RECEIPT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn run_intent(path: &Path, mut intent: RevertIntent) -> anyhow::Result<()> {
    while intent.done < intent.moves.len() {
        #[cfg(test)]
        if FAIL_SWAP_AFTER.with(|fail| fail.get()) == Some(intent.done) {
            anyhow::bail!("injected swap failure after {} moves", intent.done);
        }
        let (from, to) = &intent.moves[intent.done];
        if from.exists() {
            std::fs::rename(from, to).with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
        } else {
            ensure!(to.exists(), "revert step lost both {} and {}", from.display(), to.display());
        }
        intent.done += 1;
        cutex::config::atomic::write_private_pretty_json_atomic(path, &intent, "DSH revert intent")?;
    }
    std::fs::remove_file(path)?;
    Ok(())
}

/// Finish a revert swap a crash interrupted. Only renames; idempotent.
pub(crate) fn finish_pending_revert(session: &DshSession) -> anyhow::Result<()> {
    let path = intent_path(session);
    let intent: RevertIntent = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("unreadable DSH revert intent")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    run_intent(&path, intent)
}

fn revert_failure(code: &str, error: anyhow::Error) -> UserInputExecutionError {
    let code = match refusal_code(&error) {
        Some(refusal @ ("turn_not_found" | "turn_in_progress")) => refusal.to_string(),
        Some("unsupported_history") => "history_revert_unsupported".into(),
        _ => code.into(),
    };
    let retryable = code == "turn_in_progress";
    error_with(&code, format!("{error:#}"), retryable)
}

fn error_with(code: &str, message: String, retryable: bool) -> UserInputExecutionError {
    error(code, message, retryable, json!({}))
}

fn runtime_online(id: &str) -> anyhow::Result<CutexSessionRecord> {
    let current = cutex::runtime::dsh::start(id)?;
    let config = cutex::config::store::load_codez_config_checked()?;
    let binding = current.app_server_runtime.as_ref().context("DSH runtime binding missing after start")?;
    let agent = current.current_runtime_agent_id.as_deref().context("DSH runtime agent missing after start")?;
    super::app_server_runtime::connect_runtime(&config, &current, binding, agent)?;
    Ok(current)
}

fn load_record(id: &str) -> anyhow::Result<CutexSessionRecord> {
    cutex::session::store::load_cutex_session_store()?
        .sessions
        .get(id)
        .cloned()
        .context("the agent does not exist on this host")
}

/// `cutex/session/history/revert` for a DSH agent (`sessionHistoryRevertResult`,
/// `native.method` = `dsh/thread/revert`).
pub(crate) fn revert(cutex_session_id: &str, before: &str, rewind_files: bool) -> Result<Value, UserInputExecutionError> {
    if rewind_files {
        return Err(error_with(
            "rewind_files_unsupported",
            "DSH keeps no workspace file checkpoints: revert restores the conversation only; files stay as they are".into(),
            false,
        ));
    }
    let lane = super::app_server_user_input::session_input_lock(cutex_session_id)
        .map_err(|e| revert_failure("history_revert_failed", e))?;
    let reservation = lane.reserve().map_err(|e| error_with("history_revert_failed", format!("{e:?}"), true))?;
    let record = load_record(cutex_session_id).map_err(|e| revert_failure("session_not_found", e))?;
    let session = record.dsh_session.clone().ok_or_else(|| error_with("history_revert_unsupported", "not a DSH agent".into(), false))?;
    if !ops_supported(&session) {
        return Err(error_with("history_revert_unsupported",
            "the agent's DSH runtime predates history revert; install the current dsh-runtime component for new agents".into(), false));
    }
    let Some(native) = session.native_session_id.clone() else {
        return Err(error_with("turn_not_found", format!("turn {before} is not in this agent's history (it never ran)"), false));
    };
    if let Some(turn) = super::app_server_runtime::runtime_manager().status(cutex_session_id).ok().flatten().and_then(|s| s.active_turn_id) {
        // Same wording as the Claude adapter, which the desktop recognises.
        return Err(error(
            "history_revert_failed",
            "interrupt the active turn before changing session history",
            true,
            json!({ "activeTurnId": turn }),
        ));
    }
    finish_pending_revert(&session).map_err(|e| revert_failure("history_revert_failed", e))?;
    let was_online = super::management_archive::archive_runtime_looks_online(&record)
        .map_err(|e| revert_failure("history_revert_failed", e))?;
    if was_online {
        super::management_context::stop_runtime_before_archive(cutex_session_id)?;
    }
    let committed = commit_revert(&record, &session, &native, before);
    drop(reservation);
    let restart = |was_online: bool| -> (bool, Option<String>) {
        if !was_online {
            return (false, None);
        }
        match runtime_online(cutex_session_id) {
            Ok(_) => (true, None),
            Err(e) => (false, Some(format!("{e:#}"))),
        }
    };
    let Committed { receipt, warnings: mut committed_warnings } = match committed {
        Ok(committed) => committed,
        Err(failure) => {
            // Nothing was swapped: bring the runtime back as it was. A swap
            // that stopped half way is finished first by the next start.
            if !intent_path(&session).exists() {
                let _ = restart(was_online);
            }
            return Err(failure);
        }
    };
    // From here on the history HAS changed: every later problem is a warning
    // of a committed revert, never a failure the caller could retry.
    let (restarted, restart_error) = restart(was_online);
    let current = load_record(cutex_session_id).unwrap_or_else(|_| record.clone());
    let mut warnings: Vec<Value> = restart_error.iter().map(|message| json!({
        "code": "dsh_runtime_restart_failed",
        "message": "History changed successfully, but the DSH runtime could not restart. Start it explicitly; do not repeat the history operation.",
        "runtimeState": "failed", "restartError": message,
    })).collect();
    warnings.append(&mut committed_warnings);
    let state = if restart_error.is_some() { "failed" } else if restarted { "online" } else { "offline" };
    let removed = receipt["removedTurnIds"].clone();
    // Bootstrap's latestPlan / latestTokenUsage and the agent list's
    // lastOutput / lastToolCall must not describe a removed turn.
    let removed_ids: Vec<&str> = removed.as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    if let Err(error) = cutex::management::v2::claude_plan::forget_removed_turns(cutex_session_id, &removed_ids) {
        eprintln!("DSH latest plan for {cutex_session_id} not reset after revert: {error:#}");
    }
    if let Err(error) = cutex::management::v2::claude_token_usage::forget_removed_turns(cutex_session_id, &removed_ids) {
        eprintln!("DSH latest token usage for {cutex_session_id} not reset after revert: {error:#}");
    }
    super::session_history::forget_reverted_activity(cutex_session_id, &removed_ids);
    Ok(json!({
        "cutexSessionId": cutex_session_id,
        "threadId": session.thread_id,
        "revertedBeforeTurnId": before,
        "removedTurns": removed.as_array().map(Vec::len).unwrap_or(0),
        "removedTurnIds": removed,
        "live": was_online,
        "runtimeRestarted": restarted,
        "runtimeState": state,
        "warnings": warnings,
        "runtimePid": if restarted { current.runtime_pid.map(Value::from).unwrap_or(Value::Null) } else { Value::Null },
        "runtimeGeneration": current.runtime_generation,
        "actionId": receipt["actionId"],
        "backupPath": receipt["backupPath"],
        "receiptPath": receipt["receiptPath"],
        "native": { "method": REVERT_METHOD, "result": receipt },
    }))
}

/// A committed revert: its receipt (as far as it could be recorded) and the
/// problems after the commit point.
struct Committed {
    receipt: Value,
    warnings: Vec<Value>,
}

/// Prepare, then swap log and timeline (the commit point: the last rename of
/// the intent). Before it, a failure changed nothing (or, inside the swap, is
/// an unknown outcome the next start / structural operation finishes); after
/// it, bookkeeping problems are warnings of a committed revert.
fn commit_revert(record: &CutexSessionRecord, session: &DshSession, native: &str, before: &str) -> Result<Committed, UserInputExecutionError> {
    let action_id = format!("revert-{}", uuid::Uuid::new_v4());
    let directory = cutex::config::paths::runtime_dir()
        .map(|dir| dir.join("history-reverts").join(&action_id))
        .map_err(|e| revert_failure("history_revert_failed", e))?;
    // Staged next to the files it replaces: every swap is a same-directory-tree rename.
    let staging = session.state_dir.join(format!(".{action_id}"));
    let run = || -> anyhow::Result<(Value, PathBuf, PathBuf)> {
        std::fs::create_dir_all(&directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        }
        #[cfg(windows)]
        cutex::platform::private_fs::secure_directory(&directory)?;
        let previous = staging.join("previous");
        let prepared = run_ops(session, json!({
            "op": "revertPrepare", "database": timeline(session), "sessionsRoot": sessions_root(session),
            "sessionId": native, "cwd": record.cwd, "beforeTurnId": before, "stagingDirectory": staging,
            "installRoot": session.install_root,
        }))?;
        std::fs::create_dir_all(&previous)?;
        let path = |key: &str| prepared[key].as_str().map(PathBuf::from).with_context(|| format!("revert preparation omitted {key}"));
        let (log, staged_log, staged_db) = (path("logPath")?, path("stagedLog")?, path("stagedDatabase")?);
        ensure!(log.starts_with(sessions_root(session)) && staged_log.starts_with(&staging) && staged_db.starts_with(&staging),
            "revert preparation returned foreign paths");
        // The previous conversation stays recoverable: its log (copied, the
        // runtime is stopped) and its timeline (moved below).
        let backup = directory.join(log.file_name().context("native log name")?);
        std::fs::copy(&log, &backup)?;
        let db = timeline(session);
        let mut moves = Vec::new();
        for suffix in ["-wal", "-shm", ""] {
            let from = PathBuf::from(format!("{}{suffix}", db.display()));
            if from.exists() {
                moves.push((from, previous.join(format!("timeline.sqlite{suffix}"))));
            }
        }
        for suffix in ["", "-wal", "-shm"] {
            let from = PathBuf::from(format!("{}{suffix}", staged_db.display()));
            if from.exists() {
                moves.push((from, PathBuf::from(format!("{}{suffix}", db.display()))));
            }
        }
        // Last: the log replaced in one rename. Until then the old log is in
        // place, and the fresh timeline would just re-project it.
        moves.push((staged_log, log.clone()));
        let receipt_path = directory.join("receipt.json");
        let mut receipt = json!({
            "actionId": action_id, "cutexSessionId": record.cutex_session_id, "threadId": session.thread_id,
            "nativeSessionId": native, "revertedBeforeTurnId": before,
            "removedTurnIds": prepared["removedTurnIds"], "keptTurnIds": prepared["keptTurnIds"],
            "throughSeq": prepared["throughSeq"], "filesRestored": false,
            "backupPath": backup, "receiptPath": receipt_path, "recoveryDirectory": directory,
            "status": "swapping", "startedAt": chrono::Utc::now().to_rfc3339(),
        });
        cutex::config::atomic::write_private_pretty_json_atomic(&receipt_path, &receipt, "DSH revert receipt")?;
        let intent = RevertIntent { action_id: action_id.clone(), moves, done: 0 };
        let intent_file = intent_path(session);
        cutex::config::atomic::write_private_pretty_json_atomic(&intent_file, &intent, "DSH revert intent")?;
        run_intent(&intent_file, intent)?;
        Ok((receipt, receipt_path, previous))
    };
    let (mut receipt, receipt_path, previous) = run().map_err(|e| {
        if intent_path(session).exists() {
            // The swap began and stopped part way: the history is neither
            // the old nor the new one until the intent is finished (next
            // start or structural operation). Never "nothing happened".
            let mut failure = revert_failure("history_revert_failed", e);
            failure.message = format!("the history swap was interrupted and is finished when the agent next starts: {}", failure.message);
            failure.retryable = false;
            failure.outcome_unknown = true;
            return failure;
        }
        // Nothing was swapped; the staged copy is no one's state.
        let _ = std::fs::remove_dir_all(&staging);
        revert_failure("history_revert_failed", e)
    })?;
    // Committed. The previous timeline joins the recovery copy (best effort:
    // otherwise it stays in the staging directory, named in the receipt).
    let mut warnings = Vec::new();
    receipt["status"] = json!("completed");
    match move_dir(&previous, &directory.join("previous-timeline")) {
        Ok(()) => {
            let _ = std::fs::remove_dir_all(&staging);
            receipt["previousTimeline"] = json!(directory.join("previous-timeline").join("timeline.sqlite"));
        }
        Err(error) => {
            eprintln!("DSH revert {action_id}: previous timeline left in {}: {error:#}", previous.display());
            receipt["previousTimeline"] = json!(previous.join("timeline.sqlite"));
        }
    }
    receipt["completedAt"] = json!(chrono::Utc::now().to_rfc3339());
    #[cfg(test)]
    let written = if FAIL_FINAL_RECEIPT.with(|fail| fail.get()) {
        Err(anyhow::anyhow!("injected final receipt write failure"))
    } else {
        cutex::config::atomic::write_private_pretty_json_atomic(&receipt_path, &receipt, "DSH revert receipt")
    };
    #[cfg(not(test))]
    let written = cutex::config::atomic::write_private_pretty_json_atomic(&receipt_path, &receipt, "DSH revert receipt");
    if let Err(error) = written {
        // The recovery copy (previous log and timeline) is in place; only
        // its receipt still says "swapping".
        eprintln!("DSH revert {action_id}: committed, but the receipt was not updated: {error:#}");
        warnings.push(json!({
            "code": "dsh_revert_receipt_not_saved",
            "message": format!("History changed successfully, but its recovery receipt could not be updated ({error:#}); the previous log and timeline are in {}. Do not repeat the history operation.", directory.display()),
        }));
    }
    Ok(Committed { receipt, warnings })
}

// ---------------------------------------------------------------- fork

fn fork_failure(error: anyhow::Error) -> UserInputExecutionError {
    match refusal_code(&error) {
        Some("turn_not_found") => error_with("turn_not_found", format!("{error:#}"), false),
        Some("turn_in_progress") => error_with("turn_in_progress", format!("{error:#}"), true),
        Some("unsupported_history") => error_with("session_fork_unsupported", format!("{error:#}"), false),
        _ => error_with("session_fork_failed", format!("session fork failed: {error:#}"), false),
    }
}

/// The child record: the source's Profile, model, effort, permissions,
/// groups and pinned DSH deployment, with its own identities and state.
fn fork_record(source: &CutexSessionRecord, id: &str, name: &str, thread: &str, native: Option<&str>, state_dir: &Path) -> anyhow::Result<CutexSessionRecord> {
    let binding = source.dsh_session.as_ref().context("DSH source missing")?;
    let mut record = CutexSessionRecord::new(id.into(), None, source.host_id.clone(), source.cwd.clone(), source.profile.clone())?;
    record.runtime_backend = binding.flavor.backend();
    record.agent_enabled = source.agent_enabled;
    record.registration_class = source.registration_class;
    record.agent_groups = source.agent_groups.clone();
    record.formal_agent_name = source.formal_agent_name.as_ref().map(|_| name.into());
    record.thread_name = Some(name.into());
    record.display_name_hint = Some(name.into());
    record.exposed_to_backend = true;
    record.provider_family = source.provider_family.clone();
    record.model_defaults = source.model_defaults.clone();
    record.reasoning_defaults = source.reasoning_defaults.clone();
    record.permission_defaults = source.permission_defaults.clone();
    record.approval_policy = source.approval_policy.clone();
    record.sandbox_mode = source.sandbox_mode.clone();
    record.sandbox_network_access = source.sandbox_network_access;
    record.sandbox_writable_roots = source.sandbox_writable_roots.clone();
    record.dsh_session = Some(DshSession {
        thread_id: thread.into(),
        native_session_id: native.map(str::to_owned),
        state_dir: state_dir.to_path_buf(),
        patch: match binding.flavor {
            AcpFlavor::Dsh => state_dir.join("profile.patch.yml"),
            AcpFlavor::Kimi => state_dir.join("home").join("kimi").join("config.toml"),
        },
        ..binding.clone()
    });
    // The fork launches like its source: same profile, variant and
    // credential (a stored-key DSH agent cannot start without its binding).
    record.profile_binding = source.profile_binding.as_ref().map(|bound| {
        bound.for_fork(
            record.model_defaults.clone().or_else(|| Some(binding.model.clone())),
            record.reasoning_defaults.clone().or_else(|| binding.reasoning_effort.clone()),
            Some(binding.deployment_version.clone()),
        )
    });
    cutex::session::fork::inherit_fork_avatar(source, &mut record);
    record.stamp_created_by_at_creation(Some(&cutex::session::model::SessionCreatedBy::Owner))?;
    Ok(record)
}

/// `cutex/session/fork` for a DSH source (facts for `session_fork::fork_result`).
pub(crate) fn fork(
    principal: &HumanManagementPrincipal,
    source: &CutexSessionRecord,
    request: &SessionForkRequest,
) -> Result<Value, UserInputExecutionError> {
    let unsupported = |message: &str| error_with("session_fork_unsupported", message.into(), false);
    if source.is_retired() {
        return Err(unsupported("restore the archived agent before forking it"));
    }
    if !cutex::runtime::lifecycle::cutex_session_host_is_local(&source.host_id, &cutex::platform::host::current_host_name()) {
        return Err(unsupported("a session can only be forked on its own host"));
    }
    let session = source.dsh_session.clone().ok_or_else(|| unsupported("not a DSH agent"))?;
    if !ops_supported(&session) {
        return Err(unsupported("the agent's DSH runtime predates fork; install the current dsh-runtime component"));
    }
    if let Some(profile) = request.profile.as_deref() {
        let same = source.profile.as_deref() == Some(profile)
            || cutex::profiles::settings_api::load_store().ok()
                .and_then(|store| cutex::profiles::lookup::find_account(&store, profile).ok().flatten().map(|account| account.name.clone()))
                .is_some_and(|name| source.profile.as_deref() == Some(name.as_str()));
        if !same {
            return Err(error("session_fork_unsupported", "a DSH fork keeps the source Profile and runtime", false,
                json!({"unsupported": true, "runtime": session.flavor.runtime_name(), "feature": "forkProfile"})));
        }
    }
    let mut created_dir: Option<PathBuf> = None;
    let mut run = || -> anyhow::Result<Value> {
        let path = cutex::session::store::cutex_sessions_path()?;
        let store = cutex::session::store::load_cutex_session_store()?;
        let taken = |name: &str| store.sessions.values().any(|r| {
            r.formal_agent_name.as_deref() == Some(name) || cutex::session::metadata::cutex_session_presented_name(r) == name
        });
        let name = request.name.clone().unwrap_or_else(|| {
            default_fork_name(&cutex::session::metadata::cutex_session_presented_name(source), taken)
        });
        ensure!(!taken(&name), "an agent named {name} already exists");
        let id = format!("cutex.{}", uuid::Uuid::new_v4());
        let thread = uuid::Uuid::new_v4().to_string();
        let native = uuid::Uuid::new_v4().to_string();
        let state_dir = cutex::config::paths::runtime_dir()?.join(session.flavor.state_root()).join(&id);
        ensure!(!state_dir.exists(), "fork state directory already exists");
        std::fs::create_dir_all(&state_dir)?;
        created_dir = Some(state_dir.clone());
        #[cfg(windows)]
        cutex::platform::private_fs::secure_directory(&state_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let child_home = state_dir.join("home");
        let facts = match &session.native_session_id {
            Some(source_native) => run_ops(&session, json!({
                "op": "forkCopy", "database": timeline(&session), "sessionsRoot": sessions_root(&session),
                "sessionId": source_native, "cwd": source.cwd, "afterTurnId": request.after_turn_id,
                "targetSessionsRoot": match session.flavor {
                    AcpFlavor::Dsh => child_home.join("sessions"),
                    AcpFlavor::Kimi => child_home.join("kimi").join("sessions"),
                }, "newSessionId": native,
                "targetDatabase": state_dir.join("timeline.sqlite"), "installRoot": session.install_root,
            }))?,
            None => {
                if let Some(turn) = &request.after_turn_id {
                    return Err(OpsRefusal { code: "turn_not_found".into(), message: format!("turn {turn} is not in this agent's history") }.into());
                }
                json!({ "forkedThroughTurnId": null, "nativeSessionId": null, "historyBytes": 0 })
            }
        };
        let child_native = facts["nativeSessionId"].as_str();
        if child_native.is_some() && session.flavor == AcpFlavor::Dsh {
            // Images in the copied history reference DSH's content-addressed store.
            let attachments = session.state_dir.join("home").join("dsh").join("attachments");
            if attachments.is_dir() {
                copy_tree(&attachments, &child_home.join("dsh").join("attachments"), true)?;
            }
        }
        let record = fork_record(source, &id, &name, &thread, child_native, &state_dir)?;
        let history_bytes = facts["historyBytes"].as_u64().unwrap_or(0);
        let origin = cutex::session::fork::ForkOrigin {
            source_session_id: source.cutex_session_id.clone(),
            source_profile: source.profile.clone(),
            target_profile: record.profile.clone(),
            history_bytes,
            cold_cache: false,
        };
        cutex::session::fork::save(&id, &origin)?;
        cutex::session::store::with_locked_session_rows(&path, |rows| {
            ensure!(!rows.sessions().any(|(_, r)| r.formal_agent_name.as_deref() == Some(&name)), "agent name changed during fork");
            rows.insert_session(id.clone(), record.clone());
            rows.commit().map(drop)
        })?;
        created_dir = None;
        if source.agent_enabled {
            super::claude_structure::import_and_assign(principal, source, &id, &name)
                .with_context(|| format!("fork {id} exists; roster import or project assignment incomplete"))?;
        }
        cutex::notify::session::session(&thread, cutex::notify::session::Change::Set(
            cutex::config::store::load_codez_config_checked()?.new_session_notification))?;
        Ok(json!({
            "newCutexSessionId": id, "sourceThreadId": session.thread_id, "threadId": thread,
            "forkedThroughTurnId": facts["forkedThroughTurnId"], "name": name,
            "profile": record.profile, "sourceProfile": source.profile, "historyBytes": history_bytes,
            "coldCache": false, "forkBoundaryNote": origin.boundary_note(), "warnings": [],
        }))
    };
    let result = run();
    if let Some(dir) = created_dir {
        // Nothing durable references it yet.
        let _ = std::fs::remove_dir_all(dir);
    }
    result.map_err(fork_failure)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cutex-dsh-structure-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn session(state_dir: &Path, adapter: &Path) -> DshSession {
        serde_json::from_value(json!({
            "thread_id": "facade", "state_dir": state_dir, "node": "/node", "adapter": adapter,
            "dsh_cli": "/dsh", "patch": state_dir.join("profile.patch.yml"), "install_root": "/install",
            "native_session_id": "native", "spike": false,
        }))
        .unwrap()
    }

    #[test]
    fn interrupted_revert_swaps_are_finished_in_order_and_never_repeated() {
        let dir = temp("intent");
        let state = dir.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let session = session(&state, &dir.join("src/server.mjs"));
        let (db, staged, recovery) = (state.join("timeline.sqlite"), dir.join("staged.sqlite"), dir.join("old.sqlite"));
        std::fs::write(&db, "old").unwrap();
        std::fs::write(&staged, "new").unwrap();
        let intent = RevertIntent {
            action_id: "a".into(),
            moves: vec![(db.clone(), recovery.clone()), (staged.clone(), db.clone())],
            done: 0,
        };
        // Crash after the first rename, before `done` was persisted.
        cutex::config::atomic::write_private_pretty_json_atomic(&intent_path(&session), &intent, "test").unwrap();
        std::fs::rename(&db, &recovery).unwrap();
        finish_pending_revert(&session).unwrap();
        assert_eq!(std::fs::read_to_string(&db).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(&recovery).unwrap(), "old");
        assert!(!intent_path(&session).exists());
        finish_pending_revert(&session).unwrap();
        // Crash after `done` was persisted: the new timeline is never moved away again.
        std::fs::write(&staged, "newer").unwrap();
        let resumed = RevertIntent { done: 1, moves: vec![(db.clone(), recovery.clone()), (staged.clone(), db.clone())], action_id: "b".into() };
        std::fs::remove_file(&recovery).unwrap();
        std::fs::rename(&db, &recovery).unwrap();
        cutex::config::atomic::write_private_pretty_json_atomic(&intent_path(&session), &resumed, "test").unwrap();
        finish_pending_revert(&session).unwrap();
        assert_eq!(std::fs::read_to_string(&db).unwrap(), "newer");
        // Both ends lost is reported, not guessed.
        let lost = RevertIntent { done: 0, moves: vec![(dir.join("x"), dir.join("y"))], action_id: "c".into() };
        cutex::config::atomic::write_private_pretty_json_atomic(&intent_path(&session), &lost, "test").unwrap();
        assert!(finish_pending_revert(&session).unwrap_err().to_string().contains("lost both"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn pre_projection_redacts_what_the_runtime_redacts() {
        let dir = temp("redact");
        let session = session(&dir, &dir.join("src/server.mjs"));
        assert!(redaction_secrets(&session).is_empty(), "no launch yet");
        std::fs::write(dir.join("launch.json"), json!({
            "token": "launch-token-value", "env": {"HOME": "/home", "SOME_API_KEY": "env-key", "PATH": "/bin"},
            "mcpServers": [{"name": "cutex", "env": [{"name": "CUTEX_AGENT_BUS_TOKEN", "value": "bus-token"}, {"name": "CUTEX_AGENT_ID", "value": "dsh.1"}]}],
        }).to_string()).unwrap();
        assert_eq!(redaction_secrets(&session), ["bus-token", "env-key", "launch-token-value"]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn ops_support_follows_the_pinned_adapter_files() {
        let dir = temp("support");
        let adapter = dir.join("src/server.mjs");
        std::fs::create_dir_all(adapter.parent().unwrap()).unwrap();
        std::fs::write(&adapter, "").unwrap();
        let mut session = session(&dir, &adapter);
        assert!(!ops_supported(&session), "an r264 runtime has no session-ops.mjs");
        std::fs::write(adapter.with_file_name(OPS_FILE), "").unwrap();
        assert!(ops_supported(&session));
        session.spike = true;
        assert!(!ops_supported(&session), "debug spikes never get structural operations");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn ops_refusals_keep_their_codes_and_other_failures_are_generic() {
        let dir = temp("ops");
        let adapter = dir.join("src/server.mjs");
        std::fs::create_dir_all(adapter.parent().unwrap()).unwrap();
        let mut session = session(&dir, &adapter);
        let missing = run_ops(&session, json!({})).unwrap_err();
        assert!(format!("{missing:#}").contains("predates session operations"));
        // A stand-in adapter: /bin/sh prints the given line.
        std::fs::write(adapter.with_file_name(OPS_FILE), "printf '%s\\n' '{\"ok\":false,\"code\":\"turn_not_found\",\"message\":\"turn t is not in this agent'\\''s history\"}'").unwrap();
        session.node = PathBuf::from("/bin/sh");
        if session.node.is_file() {
            let refused = run_ops(&session, json!({"op": "revertPrepare"})).unwrap_err();
            assert_eq!(refusal_code(&refused), Some("turn_not_found"));
            assert_eq!(revert_failure("history_revert_failed", refused).code, "turn_not_found");
            assert_eq!(fork_failure(anyhow::anyhow!("boom")).code, "session_fork_failed");
            assert!(std::fs::read_dir(&dir).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().starts_with("session-ops.")),
                "the private config file is removed");
        }
        std::fs::remove_dir_all(dir).ok();
    }

    /// A result far larger than a pipe buffer (1 MiB, plus 1 MiB of stderr)
    /// is read while the helper runs; before, the helper blocked on the full
    /// pipe and the wait ran into the timeout.
    #[cfg(unix)]
    #[test]
    fn ops_results_larger_than_a_pipe_are_read_while_waiting() {
        let dir = temp("large");
        let adapter = dir.join("src/server.mjs");
        std::fs::create_dir_all(adapter.parent().unwrap()).unwrap();
        let mut session = session(&dir, &adapter);
        session.node = PathBuf::from("/bin/sh");
        std::fs::write(adapter.with_file_name(OPS_FILE), concat!(
            "head -c 1048576 /dev/zero | tr '\\0' e >&2\n",
            "printf '{\"ok\":true,\"removedTurnIds\":[\"'\n",
            "head -c 1048576 /dev/zero | tr '\\0' t\n",
            "printf '\"]}\\n'\n",
        )).unwrap();
        let started = Instant::now();
        let value = run_ops_within(&session, json!({"op": "revertPrepare"}), Duration::from_secs(20)).unwrap();
        assert_eq!(value["removedTurnIds"][0].as_str().unwrap().len(), 1 << 20);
        assert!(started.elapsed() < Duration::from_secs(15), "read concurrently, not at the timeout");
        // Bounded: a result above the limit kills the helper instead of growing without end.
        let child = std::process::Command::new("/bin/sh").args(["-c", "head -c 1048576 /dev/zero"])
            .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
        let error = wait_for_ops(child, Duration::from_secs(20), 1024).unwrap_err();
        assert!(format!("{error:#}").contains("exceeds 1024 bytes"), "{error:#}");
        std::fs::remove_dir_all(dir).ok();
    }

    /// The timeout still holds: a hung helper is killed and reaped.
    #[cfg(unix)]
    #[test]
    fn a_hung_ops_helper_is_killed_and_reaped_at_the_timeout() {
        let dir = temp("hung");
        let pid_file = dir.join("pid");
        let child = std::process::Command::new("/bin/sh").arg("-c").arg(format!("echo $$ > '{}'; exec sleep 30", pid_file.display()))
            .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
        let started = Instant::now();
        let error = wait_for_ops(child, Duration::from_millis(500), OPS_MAX_RESULT).unwrap_err();
        assert!(format!("{error:#}").contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(10));
        let pid = std::fs::read_to_string(&pid_file).unwrap().trim().to_string();
        assert!(!Path::new(&format!("/proc/{pid}")).exists() || cfg!(not(target_os = "linux")), "the helper was reaped");
        std::fs::remove_dir_all(dir).ok();
    }

    /// A stopped DSH agent with a stand-in `session-ops.mjs` (/bin/sh) that
    /// stages a new log and timeline like `revertPrepare`.
    #[cfg(unix)]
    fn revert_fixture(root: &Path) -> (String, PathBuf, PathBuf) {
        let id = format!("dsh.revert-commit-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        let state = cutex::config::paths::runtime_dir().unwrap().join("dsh").join(&id);
        let log = state.join("home/sessions/native.jsonl");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, "old log").unwrap();
        std::fs::write(state.join("timeline.sqlite"), "old timeline").unwrap();
        let adapter = root.join("component/src/server.mjs");
        std::fs::create_dir_all(adapter.parent().unwrap()).unwrap();
        std::fs::write(&adapter, "").unwrap();
        std::fs::write(adapter.with_file_name(OPS_FILE), r#"
staging=$(sed -n 's/.*"stagingDirectory": *"\([^"]*\)".*/\1/p' "$DSH_CUTEX_SESSION_OPS_CONFIG")
mkdir -p "$staging"
printf 'new log' > "$staging/native.jsonl"
printf 'new timeline' > "$staging/timeline.sqlite"
printf '{"ok":true,"logPath":"%s","stagedLog":"%s","stagedDatabase":"%s","removedTurnIds":["native:turn:2"],"keptTurnIds":["native:turn:1"],"throughSeq":5}\n' "$PWD/home/sessions/native.jsonl" "$staging/native.jsonl" "$staging/timeline.sqlite"
"#).unwrap();
        let mut dsh = session(&state, &adapter);
        dsh.node = PathBuf::from("/bin/sh");
        let mut record = CutexSessionRecord::new(id.clone(), None, cutex::platform::host::current_host_name(), root.display().to_string(), None).unwrap();
        record.runtime_backend = CutexSessionRuntimeBackend::Dsh;
        record.dsh_session = Some(dsh);
        let mut store = cutex::session::store::load_cutex_session_store().unwrap();
        store.sessions.insert(id.clone(), record);
        cutex::session::store::save_cutex_session_store(&store).unwrap();
        (id, state, log)
    }

    /// Fault injection on the final receipt write: the swap is committed, so
    /// the revert is reported as done (with a warning), the post-commit
    /// cleanup (C-7: plan / token usage / activity previews of removed turns)
    /// still runs and the dispatcher can emit `historyReverted`. Before, the
    /// failure surfaced as an ordinary `history_revert_failed` with
    /// `outcome_unknown: false`, inviting a retry of an applied revert.
    #[cfg(unix)]
    #[test]
    fn a_receipt_failure_after_the_swap_reports_the_committed_revert() {
        use sha2::Digest;
        let home = crate::cli_app::test_home::IsolatedTestHome::new("dsh-revert-commit").unwrap();
        let (id, state, log) = revert_fixture(home.root());
        // A latest plan of the turn the revert removes.
        let plan = cutex::config::paths::runtime_dir().unwrap().join("management-v2/claude-plan")
            .join(format!("{}.json", &format!("{:x}", sha2::Sha256::digest(id.as_bytes()))[..32]));
        std::fs::create_dir_all(plan.parent().unwrap()).unwrap();
        std::fs::write(&plan, json!({"cutexSessionId": id, "latestPlan": {"threadId": "facade", "turnId": "native:turn:2", "plan": []}}).to_string()).unwrap();
        assert!(cutex::management::v2::claude_plan::latest(&id, "facade").unwrap().is_some());
        FAIL_FINAL_RECEIPT.with(|fail| fail.set(true));
        let result = revert(&id, "native:turn:2", false);
        FAIL_FINAL_RECEIPT.with(|fail| fail.set(false));
        let result = result.expect("a committed revert is reported as committed");
        assert_eq!(result["removedTurnIds"], json!(["native:turn:2"]));
        assert_eq!(result["runtimeState"], "offline");
        let warnings = result["warnings"].as_array().unwrap();
        assert!(warnings.iter().any(|warning| warning["code"] == "dsh_revert_receipt_not_saved"), "{warnings:?}");
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "new log");
        assert_eq!(std::fs::read_to_string(state.join("timeline.sqlite")).unwrap(), "new timeline");
        assert!(!intent_path(&load_record(&id).unwrap().dsh_session.unwrap()).exists());
        assert_eq!(cutex::management::v2::claude_plan::latest(&id, "facade").unwrap(), None, "post-commit cleanup ran");
        let receipt: Value = serde_json::from_slice(&std::fs::read(result["receiptPath"].as_str().unwrap()).unwrap()).unwrap();
        assert_eq!(receipt["status"], "swapping", "only the receipt update failed");
        // The response is a valid revert result (warnings included).
        cutex::management::v2::contract_validation::validate_cutex_response(&json!({
            "contractVersion": 2, "requestId": "r", "cutexSessionId": id,
            "cutex": { "method": "cutex/session/history/revert", "result": result },
        })).unwrap();
    }

    /// A swap that stopped part way is an unknown outcome (finished by the
    /// next start), never "nothing changed".
    #[cfg(unix)]
    #[test]
    fn a_swap_interrupted_part_way_is_an_unknown_outcome() {
        let home = crate::cli_app::test_home::IsolatedTestHome::new("dsh-revert-partial").unwrap();
        let (id, state, log) = revert_fixture(home.root());
        FAIL_SWAP_AFTER.with(|fail| fail.set(Some(1)));
        let result = revert(&id, "native:turn:2", false);
        FAIL_SWAP_AFTER.with(|fail| fail.set(None));
        let error = result.unwrap_err();
        assert!(error.outcome_unknown && !error.retryable, "{error:?}");
        let session = load_record(&id).unwrap().dsh_session.unwrap();
        assert!(intent_path(&session).exists());
        finish_pending_revert(&session).unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "new log");
        assert_eq!(std::fs::read_to_string(state.join("timeline.sqlite")).unwrap(), "new timeline");
    }

    #[test]
    fn move_and_copy_never_follow_links() {
        let dir = temp("move");
        let from = dir.join("from");
        std::fs::create_dir_all(from.join("home/sessions")).unwrap();
        std::fs::write(from.join("home/sessions/log"), "x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc", from.join("home/link")).unwrap();
        let copied = dir.join("copy");
        assert_eq!(copy_tree(&from, &copied, true).unwrap(), 1);
        assert!(!copied.join("home/link").exists());
        move_dir(&from, &dir.join("moved")).unwrap();
        assert!(!from.exists() && dir.join("moved/home/sessions/log").is_file());
        assert!(move_dir(&copied, &dir.join("moved")).is_err(), "never merges into an existing target");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn fork_records_keep_the_binding_but_get_their_own_identities() {
        let dir = temp("record");
        let mut source = CutexSessionRecord::new("cutex.src".into(), None, "host".into(), "/work".into(), Some("DeepSeek".into())).unwrap();
        source.runtime_backend = CutexSessionRuntimeBackend::Dsh;
        source.formal_agent_name = Some("src".into());
        source.agent_enabled = true;
        source.model_defaults = Some("deepseek-v4-pro".into());
        source.reasoning_defaults = Some("high".into());
        let mut binding = session(&dir.join("src-state"), &dir.join("src/server.mjs"));
        binding.model = "deepseek-v4-pro".into();
        binding.reasoning_effort = Some("high".into());
        binding.api_key_env = "DEEPSEEK_API_KEY".into();
        source.dsh_session = Some(binding.clone());
        let bound_version = binding.deployment_version.clone();
        let state = dir.join("child-state");
        let child = fork_record(&source, "cutex.child", "src (fork)", "thread-2", Some("native-2"), &state).unwrap();
        let bound = child.dsh_session.as_ref().unwrap();
        assert_eq!((bound.thread_id.as_str(), bound.native_session_id.as_deref()), ("thread-2", Some("native-2")));
        assert_eq!((bound.state_dir.as_path(), bound.patch.as_path()), (state.as_path(), state.join("profile.patch.yml").as_path()));
        assert_eq!((&bound.model, &bound.reasoning_effort, &bound.api_key_env, &bound.adapter), (&binding.model, &binding.reasoning_effort, &binding.api_key_env, &binding.adapter));
        assert_eq!(child.runtime_backend, CutexSessionRuntimeBackend::Dsh);
        assert_eq!((child.profile.as_deref(), child.formal_agent_name.as_deref()), (Some("DeepSeek"), Some("src (fork)")));
        assert_eq!(child.app_server_thread_id(), Some("thread-2"));
        assert!(child.app_server_runtime.is_none() && child.runtime_pid.is_none() && child.runtime_generation == 0);
        assert!(child.profile_binding.is_none(), "a legacy source has no binding; neither has its fork");
        let fresh = fork_record(&source, "cutex.child2", "x", "thread-3", None, &state).unwrap();
        assert_eq!(fresh.dsh_session.unwrap().native_session_id, None, "no completed turn: a fresh native session");
        // C-1: a bound source (r265 variant agent) gives its fork the same
        // profile, variant and credential; model/effort are the fork's launch values.
        let bound: cutex::profiles::runtime_variants::ProfileLaunchBinding = serde_json::from_value(json!({
            "profileId": "p-1", "variantId": "dsh", "runtime": "dsh",
            "credential": {"source": "env", "envName": "DEEPSEEK_API_KEY"},
            "model": "deepseek-flash", "reasoningEffort": "low", "runtimePackageVersion": "0.2.1-rc.3",
            "boundAt": "2026-10-27T00:00:00+00:00",
        })).unwrap();
        source.profile_binding = Some(bound.clone());
        let child = fork_record(&source, "cutex.child3", "y", "thread-4", Some("native-4"), &state).unwrap();
        let forked = child.profile_binding.expect("fork keeps the launch binding");
        assert_eq!((forked.profile_id.as_str(), forked.variant_id.as_str(), forked.runtime.as_str()), ("p-1", "dsh", "dsh"));
        assert_eq!(forked.credential, bound.credential);
        assert_eq!((forked.model.as_deref(), forked.reasoning_effort.as_deref()), (Some("deepseek-v4-pro"), Some("high")));
        assert_eq!(forked.runtime_package_version.as_deref(), Some(bound_version.as_str()));
        assert_ne!(forked.bound_at, bound.bound_at);
        std::fs::remove_dir_all(dir).ok();
    }
}
