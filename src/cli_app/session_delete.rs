//! `ManagementRequestContext::delete_session`: permanent delete of a cutex
//! session (Management `cutex/session/delete`, and the delete-empty step of
//! `cutex/session/retire`).
//!
//! Order: refuse referenced/busy sessions before any side effect, write a
//! recovery copy (record + rollouts) under
//! `runtime/session-deletes/<UTC stamp>-<id>/`, stop a running runtime,
//! delete the native thread (native `thread/delete` on a private app-server
//! on the session's native home; when that native cannot be started, the
//! rollout files are moved into the recovery directory instead), then remove
//! the durable record, its IM projection, its queued input and its roster row.
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use cutex::catalog::{CatalogEndpoint, CatalogError};
use cutex::management::v2::session_delete::SessionDeleteRequest;
use cutex::management::v2::user_input::UserInputExecutionError;
use cutex::session::model::CutexSessionRecord;
use serde_json::{json, Value};

const TURNS_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

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

fn failed(step: &str, error_text: impl std::fmt::Display) -> UserInputExecutionError {
    error(
        "session_delete_failed",
        format!("session delete failed while {step}: {error_text:#}"),
        true,
        json!({ "step": step }),
    )
}

/// Why a session is not "empty" (never received input), or `None` if it is.
/// Empty = a Codex session whose native thread has no turns and whose
/// Management input queue holds nothing. Claude sessions, sessions without a
/// bound thread and unreadable threads are never empty.
pub(crate) fn not_empty_reason(record: &CutexSessionRecord) -> Option<&'static str> {
    if record.claude_session.is_some() {
        return Some("unsupported");
    }
    let Some(thread) = record.codex_session_id.as_deref() else {
        return Some("noThread");
    };
    match cutex::management::v2::user_input::user_input_repository()
        .and_then(|queue| queue.snapshot(&record.cutex_session_id))
    {
        Ok(items) if items.is_empty() => {}
        Ok(_) => return Some("queuedInput"),
        Err(_) => return Some("unknown"),
    }
    // Cheap positive evidence first, so archiving a used offline session does
    // not start a native just to learn it is not empty.
    if super::session_history::native_home_for(Some(record))
        .ok()
        .and_then(|home| cutex::launch::native_history::current(&home, thread).ok())
        .is_some_and(|rollout| rollout_shows_turns(&rollout))
    {
        return Some("turns");
    }
    let params = json!({"threadId": thread, "sortDirection": "desc", "limit": 1, "itemsView": "summary"});
    let page = match super::app_server_runtime::runtime_manager().handle(&record.cutex_session_id) {
        Ok(handle) => handle
            .request_with_timeout("thread/turns/list", params, TURNS_PROBE_TIMEOUT)
            .map_err(anyhow::Error::from),
        Err(_) => isolated_endpoint(record)
            .and_then(|mut endpoint| Ok(endpoint.request("thread/turns/list", params)?)),
    };
    match page {
        Ok(page) => match page.get("data").and_then(Value::as_array) {
            Some(turns) if turns.is_empty() => None,
            Some(_) => Some("turns"),
            None => Some("unknown"),
        },
        Err(_) => Some("unknown"),
    }
}

