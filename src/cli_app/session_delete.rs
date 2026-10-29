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
/// Management input queue holds nothing. DSH/Claude sessions, sessions without a
/// bound thread and unreadable threads are never empty.
pub(crate) fn not_empty_reason(record: &CutexSessionRecord) -> Option<&'static str> {
    if record.claude_session.is_some() || record.runtime_backend.is_acp() {
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

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
fn isolated_endpoint(record: &CutexSessionRecord) -> anyhow::Result<cutex::catalog::OwnedStdioEndpoint> {
    let home = super::session_history::native_home_for(Some(record))?;
    super::session_history::isolated_endpoint(Some(record), &home)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
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
        // Project services outlive their registrant (services-project-scope):
        // the delete clears the registrant instead.
        .filter(|service| {
            service.owner_cutex_session_id.as_deref() == Some(key) && service.owner_scope.is_session()
        })
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
    let message = if details["nativeForkReferences"] == true {
        "a fork of this session on the same runtime home still reads its history; delete that fork first"
    } else {
        "this session is still referenced (Director, Operator or owned services); resolve those first"
    };
    error("session_referenced", message, false, details)
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

/// Create abandon (docs/design/028): the native thread of an unfinished
/// create that has no durable session. Moves its rollout (and the history copy
/// `adopt_from` made under `runtime/light/agents/<thread>`) into a recovery
/// directory; returns one line per moved item. Nothing found is not an error.
pub(crate) fn move_abandoned_native_thread(action_id: &str, thread: &str) -> anyhow::Result<Vec<String>> {
    anyhow::ensure!(uuid::Uuid::parse_str(thread).is_ok(), "native thread id is not a UUID");
    let directory = recovery_directory(&format!("abandon-{action_id}"))?;
    let mut moved = Vec::new();
    let home = cutex::launch::local_deployment::LocalDeployment::source_home()?;
    if let Ok(rollout) = cutex::launch::native_history::current(&home, thread) {
        move_rollout_aside(&home, thread, &rollout, &directory).map_err(|error| anyhow::anyhow!("{}", error.message))?;
        moved.push(format!("native thread {thread}: history moved to {}", directory.join("moved").display()));
    }
    let light = cutex::config::paths::runtime_dir()?.join("light/agents").join(thread);
    if light.is_dir() {
        let target = directory.join("light-agent");
        std::fs::rename(&light, &target)?;
        moved.push(format!("native thread {thread}: adopted history copy moved to {}", target.display()));
    }
    if moved.is_empty() {
        moved.push(format!("native thread {thread}: no history found (nothing to move)"));
    } else {
        write_json(&directory.join("receipt.json"), &json!({"abandonedCreate": action_id, "nativeThread": thread, "moved": moved}))?;
    }
    Ok(moved)
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
    /// The native started but has no `thread/delete` (internal: the caller
    /// falls back to moving storage, so it never reaches a receipt).
    Unsupported,
}

impl NativeOutcome {
    fn label(&self) -> &'static str {
        match self {
            Self::Deleted => "thread/delete",
            Self::Moved => "storageMove",
            Self::Nothing | Self::Unsupported => "none",
        }
    }
}

fn is_fork_reference(error: &CatalogError) -> bool {
    matches!(error, CatalogError::Rpc { error, .. } if error.message.contains("forked history still references"))
}

fn is_thread_not_found(error: &CatalogError) -> bool {
    matches!(error, CatalogError::Rpc { error, .. } if error.message.contains("thread not found"))
}

/// A native that predates `thread/delete` (old explicit-launch bundles, e.g.
/// r177-era sessions): `-32601`, or the app-server's `-32600` "unknown
/// variant" from request deserialization.
fn is_unsupported_method(error: &CatalogError) -> bool {
    matches!(error, CatalogError::Rpc { error, .. } if error.code == -32601
        || (error.code == -32600 && {
            let message = error.message.to_lowercase();
            message.contains("unknown variant") || message.contains("unknown method")
        }))
}

/// The left-behind copy of fork `candidate` in `home` (canonical), if it is
/// one: a fork made from storage (`cutex/session/fork`) is created in the
/// source's native home and then adopted into a home of its own,
/// `runtime/light/agents/<thread>`, whose `adoption.json` records that source
/// home as `history_home`. The copy left in the source home is no session's
/// history, yet its `history_base` still references the source's rollouts, so
/// native `thread/delete` of the source refused ("forked history still
/// references it") even after the fork itself was deleted. `None` when the
/// thread is not such a copy or a session record still uses it in `home`.
fn fork_leftover_copy(
    agents: &Path,
    home: &Path,
    store: &cutex::session::model::CutexSessionStore,
    candidate: &str,
) -> Option<PathBuf> {
    let adoption: Value =
        serde_json::from_slice(&std::fs::read(agents.join(candidate).join("adoption.json")).ok()?).ok()?;
    if adoption.get("native_id").and_then(Value::as_str) != Some(candidate) {
        return None;
    }
    let history_home = Path::new(adoption.get("history_home")?.as_str()?).canonicalize().ok()?;
    if history_home != home || agents.join(candidate).canonicalize().ok()? == home {
        return None;
    }
    let used_here = store.sessions.values().any(|record| {
        record.codex_session_id.as_deref() == Some(candidate)
            && super::session_history::native_home_for(Some(record))
                .ok()
                .and_then(|own| own.canonicalize().ok())
                .is_some_and(|own| own == home)
    });
    if used_here {
        return None;
    }
    cutex::launch::native_history::current(home, candidate).ok()
}

/// Left-behind fork copies in `home` (see [`fork_leftover_copy`]) whose
/// inherited history includes a rollout of `thread`.
fn fork_leftovers_referencing(
    home: &Path,
    thread: &str,
    store: &cutex::session::model::CutexSessionStore,
) -> Vec<(String, PathBuf)> {
    let (Ok(home), Ok(agents)) = (
        home.canonicalize(),
        cutex::config::paths::runtime_dir().map(|runtime| runtime.join("light").join("agents")),
    ) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&agents) else {
        return Vec::new();
    };
    let mut found = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .filter(|name| name != thread && uuid::Uuid::parse_str(name).is_ok_and(|uuid| uuid.to_string() == *name))
        .filter_map(|candidate| {
            let rollout = fork_leftover_copy(&agents, &home, store, &candidate)?;
            let chain = cutex::launch::native_history::recovery_sources(&home, &rollout).ok()?;
            chain
                .iter()
                .skip(1)
                .any(|inherited| {
                    cutex::launch::native_history::metadata(inherited)
                        .is_ok_and(|meta| meta.get("id").and_then(Value::as_str) == Some(thread))
                })
                .then_some((candidate, rollout))
        })
        .collect::<Vec<_>>();
    found.sort();
    found
}

