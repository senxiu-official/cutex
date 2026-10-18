//! Claude structural operations use the adapter's native-session journal,
//! with the same cutex input lock and durable roster as Codex operations.
use anyhow::{ensure, Context};
use cutex::management::control_plane::HumanManagementPrincipal;
use cutex::management::v2::session_fork::{default_fork_name, SessionForkRequest};
use cutex::management::v2::user_input::UserInputExecutionError;
use cutex::session::model::CutexSessionRecord;
use serde_json::{json, Value};

fn failure(code: &str, error: anyhow::Error) -> UserInputExecutionError {
    use cutex::app_server::client::AppServerClientError;
    let details = match error.downcast_ref::<AppServerClientError>() {
        Some(AppServerClientError::Rpc(error)) => error.data.clone().unwrap_or_else(|| json!({})),
        _ => json!({}),
    };
    let uncertain = match error.downcast_ref::<AppServerClientError>() {
        Some(AppServerClientError::Timeout { .. } | AppServerClientError::Disconnected(_)
            | AppServerClientError::Transport(_)) => true,
        Some(AppServerClientError::Rpc(error)) => error.data.as_ref()
            .and_then(|data| data.get("outcomeUnknown")).and_then(Value::as_bool) == Some(true),
        _ => false,
    };
    UserInputExecutionError {
        stage: "route".into(),
        code: code.into(),
        message: format!("{error:#}"),
        retryable: false,
        details,
        outcome_unknown: uncertain,
    }
}

fn restart_report(runtime: &Value) -> (bool, Vec<Value>) {
    let restarted = matches!(runtime["state"].as_str(), Some("waiting" | "working"))
        && runtime["connectionId"].as_str().is_some();
    let warnings = if runtime["state"] == "failed" {
        vec![json!({"code": "claude_runtime_restart_failed",
            "message": "History changed successfully, but the Claude runtime could not restart. Inspect the runtime and start it explicitly; do not repeat the history operation.",
            "runtimeState": "failed", "restartError": runtime["restartError"]})]
    } else {
        Vec::new()
    };
    (restarted, warnings)
}

pub(super) fn revert(
    record: &CutexSessionRecord,
    before: &str,
    rewind_files: bool,
) -> Result<Value, UserInputExecutionError> {
    let run = || -> anyhow::Result<Value> {
        let id = &record.cutex_session_id;
        let input_lock = super::app_server_user_input::session_input_lock(id)?;
        let guard = input_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("session input lock poisoned"))?;
        let result = cutex::runtime::claude::structure::request(
            id,
            "claude/thread/revert",
            revert_params(before, rewind_files),
        )?;
        drop(guard);
        refresh_observer(record, &result["runtime"])?;
        let current = cutex::session::store::load_cutex_session_store()?;
        let generation = current
            .sessions
            .get(id)
            .context("reverted session disappeared")?
            .runtime_generation;
        Ok(revert_response(record, before, generation, result))
    };
    run().map_err(|e| failure("history_revert_failed", e))
}

/// `claude/thread/revert` params: `rewindFiles` (the adapter restores the
/// files from Claude Code's checkpoint of the first removed turn) is sent only
/// when requested, so adapters before claude-r8's file rewind never see it.
fn revert_params(before: &str, rewind_files: bool) -> Value {
    if rewind_files {
        json!({"beforeTurnId": before, "rewindFiles": true})
    } else {
        json!({"beforeTurnId": before})
    }
}

/// `cutex/session/history/revert` result (`sessionHistoryRevertResult`,
/// Claude branch) around the adapter's `claude/thread/revert` receipt.
fn revert_response(record: &CutexSessionRecord, before: &str, generation: u64, result: Value) -> Value {
    let (restarted, warnings) = restart_report(&result["runtime"]);
    json!({
        "cutexSessionId": record.cutex_session_id, "threadId": record.app_server_thread_id(),
        "revertedBeforeTurnId": before, "removedTurnIds": result["removedTurnIds"],
        "removedTurns": result["removedTurnIds"].as_array().map(Vec::len).unwrap_or(0),
        "live": record.runtime_pid.is_some(), "runtimeRestarted": restarted,
        "runtimeState": result["runtime"]["state"], "warnings": warnings,
        "runtimePid": record.runtime_pid, "runtimeGeneration": generation,
        "actionId": result["actionId"], "backupPath": result["backupPath"],
        "receiptPath": result["receiptPath"], "native": {"method": "claude/thread/revert", "result": result}
    })
}

