//! `cutex/session/history/revert {beforeTurnId, expectedRevision?, rewindFiles?}`: remove a
//! turn and every newer turn from the agent's native thread, keeping the same
//! agent, thread and (when online) the same runtime process.
//!
//! The app layer (`ManagementRequestContext::revert_session_history`) saves a
//! private recovery copy of the rollout under `runtime/history-reverts/` and
//! then sends native `thread/revert` (paginated threads) or `thread/rollback`
//! (legacy threads) over the daemon's own connection to the live runtime.
//! Native reloads the loaded thread in place and keeps subscriptions, so the
//! session's event stream also carries the native `thread/reverted`
//! notification. Without a live runtime it falls back to the isolated path.
//! Raw `thread/revert` is intentionally not exposed through the native
//! request policy; only this method issues it.
//!
//! Emits `cutex/session/historyReverted` after a confirmed revert.

use serde_json::{json, Value};

use crate::management::control_plane::HumanManagementPrincipal;
use crate::management::server::ManagementHistoryRevertHandler;

use super::repository::EventRepository;
use super::server::{
    append_cutex_event, invalid_user_input_error, post_operation_persistence_error,
    require_session_revision, ValidatedCutexRequest,
};
use super::user_input::UserInputExecutionError;

pub const METHOD: &str = "cutex/session/history/revert";
pub const EVENT: &str = "cutex/session/historyReverted";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RevertParams {
    pub(super) before_turn_id: String,
    pub(super) expected_revision: bool,
    /// Claude agents: also restore files from Claude Code's checkpoint
    /// (capability `claude-rewind-files`). Codex refuses it.
    pub(super) rewind_files: bool,
}