/// Native `thread/delete` of left-behind fork copies, each first copied to
/// `<backup>/fork-copies/` when given. A copy another copy inherits from is
/// retried after that one is gone. Returns how many are gone.
fn delete_fork_copies(
    endpoint: &mut dyn CatalogEndpoint,
    copies: &[(String, PathBuf)],
    backup: Option<&Path>,
) -> usize {
    let mut pending = Vec::new();
    for (thread, rollout) in copies {
        if let Some(backup) = backup {
            let target = backup.join("fork-copies");
            let saved = std::fs::create_dir_all(&target).and_then(|_| {
                std::fs::copy(rollout, target.join(rollout.file_name().unwrap_or_default())).map(|_| ())
            });
            if let Err(error) = saved {
                eprintln!("session delete: fork copy {thread} kept, no backup: {error}");
                continue;
            }
        }
        pending.push(thread.as_str());
    }
    let mut released = 0;
    loop {
        let before = released;
        pending.retain(|thread| match endpoint.request("thread/delete", json!({ "threadId": thread })) {
            Ok(_) => {
                released += 1;
                false
            }
            Err(native) if is_thread_not_found(&native) => {
                released += 1;
                false
            }
            Err(native) if is_fork_reference(&native) => true,
            Err(native) => {
                eprintln!("session delete: fork copy {thread} not deleted: {native}");
                false
            }
        });
        if pending.is_empty() || released == before {
            break;
        }
    }
    released
}

/// After `cutex/session/fork` adopted `thread` into a home of its own: delete
/// the copy native `thread/fork` left in the source's home (`home`), so it no
/// longer keeps the source's history referenced. Best effort: a later delete
/// of the source releases a copy still left here.
pub(crate) fn release_fork_copy(source: &CutexSessionRecord, home: &Path, thread: &str) {
    let copy = (|| {
        let home = home.canonicalize().ok()?;
        let agents = cutex::config::paths::runtime_dir().ok()?.join("light").join("agents");
        let store = cutex::session::store::load_cutex_session_store().ok()?;
        fork_leftover_copy(&agents, &home, &store, thread)
    })();
    let Some(rollout) = copy else {
        return;
    };
    match isolated_endpoint(source) {
        Ok(mut endpoint) => {
            delete_fork_copies(&mut endpoint, &[(thread.to_string(), rollout)], None);
        }
        Err(error) => eprintln!("session fork: source-home copy of {thread} kept ({error:#})"),
    }
}

/// Native `thread/delete` of `thread` in `home`. A fork-reference refusal
/// caused only by copies earlier forks left in this home (also in homes
/// written before forks released them, see [`release_fork_copy`]) deletes
/// those copies (backed up under `directory`) and retries; a fork that still
/// lives on this home's history keeps the refusal.
fn delete_thread(
    endpoint: &mut dyn CatalogEndpoint,
    home: &Path,
    thread: &str,
    directory: &Path,
) -> Result<NativeOutcome, UserInputExecutionError> {
    let mut refusal = match endpoint.request("thread/delete", json!({ "threadId": thread })) {
        Ok(_) => return Ok(NativeOutcome::Deleted),
        Err(native) if is_fork_reference(&native) => native,
        Err(native) if is_thread_not_found(&native) => return Ok(NativeOutcome::Nothing),
        Err(native) if is_unsupported_method(&native) => return Ok(NativeOutcome::Unsupported),
        Err(native) => return Err(failed("deleting the native thread", native)),
    };
    let copies = cutex::session::store::load_cutex_session_store()
        .map(|store| fork_leftovers_referencing(home, thread, &store))
        .unwrap_or_default();
    if !copies.is_empty() && delete_fork_copies(endpoint, &copies, Some(directory)) > 0 {
        match endpoint.request("thread/delete", json!({ "threadId": thread })) {
            Ok(_) => return Ok(NativeOutcome::Deleted),
            Err(native) if is_thread_not_found(&native) => return Ok(NativeOutcome::Nothing),
            Err(native) if is_fork_reference(&native) => refusal = native,
            Err(native) => return Err(failed("deleting the native thread", native)),
        }
    }
    Err(referenced_error(json!({
        "nativeForkReferences": true,
        "message": refusal.to_string(),
    })))
}

