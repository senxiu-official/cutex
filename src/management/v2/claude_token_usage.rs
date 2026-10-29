//! `cutexState.latestTokenUsage` of Claude sessions (and, since r265, of DSH
//! sessions: `app_server_runtime::record_dsh_latest_state`).
//!
//! Codex sessions derive it from their native rollout
//! (`launch::native_token_usage`); a Claude session has no rollout, so the
//! daemon keeps the newest `thread/tokenUsage/updated` notification the
//! adapter sent for the session's bound thread, one small private file per
//! session under `<runtime>/management-v2/claude-token-usage/`. It survives
//! daemon restarts, so bootstrap / history show context usage right after a
//! (re)connect (adapters before claude-r22 report usage only at turn end;
//! claude-r22 also after each model request while a turn runs).
//!
//! Wire shape (as Codex): the notification params `{threadId, turnId,
//! tokenUsage}` plus `updatedAt` (the envelope's `receivedAt`). A value whose
//! `threadId` is not the session's current Claude thread is not served, and
//! a revert that removes the value's turn drops it (the next usage report of
//! the adapter replaces it).

use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde_json::{json, Value};
use sha2::Digest;

use crate::config::atomic::write_private_pretty_json_atomic;
use crate::config::paths::runtime_dir;

use super::model::{EventEnvelope, EventSource, NativeMessageKind};

const DIRECTORY: &str = "claude-token-usage";
const METHOD: &str = "thread/tokenUsage/updated";

fn default_root() -> anyhow::Result<PathBuf> {
    Ok(runtime_dir()?.join("management-v2"))
}

fn file_for(root: &Path, cutex_session_id: &str) -> PathBuf {
    session_file(root, DIRECTORY, cutex_session_id)
}

fn locked<T>(
    root: &Path,
    id: &str,
    operation: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let path = file_for(root, id);
    std::fs::create_dir_all(path.parent().expect("usage directory"))?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))?;
    lock.lock_exclusive()?;
    operation()
}

