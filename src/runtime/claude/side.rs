//! Side chats of a Claude agent (adapter feature `sideChat`, claude-r21).
//! Management relays `/v2/sessions/{id}/side-chats` to the agent's running
//! adapter (`claude/side/*`), which holds the whole Side state in memory: a
//! hidden thread on an SDK fork of the agent's native session. Opening Side
//! never starts a runtime; an adapter without the feature is refused.
use super::*;

/// Why a Side request did not reach (or was refused by) the Claude adapter.
#[derive(Debug)]
pub enum SideError {
    /// Not a Claude session with an adapter that declares `sideChat`.
    Unsupported,
    /// No adapter runs for this agent.
    Offline,
    /// The adapter refused: `data.code` is the cutex Side error code.
    Refused { rpc_code: i64, code: String, message: String, data: Value },
    Failed(anyhow::Error),
}

/// The deployed adapter (`features.json`) declares Side chats.
pub fn supports_side(session: &ClaudeSession) -> bool {
    adapter_features(&session.deployment.adapter_source)["sideChat"] == true
}

/// The Claude binding of `record` when Side may be relayed to its adapter.
pub fn side_gate(record: &CutexSessionRecord) -> Result<&ClaudeSession, SideError> {
    let session = record.claude_session.as_ref().ok_or(SideError::Unsupported)?;
    if !supports_side(session) {
        return Err(SideError::Unsupported);
    }
    if record.is_retired() || record.runtime_pid.is_none() {
        return Err(SideError::Offline);
    }
    Ok(session)
}

/// `claude/side/<action>` on the agent's running adapter, for its main thread.
pub fn side_request(id: &str, action: &str, mut params: Value) -> Result<Value, SideError> {
    let current = crate::session::store::with_cutex_session_store(|store| store.sessions.get(id).cloned())
        .map_err(SideError::Failed)?
        .ok_or(SideError::Unsupported)?;
    let session = side_gate(&current)?;
    // A remote session's adapter is not reachable from here.
    record(id).map_err(|_| SideError::Offline)?;
    let (client, _) = connect(session).map_err(|_| SideError::Offline)?;
    params["threadId"] = json!(session.thread_id);
    match client.handle().request(&format!("claude/side/{action}"), params) {
        Ok(result) => Ok(result),
        Err(crate::app_server::client::AppServerClientError::Rpc(error)) => {
            let data = error.data.unwrap_or(Value::Null);
            let code = data.get("code").and_then(Value::as_str).unwrap_or_default().to_string();
            Err(SideError::Refused { rpc_code: error.code, code, message: error.message, data })
        }
        Err(error) => Err(SideError::Failed(anyhow::Error::new(error))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(root: &Path, features: Option<Value>, pid: Option<u32>) -> CutexSessionRecord {
        let adapter = root.join("adapter");
        std::fs::create_dir_all(adapter.join("src/cute_claude")).unwrap();
        if let Some(features) = features {
            std::fs::write(
                adapter.join("src/cute_claude/features.json"),
                serde_json::to_vec(&json!({"version": 1, "features": features})).unwrap(),
            )
            .unwrap();
        }
        let mut record = CutexSessionRecord::new(
            "cutex.side".into(), None, "test-host".into(), root.to_string_lossy().into(), None,
        )
        .unwrap();
        record.runtime_backend = CutexSessionRuntimeBackend::ClaudeSubscription;
        record.runtime_pid = pid;
        record.claude_session = Some(ClaudeSession {
            deployment: ClaudeDeployment {
                python: root.join("python"), adapter_source: adapter, claude_cli: root.join("claude"),
                claude_home: root.join("account"), tui_binary: root.join("tui"),
            },
            thread_id: uuid::Uuid::new_v4().to_string(),
            state_dir: root.join("state"),
            model: "sonnet".into(),
            effort: None,
            instructions: String::new(),
            allow_write: false,
            allow_exec: false, write_explicit: false, exec_explicit: false,
            native_session_id: None,
            connection_id: None,
            initial_permission_mode: None,
        });
        record
    }

    #[test]
    fn side_needs_an_adapter_declaring_it_and_a_running_runtime() {
        let roots: Vec<_> = (0..4)
            .map(|_| std::env::temp_dir().join(format!("claude-side-{}", uuid::Uuid::new_v4())))
            .collect();
        // claude-r20 and older: no `sideChat` (or no features.json at all).
        let old = record(&roots[0], Some(json!({"fork": true})), Some(42));
        assert!(matches!(side_gate(&old), Err(SideError::Unsupported)));
        let bare = record(&roots[1], None, Some(42));
        assert!(matches!(side_gate(&bare), Err(SideError::Unsupported)));
        let offline = record(&roots[2], Some(json!({"sideChat": true})), None);
        assert!(matches!(side_gate(&offline), Err(SideError::Offline)));
        let online = record(&roots[3], Some(json!({"sideChat": true})), Some(42));
        assert_eq!(side_gate(&online).unwrap().thread_id, online.claude_session.as_ref().unwrap().thread_id);
        let mut codex = online.clone();
        codex.claude_session = None;
        assert!(matches!(side_gate(&codex), Err(SideError::Unsupported)));
        for root in roots {
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}