/// Fork facts for the management layer (`session_fork::fork_result`), which
/// passes `warnings` (the source runtime's restart outcome) through.
fn fork_facts(source: &CutexSessionRecord, id: &str, thread: &str, name: &str, facts: &Value) -> Value {
    let (_, warnings) = restart_report(&facts["sourceRuntime"]);
    json!({"newCutexSessionId": id, "sourceThreadId": source.app_server_thread_id(),
        "threadId": thread, "forkedThroughTurnId": facts["forkedThroughTurnId"], "name": name,
        "warnings": warnings})
}

pub(super) fn fork(
    principal: &HumanManagementPrincipal,
    source: &CutexSessionRecord,
    request: &SessionForkRequest,
) -> Result<Value, UserInputExecutionError> {
    let run = || -> anyhow::Result<Value> {
        ensure!(
            !source.is_retired(),
            "restore the archived agent before forking"
        );
        let path = cutex::session::store::cutex_sessions_path()?;
        let store = cutex::session::store::load_cutex_session_store()?;
        let taken = |name: &str| {
            store.sessions.values().any(|r| {
                r.formal_agent_name.as_deref() == Some(name)
                    || cutex::session::metadata::cutex_session_presented_name(r) == name
            })
        };
        let name = request.name.clone().unwrap_or_else(|| {
            default_fork_name(
                &cutex::session::metadata::cutex_session_presented_name(source),
                taken,
            )
        });
        ensure!(!taken(&name), "an agent named {name} already exists");
        let id = format!("cutex.{}", uuid::Uuid::new_v4());
        let thread = uuid::Uuid::new_v4().to_string();
        let target = cutex::config::paths::runtime_dir()?
            .join("claude/sessions")
            .join(&id);
        let input_lock =
            super::app_server_user_input::session_input_lock(&source.cutex_session_id)?;
        let guard = input_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("session input lock poisoned"))?;
        let facts = cutex::runtime::claude::structure::request(
            &source.cutex_session_id,
            "claude/thread/fork",
            json!({"lastTurnId": request.after_turn_id, "targetThreadId": thread, "targetStateDir": target, "name": name}),
        )?;
        let record = cutex::runtime::claude::structure::fork_record(source, &id, &name, &facts)?;
        cutex::session::store::with_locked_session_rows(&path, |rows| {
            ensure!(
                !rows
                    .sessions()
                    .any(|(_, r)| r.formal_agent_name.as_deref() == Some(&name)),
                "agent name changed during fork"
            );
            rows.insert_session(id.clone(), record.clone());
            rows.commit().map(drop)
        })
        .with_context(|| {
            format!(
                "fork projection saved at {}; durable registration incomplete",
                target.display()
            )
        })?;
        drop(guard);
        refresh_observer(source, &facts["sourceRuntime"])?;
        if source.agent_enabled {
            import_and_assign(principal, source, &id, &name).with_context(|| {
                format!("fork {id} exists; roster import or project assignment incomplete")
            })?;
        }
        cutex::notify::session::session(
            &thread,
            cutex::notify::session::Change::Set(
                cutex::config::store::load_codez_config_checked()?.new_session_notification,
            ),
        )?;
        Ok(fork_facts(source, &id, &thread, &name, &facts))
    };
    run().map_err(|e| failure("session_fork_failed", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structural_rpc_failure_preserves_recovery_receipt_and_uncertainty() {
        use cutex::app_server::client::AppServerClientError;
        for unknown in [false, true] {
            let receipt = json!({"historyCommitted": true, "outcomeUnknown": unknown,
                "reason": "projectionFailed", "actionId": "history-action",
                "receiptPath": "private/session-operations/history-action.json",
                "backupPath": "private/session-operations/history-action.backup"});
            let error = anyhow::Error::new(AppServerClientError::Rpc(cutex::app_server::protocol::RpcError {
                code: -32000, message: "Projection failed after native history changed".into(),
                data: Some(receipt.clone()),
            })).context("Claude structural operation failed");
            let result = failure("history_revert_failed", error);
            assert_eq!(result.details, receipt);
            assert_eq!(result.outcome_unknown, unknown);
            assert!(!result.retryable);
        }
        let result = failure("history_revert_failed",
            anyhow::Error::new(AppServerClientError::Transport("response lost".into())));
        assert!(result.outcome_unknown);
        assert!(!result.retryable);
    }

    #[test]
    fn revert_params_carry_rewind_files_only_when_requested() {
        assert_eq!(revert_params("turn-2", false), json!({"beforeTurnId": "turn-2"}));
        assert_eq!(revert_params("turn-2", true), json!({"beforeTurnId": "turn-2", "rewindFiles": true}));
    }

    #[test]
    fn committed_history_with_failed_restart_returns_a_warning() {
        let (restarted, warnings) = restart_report(&json!({"state": "failed", "restartError": "ProviderError"}));
        assert!(!restarted);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0]["code"], "claude_runtime_restart_failed");
        assert!(warnings[0]["message"].as_str().unwrap().contains("do not repeat"));
        assert_eq!(restart_report(&json!({"state": "offline"})), (false, vec![]));
        assert_eq!(restart_report(&json!({"state": "waiting", "connectionId": "new"})), (true, vec![]));
        assert_eq!(restart_report(&json!({"state": "working"})), (false, vec![]));
    }

    fn claude_record(runtime_pid: Option<u32>) -> CutexSessionRecord {
        let mut record = CutexSessionRecord::new_at(
            "cutex.claude".into(), None, "host".into(), "/work".into(),
            Some("claude-profile".into()), "2026-10-10T00:00:00Z".into(),
        )
        .unwrap();
        record.runtime_pid = runtime_pid;
        record.claude_session = Some(cutex::runtime::claude::ClaudeSession {
            deployment: cutex::runtime::claude::ClaudeDeployment {
                python: "/python".into(), adapter_source: "/adapter".into(),
                claude_cli: "/claude".into(), claude_home: "/claude-home".into(),
                tui_binary: "/claude-tui".into(),
            },
            thread_id: "018a0000-0000-7000-8000-000000000001".into(),
            state_dir: "/state".into(),
            model: "sonnet".into(),
            effort: None,
            instructions: String::new(),
            allow_write: false,
            allow_exec: false, write_explicit: false, exec_explicit: false,
            native_session_id: Some("018a0000-0000-7000-8000-0000000000aa".into()),
            connection_id: Some("c1".into()),
            initial_permission_mode: None,
        });
        record
    }

    /// The adapter's `claude/thread/revert` receipt (`session_ops.py` audit
    /// record plus the restored runtime).
    fn revert_receipt(runtime: Value, backup: Value) -> Value {
        json!({"threadId": "018a0000-0000-7000-8000-000000000001", "beforeTurnId": "turn-2",
            "removedTurnIds": ["turn-2", "turn-3"], "sourceSessionId": "018a0000-0000-7000-8000-0000000000aa",
            "backupPath": backup, "rewindFilesRequested": false, "filesRewound": false,
            "historyCommitted": true, "nativeSessionId": "018a0000-0000-7000-8000-0000000000bb",
            "runtime": runtime, "actionId": "action-1", "state": "completed",
            "receiptPath": "/state/session-operations/action-1.json"})
    }

    fn validate(method: &str, result: Value) -> Result<(), String> {
        cutex::management::v2::contract_validation::validate_cutex_response(&json!({
            "contractVersion": 2, "requestId": "r", "cutexSessionId": "cutex.claude",
            "cutex": {"method": method, "result": result},
        }))
    }

    #[test]
    fn revert_responses_match_the_management_contract() {
        const REVERT: &str = cutex::management::v2::history_revert::METHOD;
        let backup = json!("/claude-home/projects/w/018a0000-0000-7000-8000-0000000000aa.jsonl");
        let restarted = revert_response(&claude_record(Some(42)), "turn-2", 8, revert_receipt(
            json!({"threadId": "t", "state": "waiting", "connectionId": "c2", "processId": 43}), backup.clone()));
        assert_eq!(restarted["runtimeRestarted"], true);
        assert_eq!(restarted["removedTurns"], 2);
        validate(REVERT, restarted).expect("restarted runtime");
        let failed = revert_response(&claude_record(Some(42)), "turn-2", 7, revert_receipt(
            json!({"threadId": "t", "state": "failed", "sessionId": "s", "restartError": "ProviderError"}), backup));
        assert_eq!(failed["warnings"][0]["code"], "claude_runtime_restart_failed");
        validate(REVERT, failed).expect("committed history, restart failed");
        // Offline, first turn reverted, no native transcript existed.
        let offline = revert_response(&claude_record(None), "turn-2", 7, revert_receipt(
            json!({"threadId": "t", "state": "offline", "processId": null}), Value::Null));
        assert_eq!(offline["live"], false);
        validate(REVERT, offline).expect("offline runtime without a transcript");
    }

    #[test]
    fn fork_warnings_reach_the_management_response() {
        use cutex::management::v2::session_fork::{fork_result, METHOD};
        let facts = json!({"threadId": "018a0000-0000-7000-8000-000000000002",
            "forkedThroughTurnId": "turn-1", "sourceThreadId": "018a0000-0000-7000-8000-000000000001",
            "sourceRuntime": {"threadId": "t", "state": "failed", "restartError": "ProviderError"}});
        let facts = fork_facts(&claude_record(Some(42)), "cutex.fork", "018a0000-0000-7000-8000-000000000002",
            "Claude (fork)", &facts);
        let result = fork_result("cutex.claude", &facts, json!({"cutexSessionId": "cutex.fork"}));
        assert_eq!(result["warnings"][0]["code"], "claude_runtime_restart_failed");
        validate(METHOD, result).expect("fork with a source restart warning");
        let facts = fork_facts(&claude_record(None), "cutex.fork", "018a0000-0000-7000-8000-000000000002",
            "Claude (fork)", &json!({"forkedThroughTurnId": null, "sourceRuntime": {"state": "offline"}}));
        let result = fork_result("cutex.claude", &facts, json!({}));
        assert_eq!(result["warnings"], json!([]));
        validate(METHOD, result).expect("offline fork");
    }
}

