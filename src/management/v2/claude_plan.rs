//! `cutexState.latestPlan` of Claude sessions (and, since r265, of DSH
//! sessions: their adapter sends DSH's `todo_write` checklist as the same
//! `turn/plan/updated`; `app_server_runtime::record_dsh_latest_state`).
//!
//! Codex sessions derive it from their native rollout (`launch::native_plan`);
//! a Claude session has no rollout, so the daemon keeps the newest
//! `turn/plan/updated` notification the adapter sent for the session's bound
//! thread (its `TodoWrite` list or `Task*` checklist), one small private file
//! per session under `<runtime>/management-v2/claude-plan/`, as
//! `claude_token_usage` does for usage. It survives daemon restarts, so
//! bootstrap / history show the checklist after a (re)connect.
//!
//! Wire shape (as Codex): `{threadId, turnId, explanation: string|null,
//! plan: [{step, status: pending|inProgress|completed}], updatedAt}` where
//! `updatedAt` is the envelope's `receivedAt`. A value whose `threadId` is
//! not the session's current Claude thread (a subagent's, a rebound thread)
//! is not served, and a revert that removes the value's turn drops it.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::config::atomic::write_private_pretty_json_atomic;
use crate::config::paths::runtime_dir;

use super::model::{EventEnvelope, EventSource, NativeMessageKind};

const DIRECTORY: &str = "claude-plan";
const METHOD: &str = "turn/plan/updated";
const STATUSES: [&str; 3] = ["pending", "inProgress", "completed"];

fn default_root() -> anyhow::Result<PathBuf> {
    Ok(runtime_dir()?.join("management-v2"))
}

fn file_for(root: &Path, cutex_session_id: &str) -> PathBuf {
    super::claude_token_usage::session_file(root, DIRECTORY, cutex_session_id)
}

/// The `latestPlan` value of a published Claude event, if it is a
/// well-formed `turn/plan/updated` notification of `bound_thread_id`.
fn plan_of(envelope: &EventEnvelope, bound_thread_id: &str) -> Option<Value> {
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
    let plan = params
        .get("plan")?
        .as_array()?
        .iter()
        .map(|step| {
            let text = step.get("step").and_then(Value::as_str)?;
            let status = step.get("status").and_then(Value::as_str).filter(|s| STATUSES.contains(s))?;
            Some(json!({ "step": text, "status": status }))
        })
        .collect::<Option<Vec<_>>>()?;
    let turn_id = params.get("turnId").and_then(Value::as_str).filter(|turn| !turn.is_empty());
    let explanation = params.get("explanation").and_then(Value::as_str);
    Some(json!({
        "threadId": thread_id,
        "turnId": turn_id,
        "explanation": explanation,
        "plan": plan,
        "updatedAt": envelope.received_at,
    }))
}

/// Keeps `envelope` as the session's latest plan when it is a plan
/// notification of the bound Claude thread. Returns whether it was stored.
pub fn record_event(envelope: &EventEnvelope, bound_thread_id: &str) -> anyhow::Result<bool> {
    record_event_at(&default_root()?, envelope, bound_thread_id)
}