/// A rollout that records a turn context or a model/user item has turns.
/// `false` only means "no evidence"; the native decides.
fn rollout_shows_turns(rollout: &Path) -> bool {
    const LARGE: u64 = 1024 * 1024;
    if std::fs::metadata(rollout).is_ok_and(|metadata| metadata.len() > LARGE) {
        return true;
    }
    std::fs::read_to_string(rollout).is_ok_and(|text| {
        text.contains(r#""type":"turn_context""#) || text.contains(r#""type":"response_item""#)
    })
}

#[cfg(any(target_os = "linux", windows))]
fn isolated_endpoint(record: &CutexSessionRecord) -> anyhow::Result<cutex::catalog::OwnedStdioEndpoint> {
    let home = super::session_history::native_home_for(Some(record))?;
    super::session_history::isolated_endpoint(Some(record), &home)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn isolated_endpoint(_: &CutexSessionRecord) -> anyhow::Result<cutex::catalog::OwnedStdioEndpoint> {
    anyhow::bail!("a local native runtime requires Linux or Windows")
}

fn load(id: &str) -> Result<(String, CutexSessionRecord), UserInputExecutionError> {
    let store = cutex::session::store::load_cutex_session_store()
        .map_err(|error| failed("loading the session store", error))?;
    let key = cutex::session::service::cutex_session_key_for_user_id_including_retired(&store, id)
        .ok_or_else(|| {
            error(
                "session_not_found",
                "the durable cutex session does not exist",
                false,
                json!({ "cutexSessionId": id }),
            )
        })?;
    let record = store.sessions[&key].clone();
    Ok((key, record))
}

/// Everything that would dangle after the delete (empty object: nothing).
fn references(key: &str) -> Result<Value, UserInputExecutionError> {
    let id = cutex::role_revision::CutexSessionId::new(key.to_string())
        .map_err(|_| failed("checking references", "invalid cutex session id"))?;
    let provider = cutex::agent_management::AgentManagementProvider::open_default()
        .map_err(|error| failed("opening agent management", error))?;
    let found = provider
        .session_delete_references(&id)
        .map_err(|error| failed("checking references", error))?;
    let services = cutex::services::store::ServiceStore::open_default()
        .map_err(|error| failed("opening the services registry", error))?
        .load()
        .map_err(|error| failed("reading the services registry", error.message))?
        .services
        .values()
        .filter(|service| service.owner_cutex_session_id.as_deref() == Some(key))
        .map(|service| service.slug.clone())
        .collect::<Vec<_>>();
    if !found.blocks_delete() && services.is_empty() {
        return Ok(Value::Null);
    }
    let mut details = serde_json::to_value(&found).unwrap_or_else(|_| json!({}));
    details["ownedServices"] = json!(services);
    Ok(details)
}

fn referenced_error(details: Value) -> UserInputExecutionError {
    error(
        "session_referenced",
        "this session is still referenced (Director, Operator or owned services); resolve those first",
        false,
        details,
    )
}

fn busy_error(message: &str, details: Value) -> UserInputExecutionError {
    error("session_busy", message, true, details)
}

/// `runtime/session-deletes/<UTC stamp>-<id>/`, private.
fn recovery_directory(key: &str) -> anyhow::Result<PathBuf> {
    let safe: String = key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .collect();
    let directory = cutex::config::paths::runtime_dir()?
        .join("session-deletes")
        .join(format!("{}-{safe}", chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ")));
    std::fs::create_dir_all(&directory)?;
    #[cfg(windows)]
    cutex::platform::private_fs::secure_directory(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(directory)
}

fn write_json(path: &Path, value: &Value) -> anyhow::Result<()> {
    cutex::config::atomic::write_private_pretty_json_atomic(path, value, "session delete recovery")
}

/// Copies of the thread's rollout (and the ancestors it references) under
/// `<directory>/rollouts/`. Missing history (never materialized) is not an
/// error. Returns the current rollout path, if any.
fn backup_rollouts(home: &Path, thread: &str, directory: &Path) -> anyhow::Result<Option<PathBuf>> {
    let Ok(source) = cutex::launch::native_history::current(home, thread) else {
        return Ok(None);
    };
    let target = directory.join("rollouts");
    std::fs::create_dir_all(&target)?;
    for (index, path) in cutex::launch::native_history::recovery_sources(home, &source)?
        .iter()
        .enumerate()
    {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("rollout-{index}.jsonl"));
        let copy = target.join(format!("{index}-{name}"));
        std::fs::copy(path, &copy)
            .with_context(|| format!("could not copy {}", path.display()))?;
        std::fs::OpenOptions::new().write(true).open(&copy)?.sync_all()?;
    }
    Ok(Some(source))
}

enum NativeOutcome {
    Deleted,
    Moved,
    Nothing,
}

impl NativeOutcome {
    fn label(&self) -> &'static str {
        match self {
            Self::Deleted => "thread/delete",
            Self::Moved => "storageMove",
            Self::Nothing => "none",
        }
    }
}

fn is_fork_reference(error: &CatalogError) -> bool {
    matches!(error, CatalogError::Rpc { error, .. } if error.message.contains("forked history still references"))
}

fn is_thread_not_found(error: &CatalogError) -> bool {
    matches!(error, CatalogError::Rpc { error, .. } if error.message.contains("thread not found"))
}

fn delete_native(record: &CutexSessionRecord, directory: &Path) -> Result<NativeOutcome, UserInputExecutionError> {
    let Some(thread) = record.codex_session_id.as_deref() else {
        return Ok(NativeOutcome::Nothing);
    };
    let home = super::session_history::native_home_for(Some(record))
        .map_err(|error| failed("resolving the native home", error))?;
    let rollout = backup_rollouts(&home, thread, directory)
        .map_err(|error| failed("saving the recovery copy", error))?;
    match isolated_endpoint(record) {
        Ok(mut endpoint) => match endpoint.request("thread/delete", json!({ "threadId": thread })) {
            Ok(_) => Ok(NativeOutcome::Deleted),
            Err(native) if is_fork_reference(&native) => Err(referenced_error(json!({
                "nativeForkReferences": true,
                "message": native.to_string(),
            }))),
            Err(native) if is_thread_not_found(&native) => Ok(NativeOutcome::Nothing),
            Err(native) => Err(failed("deleting the native thread", native)),
        },
        Err(spawn) => {
            // No native available: move this thread's own rollout into the
            // recovery directory (state DB rows stay; native lists skip a
            // thread whose rollout is gone).
            eprintln!("session delete: native unavailable for {thread} ({spawn:#}); moving storage");
            let Some(rollout) = rollout else {
                return Ok(NativeOutcome::Nothing);
            };
            let moved = directory.join("moved");
            std::fs::create_dir_all(&moved).map_err(|error| failed("moving storage", error))?;
            let target = moved.join(rollout.file_name().unwrap_or_default());
            std::fs::rename(&rollout, &target)
                .or_else(|_| std::fs::copy(&rollout, &target).and_then(|_| std::fs::remove_file(&rollout)))
                .map_err(|error| failed("moving storage", error))?;
            Ok(NativeOutcome::Moved)
        }
    }
}

/// Claude sessions keep their state in a private per-session directory.
fn move_claude_state(record: &CutexSessionRecord, directory: &Path) -> Result<NativeOutcome, UserInputExecutionError> {
    let Some(session) = &record.claude_session else {
        return Ok(NativeOutcome::Nothing);
    };
    if !session.state_dir.exists() {
        return Ok(NativeOutcome::Nothing);
    }
    std::fs::rename(&session.state_dir, directory.join("claude-state"))
        .map_err(|error| failed("moving the Claude session state", error))?;
    Ok(NativeOutcome::Moved)
}

pub(crate) fn delete_for_management(
    cutex_session_id: &str,
    request: &SessionDeleteRequest,
) -> Result<Value, UserInputExecutionError> {
    let _guard = super::management_context::lock_session_mutations()?;
    let (key, record) = load(cutex_session_id)?;
    if let Some(expected) = request.expected_revision {
        if expected != record.durable_revision() {
            return Err(super::management_context::session_revision_conflict(
                expected,
                record.durable_revision(),
            ));
        }
    }
    if !cutex::runtime::lifecycle::cutex_session_host_is_local(
        &record.host_id,
        &cutex::platform::host::current_host_name(),
    ) {
        return Err(error(
            "session_delete_unsupported",
            "a session can only be deleted on its own host",
            false,
            json!({ "hostId": record.host_id }),
        ));
    }
    if request.only_if_empty {
        if let Some(reason) = not_empty_reason(&record) {
            return Ok(json!({ "cutexSessionId": key, "deleted": false, "notEmptyReason": reason }));
        }
    }
    // Busy: a launch in progress or a running turn.
    if let Err(error) = super::management_context::auto_clear_dead_launch_claim(&key) {
        eprintln!("session delete: launch claim check failed for {key}: {error:#}");
    }
    let (_, record) = load(&key)?;
    if record.app_server_launch_claim_id.is_some() {
        return Err(busy_error("the runtime is being launched; retry when it settled", json!({ "launchInProgress": true })));
    }
    if let Some(turn) = super::app_server_runtime::runtime_manager()
        .status(&key)
        .ok()
        .flatten()
        .and_then(|status| status.active_turn_id)
    {
        return Err(busy_error(
            "a turn is running; stop it before deleting the session",
            json!({ "activeTurnId": turn }),
        ));
    }
    let references = references(&key)?;
    if !references.is_null() {
        return Err(referenced_error(references));
    }

    let directory = recovery_directory(&key).map_err(|error| failed("creating the recovery directory", error))?;
    let receipt_path = directory.join("receipt.json");
    let mut receipt = json!({
        "cutexSessionId": key,
        "threadId": record.app_server_thread_id(),
        "status": "prepared",
        "startedAt": chrono::Utc::now().to_rfc3339(),
    });
    let recorded = serde_json::to_value(&record).map_err(|error| failed("saving the recovery copy", error))?;
    write_json(&directory.join("record.json"), &recorded)
        .and_then(|_| write_json(&receipt_path, &receipt))
        .map_err(|error| failed("saving the recovery copy", error))?;

    let runtime_stopped = super::management_archive::archive_runtime_looks_online(&record)
        .map_err(|error| failed("checking the runtime", error))?;
    if runtime_stopped {
        super::management_context::stop_runtime_before_archive(&key)?;
    }
    let (_, record) = load(&key)?;
    let native = if record.claude_session.is_some() {
        move_claude_state(&record, &directory).map(|outcome| match outcome {
            NativeOutcome::Moved => "claudeState",
            other => other.label(),
        })
    } else {
        delete_native(&record, &directory).map(|outcome| outcome.label())
    };
    let native = match native {
        Ok(native) => native,
        Err(failure) => {
            receipt["status"] = json!("native_refused");
            receipt["error"] = json!(failure.message);
            let _ = write_json(&receipt_path, &receipt);
            return Err(failure);
        }
    };
    receipt["nativeDelete"] = json!(native);
    receipt["status"] = json!("native_deleted");
    let _ = write_json(&receipt_path, &receipt);

    let removed = cutex::session::store::remove_cutex_session_record(&record).map_err(|error| {
        UserInputExecutionError {
            stage: "persistence".into(),
            code: "persistence_uncertain".into(),
            message: format!(
                "the native thread was deleted but the session record could not be removed: {error:#}; recovery copy: {}",
                directory.display()
            ),
            retryable: false,
            details: json!({ "resyncRequired": true, "backupPath": directory }),
            outcome_unknown: true,
        }
    })?;
    // Best-effort projections; the record is gone either way.
    if let Some(native_id) = removed.codex_session_id.as_deref() {
        if let Err(error) = cutex::session::im_bridge::remove_cutex_session_im_record(native_id) {
            eprintln!("session delete: IM projection cleanup failed for {key}: {error:#}");
        }
    }
    if let Err(error) = cutex::management::v2::user_input::user_input_repository()
        .and_then(|queue| queue.clear_session(&key))
    {
        eprintln!("session delete: queue cleanup failed for {key}: {error:#}");
    }
    if let Ok(id) = cutex::role_revision::CutexSessionId::new(key.clone()) {
        if let Err(error) = cutex::agent_management::AgentManagementProvider::open_default()
            .map_err(|error| anyhow::anyhow!("{error:#}"))
            .and_then(|provider| provider.forget_deleted_session(&id).map_err(anyhow::Error::from))
        {
            eprintln!("session delete: roster cleanup failed for {key}: {error:#}");
        }
    }
    cutex::notes::mark_session_deleted_default(&key);
    let deleted_at = chrono::Utc::now().to_rfc3339();
    receipt["status"] = json!("complete");
    receipt["deletedAt"] = json!(deleted_at);
    let _ = write_json(&receipt_path, &receipt);
    Ok(json!({
        "cutexSessionId": key,
        "deleted": true,
        "threadId": removed.app_server_thread_id(),
        "nativeDelete": native,
        "runtimeStopped": runtime_stopped,
        "backupPath": directory,
        "deletedAt": deleted_at,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rpc(message: &str) -> CatalogError {
        CatalogError::Rpc {
            method: "thread/delete".into(),
            error: cutex::app_server::protocol::RpcError {
                code: -32600,
                message: message.into(),
                data: None,
            },
        }
    }

    #[test]
    fn native_refusals_are_classified() {
        assert!(is_fork_reference(&rpc(
            "cannot delete thread 019a: forked history still references it"
        )));
        assert!(is_thread_not_found(&rpc("thread not found: 019a")));
        assert!(!is_fork_reference(&rpc("thread not found: 019a")));
        assert!(!is_thread_not_found(&CatalogError::Transport("closed".into())));
    }

    #[test]
    fn rollout_evidence_only_reports_turns_it_can_see() {
        let dir = std::env::temp_dir().join(format!("cutex-delete-rollout-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let empty = dir.join("empty.jsonl");
        std::fs::write(&empty, "{\"timestamp\":\"t\",\"type\":\"session_meta\",\"payload\":{}}\n").unwrap();
        assert!(!rollout_shows_turns(&empty));
        let used = dir.join("used.jsonl");
        std::fs::write(
            &used,
            "{\"type\":\"session_meta\",\"payload\":{}}\n{\"type\":\"turn_context\",\"payload\":{}}\n",
        )
        .unwrap();
        assert!(rollout_shows_turns(&used));
        assert!(!rollout_shows_turns(&dir.join("missing.jsonl")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn claude_and_unbound_sessions_are_never_empty() {
        let mut record = CutexSessionRecord::new_at(
            "cutex.unbound".into(),
            None,
            "host".into(),
            "/tmp".into(),
            None,
            "2026-10-07T00:00:00Z".into(),
        )
        .unwrap();
        assert_eq!(not_empty_reason(&record), Some("noThread"));
        record.claude_session = Some(cutex::runtime::claude::ClaudeSession {
            deployment: cutex::runtime::claude::ClaudeDeployment {
                python: "/python".into(),
                adapter_source: "/adapter".into(),
                claude_cli: "/claude".into(),
                claude_home: "/claude-home".into(),
                tui_binary: "/claude-tui".into(),
            },
            thread_id: "t".into(),
            state_dir: "/state".into(),
            model: "m".into(),
            effort: None,
            instructions: String::new(),
            allow_write: false,
            allow_exec: false, write_explicit: false, exec_explicit: false,
            native_session_id: None,
            connection_id: None,
        });
        assert_eq!(not_empty_reason(&record), Some("unsupported"));
    }
}