fn delete_native(record: &CutexSessionRecord, directory: &Path) -> Result<NativeOutcome, UserInputExecutionError> {
    let Some(thread) = record.codex_session_id.as_deref() else {
        return Ok(NativeOutcome::Nothing);
    };
    let home = super::session_history::native_home_for(Some(record))
        .map_err(|error| failed("resolving the native home", error))?;
    let rollout = backup_rollouts(&home, thread, directory)
        .map_err(|error| failed("saving the recovery copy", error))?;
    let reason = match isolated_endpoint(record) {
        Ok(mut endpoint) => match delete_thread(&mut endpoint, &home, thread, directory)? {
            NativeOutcome::Unsupported => {
                // Stop the private native before its storage moves.
                drop(endpoint);
                "the native runtime has no thread/delete".to_string()
            }
            outcome => return Ok(outcome),
        },
        Err(spawn) => format!("{spawn:#}"),
    };
    // No usable native (an old explicit-launch bundle that is gone, changed
    // or predates thread/delete): move this thread's own rollout into the
    // recovery directory (state DB rows stay; native lists skip a thread
    // whose rollout is gone).
    eprintln!("session delete: native unavailable for {thread} ({reason}); moving storage");
    let Some(rollout) = rollout else {
        return Ok(NativeOutcome::Nothing);
    };
    move_rollout_aside(&home, thread, &rollout, directory)
}

/// Storage-move fallback for `thread` in `home`. Without a native there is no
/// reference index to refuse a fork that still reads this history, so the
/// rollouts of `home` are checked here: copies earlier forks left behind (no
/// session's history, see [`fork_leftover_copy`]) move along into
/// `fork-copies/`; any other rollout inheriting from `thread` refuses the
/// delete before anything moved.
fn move_rollout_aside(
    home: &Path,
    thread: &str,
    rollout: &Path,
    directory: &Path,
) -> Result<NativeOutcome, UserInputExecutionError> {
    let dependants = rollouts_inheriting_from(home, thread);
    let leftovers = if dependants.is_empty() {
        Vec::new()
    } else {
        cutex::session::store::load_cutex_session_store()
            .map(|store| fork_leftovers_referencing(home, thread, &store))
            .unwrap_or_default()
    };
    let leftover_paths = leftovers
        .iter()
        .filter_map(|(_, path)| path.canonicalize().ok())
        .collect::<Vec<_>>();
    let blocking = dependants
        .iter()
        .filter(|path| !leftover_paths.contains(path))
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    if !blocking.is_empty() {
        return Err(referenced_error(json!({
            "nativeForkReferences": true,
            "message": format!("forked history still references {thread}"),
            "rollouts": blocking,
        })));
    }
    let move_file = |source: &Path, target_dir: &Path| -> Result<(), UserInputExecutionError> {
        std::fs::create_dir_all(target_dir).map_err(|error| failed("moving storage", error))?;
        let target = target_dir.join(source.file_name().unwrap_or_default());
        std::fs::rename(source, &target)
            .or_else(|_| std::fs::copy(source, &target).and_then(|_| std::fs::remove_file(source)))
            .map_err(|error| failed("moving storage", error))
    };
    for leftover in &leftover_paths {
        move_file(leftover, &directory.join("fork-copies"))?;
    }
    move_file(rollout, &directory.join("moved"))?;
    Ok(NativeOutcome::Moved)
}

/// Rollouts in `home` (canonical paths) whose inherited history
/// (`session_meta.history_base`, transitively) includes `thread`.
fn rollouts_inheriting_from(home: &Path, thread: &str) -> Vec<PathBuf> {
    let mut pending = vec![home.join("sessions"), home.join("archived_sessions")];
    let mut parents = std::collections::HashMap::<String, String>::new();
    let mut paths = Vec::new();
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            let Ok(meta) = cutex::launch::native_history::metadata(&path) else {
                continue;
            };
            let (Some(id), Some(base)) = (
                meta.get("id").and_then(Value::as_str),
                meta.pointer("/history_base/thread_id").and_then(Value::as_str),
            ) else {
                continue;
            };
            parents.insert(id.to_string(), base.to_string());
            paths.push((id.to_string(), path));
        }
    }
    let inherits = |id: &str| {
        let mut current = id;
        for _ in 0..=parents.len() {
            match parents.get(current) {
                Some(base) if base == thread => return true,
                Some(base) => current = base,
                None => return false,
            }
        }
        false
    };
    let mut found = paths
        .into_iter()
        .filter(|(id, _)| id != thread && inherits(id))
        .filter_map(|(_, path)| path.canonicalize().ok())
        .collect::<Vec<_>>();
    found.sort();
    found.dedup();
    found
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