pub fn record_event_at(root: &Path, envelope: &EventEnvelope, bound_thread_id: &str) -> anyhow::Result<bool> {
    let Some(plan) = plan_of(envelope, bound_thread_id) else {
        return Ok(false);
    };
    let path = file_for(root, &envelope.cutex_session_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let stored = json!({ "cutexSessionId": envelope.cutex_session_id, "latestPlan": plan });
    write_private_pretty_json_atomic(&path, &stored, "Claude latest plan")?;
    Ok(true)
}

/// The stored latest plan of `cutex_session_id` on its current Claude thread
/// `thread_id`, or `None`.
pub fn latest(cutex_session_id: &str, thread_id: &str) -> anyhow::Result<Option<Value>> {
    latest_at(&default_root()?, cutex_session_id, thread_id)
}

pub fn latest_at(root: &Path, cutex_session_id: &str, thread_id: &str) -> anyhow::Result<Option<Value>> {
    let bytes = match std::fs::read(file_for(root, cutex_session_id)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let stored: Value = serde_json::from_slice(&bytes)?;
    if stored.get("cutexSessionId").and_then(Value::as_str) != Some(cutex_session_id) {
        return Ok(None);
    }
    Ok(stored
        .get("latestPlan")
        .filter(|plan| plan.get("threadId").and_then(Value::as_str) == Some(thread_id))
        .cloned())
}

/// After a revert: drops the stored value when its turn was removed.
pub fn forget_removed_turns(cutex_session_id: &str, removed_turn_ids: &[&str]) -> anyhow::Result<()> {
    forget_removed_turns_at(&default_root()?, cutex_session_id, removed_turn_ids)
}

pub fn forget_removed_turns_at(root: &Path, cutex_session_id: &str, removed_turn_ids: &[&str]) -> anyhow::Result<()> {
    let stored: Value = match std::fs::read(file_for(root, cutex_session_id)) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let turn = stored.pointer("/latestPlan/turnId").and_then(Value::as_str);
    if stored.is_null() || turn.is_some_and(|turn| removed_turn_ids.contains(&turn)) {
        remove_at(root, cutex_session_id)?;
    }
    Ok(())
}

/// Deletes the stored value (session deleted).
pub fn remove(cutex_session_id: &str) -> anyhow::Result<()> {
    remove_at(&default_root()?, cutex_session_id)
}

pub fn remove_at(root: &Path, cutex_session_id: &str) -> anyhow::Result<()> {
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
            let dir = std::env::temp_dir().join(format!("cutex-claude-plan-{}", uuid::Uuid::new_v4()));
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
            received_at: "2026-10-21T01:02:03Z".into(),
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

    /// The adapter's `turn/plan/updated` (`native_tools.py`: explanation null).
    fn plan_params(thread: &str, turn: &str, first: &str) -> Value {
        json!({
            "threadId": thread, "turnId": turn, "explanation": null,
            "plan": [{"step": first, "status": "completed"}, {"step": "Run tests", "status": "inProgress"},
                     {"step": "Report", "status": "pending"}]
        })
    }

    #[test]
    fn keeps_the_newest_bound_thread_plan_across_reopen() {
        let root = TempDir::new();
        let (session, thread) = ("cutex/claude:a@host", "claude:a");
        assert_eq!(latest_at(root.path(), session, thread).unwrap(), None);
        assert!(record_event_at(root.path(), &envelope(session, METHOD, plan_params(thread, "turn-1", "Read")), thread).unwrap());
        assert!(record_event_at(root.path(), &envelope(session, METHOD, plan_params(thread, "turn-2", "Edit")), thread).unwrap());
        // Other methods and other threads (subagents) never replace it.
        assert!(!record_event_at(root.path(), &envelope(session, "turn/completed", plan_params(thread, "turn-3", "x")), thread).unwrap());
        assert!(!record_event_at(root.path(), &envelope(session, METHOD, plan_params("claude:a:task:1", "turn-3", "x")), thread).unwrap());
        let found = latest_at(root.path(), session, thread).unwrap().unwrap();
        assert_eq!(found, json!({
            "threadId": thread, "turnId": "turn-2", "explanation": null,
            "plan": [{"step": "Edit", "status": "completed"}, {"step": "Run tests", "status": "inProgress"},
                     {"step": "Report", "status": "pending"}],
            "updatedAt": "2026-10-21T01:02:03Z"
        }));
        assert_eq!(latest_at(root.path(), session, "claude:b").unwrap(), None);
        assert_eq!(latest_at(root.path(), "cutex.other", thread).unwrap(), None);
    }

    #[test]
    fn a_revert_drops_only_a_removed_turn_and_delete_removes_it() {
        let root = TempDir::new();
        let (session, thread) = ("cutex.r", "claude:r");
        record_event_at(root.path(), &envelope(session, METHOD, plan_params(thread, "turn-2", "a")), thread).unwrap();
        forget_removed_turns_at(root.path(), session, &["turn-3"]).unwrap();
        assert!(latest_at(root.path(), session, thread).unwrap().is_some());
        forget_removed_turns_at(root.path(), session, &["turn-2"]).unwrap();
        assert_eq!(latest_at(root.path(), session, thread).unwrap(), None);
        record_event_at(root.path(), &envelope(session, METHOD, plan_params(thread, "turn-4", "a")), thread).unwrap();
        remove_at(root.path(), session).unwrap();
        remove_at(root.path(), session).unwrap();
        assert_eq!(latest_at(root.path(), session, thread).unwrap(), None);
    }

    #[test]
    fn malformed_plans_are_not_stored_and_an_empty_list_is() {
        let root = TempDir::new();
        let mut params = plan_params("claude:m", "turn-1", "a");
        params["plan"][1]["status"] = json!("in_progress");
        assert!(!record_event_at(root.path(), &envelope("cutex.m", METHOD, params), "claude:m").unwrap());
        let mut params = plan_params("claude:m", "turn-1", "a");
        params["plan"] = json!(null);
        assert!(!record_event_at(root.path(), &envelope("cutex.m", METHOD, params), "claude:m").unwrap());
        // A cleared checklist is the latest plan too.
        let mut params = plan_params("claude:m", "turn-1", "a");
        params["plan"] = json!([]);
        assert!(record_event_at(root.path(), &envelope("cutex.m", METHOD, params), "claude:m").unwrap());
        assert_eq!(latest_at(root.path(), "cutex.m", "claude:m").unwrap().unwrap()["plan"], json!([]));
    }
}