fn deleted(root: &Path, id: &str) -> bool {
    // Corrupt/unreadable markers also fail closed. Never reopen a deleted ID.
    !matches!(std::fs::symlink_metadata(file_for(root, id).with_extension("deleted")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound)
}

/// The private per-session file of `directory` (also `claude_plan`).
pub(super) fn session_file(root: &Path, directory: &str, cutex_session_id: &str) -> PathBuf {
    // Session ids may hold `/` and `:`; the file name is their digest and the
    // file repeats the id (checked on read).
    let digest = format!("{:x}", sha2::Sha256::digest(cutex_session_id.as_bytes()));
    root.join(directory).join(format!("{}.json", &digest[..32]))
}

/// The `latestTokenUsage` value of a published Claude event, if it is a
/// well-formed `thread/tokenUsage/updated` notification of `bound_thread_id`.
fn usage_of(envelope: &EventEnvelope, bound_thread_id: &str) -> Option<Value> {
    if envelope.source != EventSource::AppServer {
        return None;
    }
    let native = envelope.native.as_ref()?;
    if native.kind != NativeMessageKind::Notification
        || native.message.get("method").and_then(Value::as_str) != Some(METHOD)
    {
        return None;
    }
    let params = native.message.get("params")?.as_object()?;
    let thread_id = params.get("threadId").and_then(Value::as_str)?;
    if thread_id != bound_thread_id {
        return None;
    }
    let token_usage = params.get("tokenUsage").filter(|usage| {
        usage.get("total").is_some_and(Value::is_object)
            && usage.get("last").is_some_and(Value::is_object)
    })?;
    let turn_id = params
        .get("turnId")
        .and_then(Value::as_str)
        .filter(|turn| !turn.is_empty());
    let mut usage = json!({
        "threadId": thread_id,
        "turnId": turn_id,
        "tokenUsage": token_usage,
        "updatedAt": envelope.received_at,
    });
    // Only this occurrence's cache-policy report may influence batching.
    // Legacy snapshots without a generation remain displayable.
    if let Some(generation) = envelope.correlation.runtime_generation {
        usage["runtimeGeneration"] = json!(generation);
    }
    Some(usage)
}

/// Keeps `envelope` as the session's latest token usage when it is a usage
/// notification of the bound Claude thread. Returns whether it was stored.
pub fn record_event(envelope: &EventEnvelope, bound_thread_id: &str) -> anyhow::Result<bool> {
    record_event_at(&default_root()?, envelope, bound_thread_id)
}

pub fn record_event_at(
    root: &Path,
    envelope: &EventEnvelope,
    bound_thread_id: &str,
) -> anyhow::Result<bool> {
    let Some(usage) = usage_of(envelope, bound_thread_id) else {
        return Ok(false);
    };
    locked(root, &envelope.cutex_session_id, || {
        if deleted(root, &envelope.cutex_session_id) {
            return Ok(false);
        }
        let path = file_for(root, &envelope.cutex_session_id);
        let stored =
            json!({ "cutexSessionId": envelope.cutex_session_id, "latestTokenUsage": usage });
        write_private_pretty_json_atomic(&path, &stored, "Claude latest token usage")?;
        Ok(true)
    })
}

/// The stored latest token usage of `cutex_session_id` on its current Claude
/// thread `thread_id`, or `None`.
pub fn latest(cutex_session_id: &str, thread_id: &str) -> anyhow::Result<Option<Value>> {
    latest_at(&default_root()?, cutex_session_id, thread_id)
}

pub fn latest_at(
    root: &Path,
    cutex_session_id: &str,
    thread_id: &str,
) -> anyhow::Result<Option<Value>> {
    if deleted(root, cutex_session_id) {
        return Ok(None);
    }
    let path = file_for(root, cutex_session_id);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let stored: Value = serde_json::from_slice(&bytes)?;
    if stored.get("cutexSessionId").and_then(Value::as_str) != Some(cutex_session_id) {
        return Ok(None);
    }
    Ok(stored
        .get("latestTokenUsage")
        .filter(|usage| usage.get("threadId").and_then(Value::as_str) == Some(thread_id))
        .cloned())
}

/// After a revert: drops the stored value when its turn was removed.
pub fn forget_removed_turns(
    cutex_session_id: &str,
    removed_turn_ids: &[&str],
) -> anyhow::Result<()> {
    forget_removed_turns_at(&default_root()?, cutex_session_id, removed_turn_ids)
}

pub fn forget_removed_turns_at(
    root: &Path,
    cutex_session_id: &str,
    removed_turn_ids: &[&str],
) -> anyhow::Result<()> {
    locked(root, cutex_session_id, || {
        forget_removed_turns_locked(root, cutex_session_id, removed_turn_ids)
    })
}
fn forget_removed_turns_locked(
    root: &Path,
    cutex_session_id: &str,
    removed_turn_ids: &[&str],
) -> anyhow::Result<()> {
    let path = file_for(root, cutex_session_id);
    let stored: Value = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let turn = stored
        .pointer("/latestTokenUsage/turnId")
        .and_then(Value::as_str);
    if stored.is_null() || turn.is_some_and(|turn| removed_turn_ids.contains(&turn)) {
        remove_projection(root, cutex_session_id)?;
    }
    Ok(())
}

/// Deletes the stored value (session deleted).
pub fn remove(cutex_session_id: &str) -> anyhow::Result<()> {
    remove_at(&default_root()?, cutex_session_id)
}

pub fn remove_at(root: &Path, cutex_session_id: &str) -> anyhow::Result<()> {
    locked(root, cutex_session_id, || {
        write_private_pretty_json_atomic(
            &file_for(root, cutex_session_id).with_extension("deleted"),
            &true,
            "Claude deleted session fence",
        )?;
        remove_projection(root, cutex_session_id)
    })
}
fn remove_projection(root: &Path, cutex_session_id: &str) -> anyhow::Result<()> {
    match std::fs::remove_file(file_for(root, cutex_session_id)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::management::v2::model::{EventCorrelation, NativeMessage, CONTRACT_VERSION};

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("cutex-claude-usage-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn envelope(session: &str, method: &str, params: Value) -> EventEnvelope {
        EventEnvelope {
            contract_version: CONTRACT_VERSION,
            event_id: "event-1".into(),
            cursor: "cursor-1".into(),
            stream_id: "stream".into(),
            sequence: 1,
            received_at: "2026-10-20T01:02:03Z".into(),
            cutex_session_id: session.into(),
            host_id: "host".into(),
            source: EventSource::AppServer,
            sensitivity: "private".into(),
            schema: None,
            correlation: EventCorrelation::default(),
            native: Some(NativeMessage {
                kind: NativeMessageKind::Notification,
                message: json!({ "method": method, "params": params }),
            }),
            cutex: None,
        }
    }

    fn usage_params(thread: &str, turn: &str) -> Value {
        json!({
            "threadId": thread, "turnId": turn,
            "tokenUsage": {
                "total": { "totalTokens": 120, "inputTokens": 100, "cachedInputTokens": 80,
                    "cacheWriteInputTokens": 10, "outputTokens": 20, "reasoningOutputTokens": 0 },
                "last": { "totalTokens": 90, "inputTokens": 90, "cachedInputTokens": 0,
                    "cacheWriteInputTokens": 0, "outputTokens": 0, "reasoningOutputTokens": 0 },
                "modelContextWindow": 1000000
            }
        })
    }

    #[test]
    fn keeps_the_newest_bound_thread_usage_across_reopen() {
        let root = TempDir::new();
        let session = "cutex/claude:a@host";
        let thread = "claude:a";
        assert_eq!(latest_at(root.path(), session, thread).unwrap(), None);
        assert!(record_event_at(
            root.path(),
            &envelope(session, METHOD, usage_params(thread, "turn-1")),
            thread
        )
        .unwrap());
        assert!(record_event_at(
            root.path(),
            &envelope(session, METHOD, usage_params(thread, "turn-2")),
            thread
        )
        .unwrap());
        // Other methods and other threads (subagents) never replace it.
        assert!(!record_event_at(
            root.path(),
            &envelope(session, "turn/completed", usage_params(thread, "turn-3")),
            thread
        )
        .unwrap());
        assert!(!record_event_at(
            root.path(),
            &envelope(session, METHOD, usage_params("claude:a:task:1", "turn-3")),
            thread
        )
        .unwrap());
        let found = latest_at(root.path(), session, thread).unwrap().unwrap();
        assert_eq!(found["turnId"], "turn-2");
        assert_eq!(found["threadId"], thread);
        assert_eq!(found["tokenUsage"]["modelContextWindow"], 1000000);
        assert_eq!(found["updatedAt"], "2026-10-20T01:02:03Z");
        // Another thread of the same session (rebound) and another session: none.
        assert_eq!(latest_at(root.path(), session, "claude:b").unwrap(), None);
        assert_eq!(latest_at(root.path(), "cutex.other", thread).unwrap(), None);
    }

    #[test]
    fn a_revert_drops_only_a_removed_turn_and_delete_removes_it() {
        let root = TempDir::new();
        let (session, thread) = ("cutex.r", "claude:r");
        record_event_at(
            root.path(),
            &envelope(session, METHOD, usage_params(thread, "turn-2")),
            thread,
        )
        .unwrap();
        forget_removed_turns_at(root.path(), session, &["turn-3"]).unwrap();
        assert!(latest_at(root.path(), session, thread).unwrap().is_some());
        forget_removed_turns_at(root.path(), session, &["turn-2", "turn-3"]).unwrap();
        assert_eq!(latest_at(root.path(), session, thread).unwrap(), None);
        record_event_at(
            root.path(),
            &envelope(session, METHOD, usage_params(thread, "turn-4")),
            thread,
        )
        .unwrap();
        remove_at(root.path(), session).unwrap();
        remove_at(root.path(), session).unwrap();
        assert_eq!(latest_at(root.path(), session, thread).unwrap(), None);
    }

    #[test]
    fn malformed_usage_is_not_stored() {
        let root = TempDir::new();
        let mut params = usage_params("claude:m", "turn-1");
        params["tokenUsage"]["last"] = Value::Null;
        assert!(!record_event_at(
            root.path(),
            &envelope("cutex.m", METHOD, params),
            "claude:m"
        )
        .unwrap());
    }
    #[test]
    fn cache_policy_retains_occurrence_fence_and_deleted_sessions_do_not_resurrect() {
        let root = TempDir::new();
        let (session, thread) = ("cutex.cache", "claude:cache");
        let mut params = usage_params(thread, "turn-1");
        params["tokenUsage"]["cacheTtlSeconds"] = json!(300);
        let mut event = envelope(session, METHOD, params);
        event.correlation.runtime_generation = Some(7);
        assert!(record_event_at(root.path(), &event, thread).unwrap());
        let reopened = latest_at(root.path(), session, thread).unwrap().unwrap();
        assert_eq!(reopened["runtimeGeneration"], 7);
        assert_eq!(reopened["tokenUsage"]["cacheTtlSeconds"], 300);
        remove_at(root.path(), session).unwrap();
        assert!(!record_event_at(root.path(), &event, thread).unwrap());
        assert!(latest_at(root.path(), session, thread).unwrap().is_none());
        assert!(!file_for(root.path(), session).exists());
        // A delete before the first observation is fenced as well.
        remove_at(root.path(), "cutex.unseen").unwrap();
        event.cutex_session_id = "cutex.unseen".into();
        assert!(!record_event_at(root.path(), &event, thread).unwrap());
    }
}