/// Clear durable service references before removing the session record. On
/// failure the session remains addressable, so the existing delete entry can
/// retry (native deletion already treats missing threads/storage as success).
/// The existing recovery receipt records the unfinished stage across crashes.
fn clear_project_service_registrant(
    key: &str,
    receipt_path: &Path,
    receipt: &mut Value,
) -> Result<(), UserInputExecutionError> {
    receipt["status"] = json!("service_cleanup_pending");
    write_json(receipt_path, receipt)
        .map_err(|error| failed("recording pending service cleanup", error))?;
    cutex::services::store::ServiceStore::open_default()
        .map_err(|error| error.to_string())
        .and_then(|store| store.forget_registrant(key).map_err(|error| error.message))
        .map_err(|error_text| {
            let mut failure = failed("clearing project service registrant", error_text);
            failure.details["backupPath"] = json!(receipt_path.parent());
            failure.details["nativeDeleted"] = json!(true);
            failure
        })?;
    receipt["status"] = json!("services_cleaned");
    write_json(receipt_path, receipt)
        .map_err(|error| failed("recording completed service cleanup", error))?;
    Ok(())
}

pub(crate) fn delete_for_management(
    cutex_session_id: &str,
    request: &SessionDeleteRequest,
) -> Result<Value, UserInputExecutionError> {
    let lane = cutex::management::v2::user_input::session_input_lock(cutex_session_id)
        .map_err(|error| failed("reserving session deletion", error))?;
    let _operation = lane.reserve().map_err(|error| failed("reserving session deletion", error))?;
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
    let native = if record.dsh_session.is_some() {
        match super::dsh_structure::move_state(&record, &directory) {
            Ok(true) => Ok("dshState"),
            Ok(false) => Ok(NativeOutcome::Nothing.label()),
            Err(error) => Err(failed("moving the DSH agent state", error)),
        }
    } else if record.claude_session.is_some() {
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

    clear_project_service_registrant(&key, &receipt_path, &mut receipt)?;

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
    for (label, result) in [
        ("activity", cutex::management::v2::activity::forget_deleted_session(&key)),
        ("usage", cutex::management::v2::usage::forget_deleted_session(&key)),
    ] {
        if let Err(error) = result {
            eprintln!("session delete: {label} cleanup failed for {key}: {error:#}");
        }
    }
    cutex::notes::mark_session_deleted_default(&key);
    if let Err(error) = cutex::management::v2::claude_token_usage::remove(&key) {
        eprintln!("session delete: Claude token usage cleanup failed for {key}: {error:#}");
    }
    if let Err(error) = cutex::management::v2::claude_plan::remove(&key) {
        eprintln!("session delete: Claude plan cleanup failed for {key}: {error:#}");
    }
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

    const ORIGINAL: &str = "018a0000-0000-7000-8000-00000000000a";
    const FORK: &str = "018a0000-0000-7000-8000-00000000000b";
    const FORK_OF_FORK: &str = "018a0000-0000-7000-8000-00000000000c";
    const IN_HOME_FORK: &str = "018a0000-0000-7000-8000-00000000000d";

    /// Native `thread/delete` over rollout files: refuses a thread another
    /// rollout's `history_base` names, like the native reference index.
    struct FakeNative {
        home: PathBuf,
        deleted: Vec<String>,
    }

    impl CatalogEndpoint for FakeNative {
        fn request(&mut self, method: &str, params: Value) -> Result<Value, CatalogError> {
            assert_eq!(method, "thread/delete");
            let thread = params["threadId"].as_str().unwrap().to_string();
            let rpc = |message: String| CatalogError::Rpc {
                method: method.into(),
                error: cutex::app_server::protocol::RpcError { code: -32600, message, data: None },
            };
            let Ok(rollout) = cutex::launch::native_history::current(&self.home, &thread) else {
                return Err(rpc(format!("thread not found: {thread}")));
            };
            let referenced = walk(&self.home.join("sessions")).iter().any(|other| {
                cutex::launch::native_history::metadata(other)
                    .is_ok_and(|meta| meta["history_base"]["thread_id"] == thread.as_str())
            });
            if referenced {
                return Err(rpc(format!(
                    "invalid thread-store request: cannot delete thread {thread}: forked history still references it"
                )));
            }
            std::fs::remove_file(rollout).unwrap();
            self.deleted.push(thread);
            Ok(json!({}))
        }

        fn notify(&mut self, _: &str, _: Option<Value>) -> Result<(), CatalogError> {
            Ok(())
        }
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            if entry.path().is_dir() {
                found.extend(walk(&entry.path()));
            } else {
                found.push(entry.path());
            }
        }
        found
    }

    /// A rollout of `id` in `home`, inheriting from `base` when given.
    fn rollout(home: &Path, id: &str, base: Option<&str>) {
        let dir = home.join("sessions/2026/10/16");
        std::fs::create_dir_all(&dir).unwrap();
        let mut meta = json!({ "id": id, "cwd": "/work" });
        if let Some(base) = base {
            meta["history_base"] = json!({ "thread_id": base, "end_byte_offset": 10 });
        }
        let line = json!({ "timestamp": "t", "type": "session_meta", "payload": meta });
        std::fs::write(dir.join(format!("rollout-2026-10-16T00-00-00-{id}.jsonl")), format!("{line}\n")).unwrap();
    }

    /// `runtime/light/agents/<id>/adoption.json` of a fork adopted from `history_home`.
    fn adopted_fork(id: &str, history_home: &Path) -> PathBuf {
        let home = cutex::config::paths::runtime_dir().unwrap().join("light/agents").join(id);
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        let provenance = json!({ "native_id": id, "source_history": "/elsewhere", "sha256": "00", "history_home": history_home });
        std::fs::write(home.join("adoption.json"), provenance.to_string()).unwrap();
        home
    }

    fn shared_home(test: &crate::cli_app::test_home::IsolatedTestHome) -> PathBuf {
        let home = test.root().join("shared-native-home");
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        home
    }

    /// Owner's sequence: fork an agent (the fork is adopted into its own home,
    /// native leaves a copy in the source's home), delete the fork, then delete
    /// the original. The left-behind copy is no dependant: the delete goes on.
    #[test]
    fn deleting_the_original_after_its_fork_releases_the_left_behind_copy() {
        let test = crate::cli_app::test_home::IsolatedTestHome::new("delete-fork-copy").unwrap();
        let source = shared_home(&test);
        rollout(&source, ORIGINAL, None);
        rollout(&source, FORK, Some(ORIGINAL));
        let fork_home = adopted_fork(FORK, &source);
        // The fork's own copy was already deleted with the fork.
        assert!(walk(&fork_home.join("sessions")).is_empty());

        let recovery = test.root().join("recovery");
        std::fs::create_dir_all(&recovery).unwrap();
        let mut native = FakeNative { home: source.clone(), deleted: Vec::new() };
        let outcome = delete_thread(&mut native, &source, ORIGINAL, &recovery).unwrap();
        assert!(matches!(outcome, NativeOutcome::Deleted));
        assert_eq!(native.deleted, vec![FORK.to_string(), ORIGINAL.to_string()]);
        assert_eq!(walk(&recovery.join("fork-copies")).len(), 1, "the copy is backed up first");
        assert!(walk(&source.join("sessions")).is_empty());
    }

    /// Reverse order: the original is deleted while its fork lives on in its
    /// own home (it holds its own copy of the inherited history).
    #[test]
    fn deleting_the_original_while_its_adopted_fork_lives_keeps_the_fork() {
        let test = crate::cli_app::test_home::IsolatedTestHome::new("delete-fork-live").unwrap();
        let source = shared_home(&test);
        rollout(&source, ORIGINAL, None);
        rollout(&source, FORK, Some(ORIGINAL));
        let fork_home = adopted_fork(FORK, &source);
        rollout(&fork_home, ORIGINAL, None);
        rollout(&fork_home, FORK, Some(ORIGINAL));

        let mut native = FakeNative { home: source.clone(), deleted: Vec::new() };
        let outcome = delete_thread(&mut native, &source, ORIGINAL, &test.root().join("recovery")).unwrap();
        assert!(matches!(outcome, NativeOutcome::Deleted));
        assert_eq!(walk(&fork_home.join("sessions")).len(), 2, "the fork's own home is untouched");
        // Then the fork itself, in its own home.
        let mut native = FakeNative { home: fork_home.clone(), deleted: Vec::new() };
        assert!(matches!(
            delete_thread(&mut native, &fork_home, FORK, &test.root().join("recovery-2")).unwrap(),
            NativeOutcome::Deleted
        ));
    }

    /// A fork of a fork is made in the first fork's home: deleting the first
    /// fork releases the copy its own fork left there, also when that copy is
    /// in turn inherited by another left-behind copy.
    #[test]
    fn fork_of_a_fork_copies_are_released_in_dependency_order() {
        let test = crate::cli_app::test_home::IsolatedTestHome::new("delete-fork-chain").unwrap();
        let fork_home = adopted_fork(FORK, &test.root().join("unused"));
        rollout(&fork_home, ORIGINAL, None);
        rollout(&fork_home, FORK, Some(ORIGINAL));
        rollout(&fork_home, FORK_OF_FORK, Some(FORK));
        rollout(&fork_home, IN_HOME_FORK, Some(FORK_OF_FORK));
        adopted_fork(FORK_OF_FORK, &fork_home);
        adopted_fork(IN_HOME_FORK, &fork_home);

        let mut native = FakeNative { home: fork_home.clone(), deleted: Vec::new() };
        let outcome = delete_thread(&mut native, &fork_home, FORK, &test.root().join("recovery")).unwrap();
        assert!(matches!(outcome, NativeOutcome::Deleted));
        assert_eq!(native.deleted, vec![IN_HOME_FORK.to_string(), FORK_OF_FORK.to_string(), FORK.to_string()]);
    }

    /// A fork that still lives on this home's history (no home of its own) is
    /// a real dependant: the refusal stays, with fork wording.
    #[test]
    fn a_fork_living_on_the_same_home_still_blocks_the_delete() {
        let test = crate::cli_app::test_home::IsolatedTestHome::new("delete-fork-inhome").unwrap();
        let source = shared_home(&test);
        rollout(&source, ORIGINAL, None);
        rollout(&source, FORK, Some(ORIGINAL));
        adopted_fork(FORK, &source);
        rollout(&source, IN_HOME_FORK, Some(ORIGINAL));
        // Adopted from another home: not a copy left in this one.
        adopted_fork(FORK_OF_FORK, &test.root().join("other-home"));
        rollout(&source, FORK_OF_FORK, Some(ORIGINAL));

        let mut native = FakeNative { home: source.clone(), deleted: Vec::new() };
        let refusal = delete_thread(&mut native, &source, ORIGINAL, &test.root().join("recovery"))
            .err()
            .unwrap();
        assert_eq!(refusal.code, "session_referenced");
        assert_eq!(refusal.details["nativeForkReferences"], true);
        assert!(refusal.message.contains("fork"), "{}", refusal.message);
        assert_eq!(native.deleted, vec![FORK.to_string()], "only the left-behind copy went");
        assert_eq!(walk(&source.join("sessions")).len(), 3);
    }

    #[test]
    fn a_left_behind_copy_needs_matching_provenance() {
        let test = crate::cli_app::test_home::IsolatedTestHome::new("delete-fork-provenance").unwrap();
        let source = shared_home(&test);
        rollout(&source, FORK, None);
        let agents = cutex::config::paths::runtime_dir().unwrap().join("light/agents");
        let canonical = source.canonicalize().unwrap();
        let store = cutex::session::model::CutexSessionStore::default();
        assert_eq!(fork_leftover_copy(&agents, &canonical, &store, FORK), None, "no adoption record");
        adopted_fork(FORK, &test.root().join("other-home"));
        std::fs::create_dir_all(test.root().join("other-home")).unwrap();
        assert_eq!(fork_leftover_copy(&agents, &canonical, &store, FORK), None, "adopted from another home");
        adopted_fork(FORK, &source);
        assert!(fork_leftover_copy(&agents, &canonical, &store, FORK).is_some());
        std::fs::write(
            agents.join(FORK).join("adoption.json"),
            json!({ "native_id": ORIGINAL, "history_home": source }).to_string(),
        )
        .unwrap();
        assert_eq!(fork_leftover_copy(&agents, &canonical, &store, FORK), None, "provenance of another thread");
    }

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

    /// An old "local"-scope explicit-launch session like the two on host-a:
    /// adopted with `session_only` (local-only, not an Agent, no formal
    /// name), contract version 4, its own native home under
    /// `runtime/light/agents/<native id>`, pinned to the r177 bundle manifest.
    fn local_explicit_launch(native: &str, manifest: &Path) -> CutexSessionRecord {
        let home = cutex::config::paths::runtime_dir().unwrap().join("light/agents").join(native);
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        rollout(&home, native, None);
        let mut record = CutexSessionRecord::new_at(
            format!("cutex.{native}"),
            Some(native.into()),
            cutex::platform::host::current_host_name(),
            home.to_string_lossy().into_owned(),
            Some("orchid".into()),
            "2026-09-20T00:00:00Z".into(),
        )
        .unwrap();
        record.display_name_hint = Some(format!("chat {native}"));
        record.agent_enabled = false;
        record.registration_class = cutex::agent_bus::model::AgentRegistrationClass::LocalOnly;
        record.explicit_launch = Some(cutex::agent_management::ExplicitLaunchContract {
            version: 4,
            migration_action_id: None,
            native_id: native.into(),
            native_home: home.canonicalize().unwrap(),
            bundle_manifest: manifest.to_path_buf(),
            bundle_sha256: cutex::agent_management::file_sha256(manifest).unwrap(),
        });
        record
    }

    #[test]
    fn project_service_cleanup_failure_keeps_session_retryable_after_native_delete() {
        let test = crate::cli_app::test_home::IsolatedTestHome::new("delete-service-retry").unwrap();
        let manifest = test.root().join("bundle.json");
        std::fs::write(&manifest, "{}").unwrap();
        let target = local_explicit_launch(ORIGINAL, &manifest.canonicalize().unwrap());
        let mut sessions = cutex::session::model::CutexSessionStore::default();
        sessions.sessions.insert(target.cutex_session_id.clone(), target.clone());
        cutex::session::store::save_cutex_session_store(&sessions).unwrap();
        let services = cutex::services::store::ServiceStore::open_default().unwrap();
        services.mutate(|registry| {
            let service = serde_json::from_value(json!({
                "slug": "board", "displayName": "Board", "host": services.host(), "port": 1,
                "runtime": "external", "lifetime": "persistent", "ownerScope": "project",
                "projectId": "proj", "ownerCutexSessionId": target.cutex_session_id,
                "createdAt": "2026-10-28T00:00:00Z", "updatedAt": "2026-10-28T00:00:00Z"
            })).unwrap();
            registry.services.insert("board".into(), service);
            Ok(())
        }).unwrap();
        assert!(references(&target.cutex_session_id).unwrap().is_null());
        // Session services still block deletion, including alongside project services.
        services.mutate(|registry| {
            let mut personal = registry.services["board"].clone();
            personal.slug = "personal".into();
            personal.owner_scope = cutex::services::store::ServiceOwnerScope::Session;
            registry.services.insert("personal".into(), personal);
            Ok(())
        }).unwrap();
        let request = SessionDeleteRequest {
            expected_revision: Some(target.durable_revision()), only_if_empty: false,
        };
        assert_eq!(delete_for_management(&target.cutex_session_id, &request).unwrap_err().code, "session_referenced");
        services.remove("personal", &cutex::services::store::Caller::Owner).unwrap();

        // Exercise the actual post-native stage, then simulate registry I/O failure.
        let directory = recovery_directory(&target.cutex_session_id).unwrap();
        assert!(matches!(delete_native(&target, &directory).unwrap(), NativeOutcome::Moved));
        let mut receipt = json!({"cutexSessionId": target.cutex_session_id, "status": "native_deleted"});
        let receipt_path = directory.join("receipt.json");
        let registry_bytes = std::fs::read(services.registry_path()).unwrap();
        std::fs::write(services.registry_path(), "not json").unwrap();
        let failure = clear_project_service_registrant(&target.cutex_session_id, &receipt_path, &mut receipt).unwrap_err();
        assert!(failure.retryable);
        assert_eq!(failure.details["nativeDeleted"], true);
        assert!(cutex::session::store::load_cutex_session_store().unwrap().sessions.contains_key(&target.cutex_session_id));
        let saved: Value = serde_json::from_slice(&std::fs::read(&receipt_path).unwrap()).unwrap();
        assert_eq!(saved["status"], "service_cleanup_pending");
        std::fs::write(services.registry_path(), registry_bytes).unwrap();

        // Retry through the public deletion path: missing native storage is OK.
        let result = delete_for_management(&target.cutex_session_id, &request).unwrap();
        assert_eq!(result["deleted"], true);
        let board = services.get("board").unwrap();
        assert_eq!(board.owner_cutex_session_id, None);
        assert_eq!(board.owner_scope, cutex::services::store::ServiceOwnerScope::Project);
        assert_eq!(board.project_id.as_deref(), Some("proj"));
        assert!(!cutex::session::store::load_cutex_session_store().unwrap().sessions.contains_key(&target.cutex_session_id));
        let backup = PathBuf::from(result["backupPath"].as_str().unwrap());
        let completed: Value = serde_json::from_slice(&std::fs::read(backup.join("receipt.json")).unwrap()).unwrap();
        assert_eq!(completed["status"], "complete");
    }

    #[test]
    fn local_explicit_launch_session_with_an_unstartable_old_bundle_is_deleted_by_storage_move() {
        let test = crate::cli_app::test_home::IsolatedTestHome::new("delete-local-r177").unwrap();
        let bundles = test.root().join("releases/r177");
        std::fs::create_dir_all(&bundles).unwrap();
        // The r177 manifest is still there but its natives are not (pruned
        // releases): the pinned native cannot start.
        let manifest = bundles.join("bundle-r177.json");
        std::fs::write(&manifest, r#"{"version":4,"natives":"pruned"}"#).unwrap();
        let manifest = manifest.canonicalize().unwrap();
        let target = local_explicit_launch(ORIGINAL, &manifest);
        let other = local_explicit_launch(FORK, &manifest);
        assert!(target.is_owned_session(), "the host-a shape is a local owned session");
        let mut store = cutex::session::model::CutexSessionStore::default();
        for record in [&target, &other] {
            store.sessions.insert(record.cutex_session_id.clone(), record.clone());
        }
        cutex::session::store::save_cutex_session_store(&store).unwrap();
        let target_home = target.explicit_launch.as_ref().unwrap().native_home.clone();
        let other_home = other.explicit_launch.as_ref().unwrap().native_home.clone();

        let receipt = delete_for_management(
            &target.cutex_session_id,
            &SessionDeleteRequest { expected_revision: Some(target.durable_revision()), only_if_empty: false },
        )
        .unwrap();
        assert_eq!(receipt["deleted"], true);
        assert_eq!(receipt["nativeDelete"], "storageMove");
        assert_eq!(receipt["threadId"], ORIGINAL);
        let backup = PathBuf::from(receipt["backupPath"].as_str().unwrap());
        assert!(backup.starts_with(cutex::config::paths::runtime_dir().unwrap().join("session-deletes")));
        let saved: Value = serde_json::from_slice(&std::fs::read(backup.join("record.json")).unwrap()).unwrap();
        assert_eq!(saved["cutex_session_id"], target.cutex_session_id.as_str());
        assert_eq!(walk(&backup.join("rollouts")).len(), 1, "rollout copied before anything moved");
        assert_eq!(walk(&backup.join("moved")).len(), 1, "rollout moved aside");
        assert!(walk(&target_home.join("sessions")).is_empty());
        let receipt_file: Value = serde_json::from_slice(&std::fs::read(backup.join("receipt.json")).unwrap()).unwrap();
        assert_eq!(receipt_file["status"], "complete");

        let after = cutex::session::store::load_cutex_session_store().unwrap();
        assert!(!after.sessions.contains_key(&target.cutex_session_id));
        assert_eq!(after.sessions.get(&other.cutex_session_id), Some(&other), "the other session is untouched");
        assert_eq!(walk(&other_home.join("sessions")).len(), 1);
        assert!(manifest.is_file(), "the pinned bundle is not touched");
    }

    #[test]
    fn local_explicit_launch_session_whose_bundle_is_gone_is_deleted_too() {
        let test = crate::cli_app::test_home::IsolatedTestHome::new("delete-local-gone").unwrap();
        let manifest = test.root().join("bundle-r177.json");
        std::fs::write(&manifest, "{}").unwrap();
        let manifest = manifest.canonicalize().unwrap();
        let target = local_explicit_launch(ORIGINAL, &manifest);
        std::fs::remove_file(&manifest).unwrap();
        let mut store = cutex::session::model::CutexSessionStore::default();
        store.sessions.insert(target.cutex_session_id.clone(), target.clone());
        cutex::session::store::save_cutex_session_store(&store).unwrap();
        let receipt = delete_for_management(
            &target.cutex_session_id,
            &SessionDeleteRequest { expected_revision: None, only_if_empty: false },
        )
        .unwrap();
        assert_eq!(receipt["nativeDelete"], "storageMove");
        assert!(cutex::session::store::load_cutex_session_store().unwrap().sessions.is_empty());
    }

    /// A native that starts but predates `thread/delete`.
    struct OldNative;

    impl CatalogEndpoint for OldNative {
        fn request(&mut self, method: &str, _: Value) -> Result<Value, CatalogError> {
            Err(CatalogError::Rpc {
                method: method.into(),
                error: cutex::app_server::protocol::RpcError {
                    code: -32600,
                    message: format!("Invalid request: unknown variant `{method}`, expected one of `initialize`"),
                    data: None,
                },
            })
        }

        fn notify(&mut self, _: &str, _: Option<Value>) -> Result<(), CatalogError> {
            Ok(())
        }
    }

    #[test]
    fn a_native_without_thread_delete_falls_back_to_the_storage_move() {
        let test = crate::cli_app::test_home::IsolatedTestHome::new("delete-old-native").unwrap();
        let source = shared_home(&test);
        rollout(&source, ORIGINAL, None);
        let outcome = delete_thread(&mut OldNative, &source, ORIGINAL, &test.root().join("recovery")).unwrap();
        assert!(matches!(outcome, NativeOutcome::Unsupported));
        assert!(is_unsupported_method(&rpc("Invalid request: unknown variant `thread/delete`")));
        assert!(!is_unsupported_method(&rpc("thread not found: 019a")));
    }

    #[test]
    fn storage_move_never_breaks_a_fork_that_still_reads_the_history() {
        let test = crate::cli_app::test_home::IsolatedTestHome::new("delete-move-fork").unwrap();
        let source = shared_home(&test);
        rollout(&source, ORIGINAL, None);
        rollout(&source, IN_HOME_FORK, Some(ORIGINAL));
        rollout(&source, FORK_OF_FORK, Some(IN_HOME_FORK));
        let recovery = test.root().join("recovery");
        let current = cutex::launch::native_history::current(&source, ORIGINAL).unwrap();
        let refusal = move_rollout_aside(&source, ORIGINAL, &current, &recovery).err().unwrap();
        assert_eq!(refusal.code, "session_referenced");
        assert_eq!(refusal.details["rollouts"].as_array().unwrap().len(), 2, "direct and transitive forks");
        assert_eq!(walk(&source.join("sessions")).len(), 3, "nothing moved");
        assert!(!recovery.join("moved").exists());
    }

    #[test]
    fn storage_move_takes_left_behind_fork_copies_along() {
        let test = crate::cli_app::test_home::IsolatedTestHome::new("delete-move-leftover").unwrap();
        let source = shared_home(&test);
        rollout(&source, ORIGINAL, None);
        rollout(&source, FORK, Some(ORIGINAL));
        adopted_fork(FORK, &source);
        let recovery = test.root().join("recovery");
        let current = cutex::launch::native_history::current(&source, ORIGINAL).unwrap();
        assert!(matches!(move_rollout_aside(&source, ORIGINAL, &current, &recovery).unwrap(), NativeOutcome::Moved));
        assert!(walk(&source.join("sessions")).is_empty());
        assert_eq!(walk(&recovery.join("fork-copies")).len(), 1);
        assert_eq!(walk(&recovery.join("moved")).len(), 1);
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
            initial_permission_mode: None,
        });
        assert_eq!(not_empty_reason(&record), Some("unsupported"));
    }

    #[test]
    fn dsh_sessions_never_probe_codex_for_empty_delete() {
        let mut record = CutexSessionRecord::new_at(
            "cutex.dsh-empty-probe".into(), None, "host".into(), "/tmp".into(), None,
            "2026-10-25T00:00:00Z".into(),
        ).unwrap();
        record.runtime_backend = cutex::session::model::CutexSessionRuntimeBackend::Dsh;
        assert_eq!(not_empty_reason(&record), Some("unsupported"));
        record.codex_session_id = Some(uuid::Uuid::new_v4().to_string());
        assert_eq!(not_empty_reason(&record), Some("unsupported"));
    }
}