fn refresh_observer(source: &CutexSessionRecord, runtime: &Value) -> anyhow::Result<()> {
    if source.runtime_pid.is_some() && runtime["connectionId"].as_str().is_some() {
        super::claude_runtime::manager().disconnect(&source.cutex_session_id)?;
        super::claude_runtime::observe(&source.cutex_session_id)
            .context("history operation committed; reconnecting the management observer failed")?;
    }
    Ok(())
}

/// Imports a just-created Claude record (`create_profile`, managed) into the
/// Agent Management roster under `name` (action id `<action>:<id>`).
pub(super) fn import_claude_agent(
    principal: &HumanManagementPrincipal,
    id: &str,
    name: &str,
    action: &str,
) -> anyhow::Result<()> {
    use cutex::agent_management::{AgentActionId, DurableImportRequest};
    let path = cutex::session::store::cutex_sessions_path()?;
    let provider = super::management_context::management_agent_provider()?;
    let candidate = provider
        .durable_agent_candidates(principal, &path)?
        .into_iter()
        .find(|c| {
            c.cutex_session_id
                .as_ref()
                .is_some_and(|v| v.as_str() == id)
        })
        .context("import candidate missing")?;
    let receipt = provider.import_durable_agent(
        principal,
        &path,
        &DurableImportRequest {
            action_id: AgentActionId::new(format!("{action}:{id}"))?,
            candidate,
            confirmed_formal_name: name.into(),
            assignment: None,
            detach: None,
        },
    )?;
    ensure!(
        receipt.complete,
        "import incomplete: {}",
        receipt.error.unwrap_or_default()
    );
    Ok(())
}