pub(super) fn parse_params(params: &Value) -> Result<RevertParams, UserInputExecutionError> {
    let object = params
        .as_object()
        .ok_or_else(|| invalid_user_input_error("params must be an object"))?;
    if let Some(key) = object
        .keys()
        .find(|key| !matches!(key.as_str(), "beforeTurnId" | "expectedRevision" | "rewindFiles"))
    {
        return Err(invalid_user_input_error(&format!("unknown parameter {key}")));
    }
    let before_turn_id = object
        .get("beforeTurnId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|turn| !turn.is_empty())
        .ok_or_else(|| invalid_user_input_error("beforeTurnId must be a non-empty string"))?
        .to_string();
    let rewind_files = match object.get("rewindFiles") {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err(invalid_user_input_error("rewindFiles must be a boolean")),
    };
    Ok(RevertParams {
        before_turn_id,
        expected_revision: object.contains_key("expectedRevision"),
        rewind_files,
    })
}

pub(super) fn dispatch(
    event_repository: &EventRepository,
    revert: ManagementHistoryRevertHandler,
    owner: Option<&HumanManagementPrincipal>,
    cutex_session_id: &str,
    session: &Value,
    request: &ValidatedCutexRequest,
) -> Result<Value, UserInputExecutionError> {
    if owner.is_none() {
        return Err(UserInputExecutionError {
            stage: "route".into(),
            code: "cutex_method_denied".into(),
            message: "history revert requires the Management root credential".into(),
            retryable: false,
            details: json!({}),
            outcome_unknown: false,
        });
    }
    let params = parse_params(&request.params)?;
    if params.expected_revision {
        require_session_revision(session, request.params.as_object().expect("validated object"))?;
    }
    crate::asks::lifecycle::check_no_obligations_default(cutex_session_id, "revert")
        .map_err(super::asks::operation_error)?;
    let result = revert(cutex_session_id, &params.before_turn_id, params.rewind_files);
    super::history::invalidate_session(cutex_session_id);
    let result = result?;
    // A reverted turn that answered an ask: flag it and tell the requester.
    let removed: Vec<String> = result
        .get("removedTurnIds")
        .and_then(Value::as_array)
        .map(|turns| turns.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    crate::asks::lifecycle::mark_reverted_default(cutex_session_id, &removed);
    crate::notes::mark_reverted_default(cutex_session_id, &removed);
    append_cutex_event(
        event_repository,
        cutex_session_id,
        super::model::EventCorrelation {
            thread_id: result.get("threadId").and_then(Value::as_str).map(str::to_string),
            management_request_id: Some(request.request_id.clone()),
            ..Default::default()
        },
        EVENT,
        event_params(&result),
    )
    .map_err(post_operation_persistence_error)?;
    Ok(result)
}

fn event_params(result: &Value) -> Value {
    json!({
        "threadId": result.get("threadId").cloned().unwrap_or(Value::Null),
        "revertedBeforeTurnId": result.get("revertedBeforeTurnId").cloned().unwrap_or(Value::Null),
        "removedTurns": result.get("removedTurns").cloned().unwrap_or(json!(0)),
        "removedTurnIds": result.get("removedTurnIds").cloned().unwrap_or(json!([])),
        "live": result.get("live").cloned().unwrap_or(json!(false)),
        "actionId": result.get("actionId").cloned().unwrap_or(Value::Null),
        "revertedAt": chrono::Utc::now().to_rfc3339(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_result() -> Value {
        json!({
            "cutexSessionId": "cutex.s1",
            "threadId": "018a0000-0000-7000-8000-000000000001",
            "revertedBeforeTurnId": "turn-2",
            "removedTurns": 2,
            "removedTurnIds": ["turn-3", "turn-2"],
            "live": true,
            "runtimeRestarted": false,
            "runtimePid": 4242,
            "runtimeGeneration": 3,
            "actionId": "revert-1",
            "backupPath": "C:/x/history-reverts/revert-1/before.jsonl",
            "receiptPath": "C:/x/history-reverts/revert-1/receipt.json",
            "native": { "method": "thread/revert", "result": { "thread": { "id": "t" },
                        "turnsBackwardsCursor": null, "itemsBackwardsCursor": null } },
        })
    }

    #[test]
    fn params_accept_optional_revision_and_reject_extras() {
        let parsed = parse_params(&json!({ "beforeTurnId": "turn-2" })).unwrap();
        assert_eq!(parsed.before_turn_id, "turn-2");
        assert!(!parsed.expected_revision);
        assert!(!parsed.rewind_files);
        assert!(parse_params(&json!({ "beforeTurnId": "t", "rewindFiles": true })).unwrap().rewind_files);
        assert!(!parse_params(&json!({ "beforeTurnId": "t", "rewindFiles": false })).unwrap().rewind_files);
        assert!(parse_params(&json!({ "beforeTurnId": "t", "expectedRevision": 4 }))
            .unwrap()
            .expected_revision);
        for bad in [
            json!({}),
            json!({ "beforeTurnId": "" }),
            json!({ "beforeTurnId": 3 }),
            json!({ "beforeTurnId": "t", "numTurns": 1 }),
            json!({ "beforeTurnId": "t", "rewindFiles": "yes" }),
            json!("turn"),
        ] {
            assert!(parse_params(&bad).is_err(), "{bad}");
        }
    }

    fn request(params: Value) -> ValidatedCutexRequest {
        ValidatedCutexRequest {
            request_id: "revert-req-1".into(),
            method: METHOD.into(),
            params,
        }
    }

    fn repository() -> (std::path::PathBuf, EventRepository) {
        let root = std::env::temp_dir().join(format!("cutex-v2-revert-{}", uuid::Uuid::new_v4()));
        let repository = EventRepository::open(&root, crate::platform::host::current_host_name())
            .expect("open event repository");
        (root, repository)
    }

    fn ok_handler(_: &str, before: &str, rewind_files: bool) -> Result<Value, UserInputExecutionError> {
        assert!(!rewind_files);
        assert_eq!(before, "turn-2");
        Ok(sample_result())
    }

    fn refusing_handler(_: &str, _: &str, _: bool) -> Result<Value, UserInputExecutionError> {
        panic!("handler must not run")
    }

    #[test]
    fn dispatch_requires_root_and_checks_revision_before_reverting() {
        let (root, repository) = repository();
        let session = json!({ "revision": 7 });
        let denied = dispatch(&repository, refusing_handler, None, "cutex.s1", &session,
            &request(json!({ "beforeTurnId": "turn-2" }))).unwrap_err();
        assert_eq!(denied.code, "cutex_method_denied");
        let owner = HumanManagementPrincipal::authenticated();
        let conflict = dispatch(&repository, refusing_handler, Some(&owner), "cutex.s1", &session,
            &request(json!({ "beforeTurnId": "turn-2", "expectedRevision": 6 }))).unwrap_err();
        assert_eq!(conflict.code, "revision_conflict");
        let result = dispatch(&repository, ok_handler, Some(&owner), "cutex.s1", &session,
            &request(json!({ "beforeTurnId": "turn-2", "expectedRevision": 7 }))).unwrap();
        assert_eq!(result["revertedBeforeTurnId"], "turn-2");
        let page = repository.page(super::super::repository::ReplayQuery::default()).unwrap();
        assert_eq!(page.events.len(), 1);
        let event = serde_json::to_value(&page.events[0]).unwrap();
        assert_eq!(event["cutex"]["method"], EVENT);
        assert_eq!(event["cutex"]["params"]["removedTurns"], 2);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn result_and_event_match_the_registry() {
        super::super::contract_validation::validate_cutex_response(&json!({
            "contractVersion": 2,
            "requestId": "revert-1",
            "cutexSessionId": "cutex.s1",
            "cutex": { "method": METHOD, "result": sample_result() },
        }))
        .expect("revert response matches the registry");
        let mut event = event_params(&sample_result());
        super::super::contract_validation::validate_cutex_event_message(&json!({
            "method": EVENT, "params": event.clone(),
        }))
        .expect("historyReverted matches the registry");
        event["unexpected"] = json!(1);
        assert!(super::super::contract_validation::validate_cutex_event_message(&json!({
            "method": EVENT, "params": event,
        }))
        .is_err());
        super::super::contract_validation::validate_cutex_request(&json!({
            "requestId": "r", "method": METHOD, "params": { "beforeTurnId": "t" },
        }))
        .expect("request without expectedRevision is valid");
    }

    fn validate_result(result: &Value) -> Result<(), String> {
        super::super::contract_validation::validate_cutex_response(&json!({
            "contractVersion": 2, "requestId": "r", "cutexSessionId": "cutex.s1",
            "cutex": { "method": METHOD, "result": result },
        }))
    }

    /// Shape of `claude_structure::revert` (cli_app) around the adapter's
    /// `claude/thread/revert` receipt; the builder itself is checked there.
    fn claude_result() -> Value {
        let mut result = sample_result();
        result["runtimeRestarted"] = json!(true);
        result["runtimeState"] = json!("waiting");
        result["warnings"] = json!([]);
        result["native"] = json!({ "method": "claude/thread/revert", "result": {
            "threadId": "t", "removedTurnIds": ["turn-3", "turn-2"], "actionId": "revert-1",
            "state": "completed", "historyCommitted": true,
            "runtime": { "threadId": "t", "state": "waiting", "connectionId": "c2" } } });
        result
    }

    #[test]
    fn claude_results_carry_runtime_state_and_warnings() {
        validate_result(&claude_result()).expect("restarted Claude revert");
        let mut failed = claude_result();
        failed["runtimeRestarted"] = json!(false);
        failed["runtimeState"] = json!("failed");
        failed["warnings"] = json!([{ "code": "claude_runtime_restart_failed",
            "message": "History changed successfully, but the Claude runtime could not restart.",
            "runtimeState": "failed", "restartError": null }]);
        validate_result(&failed).expect("committed Claude revert whose restart failed");
        let mut no_transcript = claude_result();
        no_transcript["backupPath"] = Value::Null;
        no_transcript["runtimeState"] = Value::Null;
        validate_result(&no_transcript).expect("Claude revert without a native transcript");
        for key in ["runtimeState", "warnings"] {
            let mut missing = claude_result();
            missing.as_object_mut().unwrap().remove(key);
            assert!(validate_result(&missing).is_err(), "Claude revert without {key}");
        }
    }

    #[test]
    fn codex_results_keep_the_runtime_and_their_backup() {
        let mut restarted = sample_result();
        restarted["runtimeRestarted"] = json!(true);
        assert!(validate_result(&restarted).is_err());
        for (key, value) in [("runtimeState", json!("waiting")), ("warnings", json!([])),
            ("backupPath", Value::Null)]
        {
            let mut result = sample_result();
            result[key] = value;
            assert!(validate_result(&result).is_err(), "Codex revert with {key}");
        }
        let mut rollback = sample_result();
        rollback["native"]["method"] = json!("thread/rollback");
        validate_result(&rollback).expect("legacy rollback");
        rollback["native"]["method"] = json!("thread/fork");
        assert!(validate_result(&rollback).is_err());
    }
}