fn import_and_assign(
    principal: &HumanManagementPrincipal,
    source: &CutexSessionRecord,
    id: &str,
    name: &str,
) -> anyhow::Result<()> {
    use cutex::agent_management::AgentActionId;
    use cutex::management::control_plane::{
        HumanManagementProjectMutationKind, HumanManagementProjectMutationRequest,
        HumanManagementProjectMutationSchema,
    };
    import_claude_agent(principal, id, name, "claude-fork-import")?;
    let provider = super::management_context::management_agent_provider()?;
    let snapshot = provider.store().snapshot()?;
    let source_id = cutex::role_revision::CutexSessionId::new(source.cutex_session_id.clone())
        .map_err(|_| anyhow::anyhow!("invalid source session ID"))?;
    let project = snapshot
        .agents
        .get(&source_id)
        .and_then(|agent| cutex::agent_management::current_project_id(&snapshot, agent));
    if let Some(project) = project {
        let summary = super::management_context::list_management_projects(principal)?
            .projects
            .into_iter()
            .find(|p| p.project_id == project)
            .context("source project disappeared")?;
        super::management_context::execute_management_project_mutation(
            principal,
            &HumanManagementProjectMutationRequest {
                schema: HumanManagementProjectMutationSchema::V1,
                action_id: AgentActionId::new(format!("claude-fork-member:{id}"))?,
                project_id: summary.project_id,
                expected_authority_epoch: Some(summary.authority_epoch),
                expected_project_revision: Some(summary.project_revision),
                operation: HumanManagementProjectMutationKind::AddMember {
                    cutex_session_id: cutex::role_revision::CutexSessionId::new(id)
                        .map_err(|_| anyhow::anyhow!("invalid fork session ID"))?,
                },
            },
        )?;
    }
    Ok(())
}
