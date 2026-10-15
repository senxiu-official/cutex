//! `cutex/agentBus/wake` (agent messaging v2): the owner wakes a session so
//! it reads and handles the notes other agents left it. It queues one owner
//! input: an idle session starts a turn now, a busy one after its current
//! turn, an offline one is brought online first (as `startIfOffline`). Every
//! pending note is read into that turn's context.

use serde_json::json;
use serde_json::Value;

use super::server::ValidatedCutexRequest;
use super::user_input::UserInputExecutionError;
use crate::management::control_plane::HumanManagementPrincipal;
use crate::management::server::ManagementRequestContext;

pub const METHOD: &str = "cutex/agentBus/wake";
/// The owner input the wake sends.
pub const WAKE_TEXT: &str = "Read and handle the unread notes other agents left for you.";
const CLIENT_ID: &str = "cutex-agent-bus-wake";

pub(super) fn dispatch(
    event_repository: &super::repository::EventRepository,
    context: ManagementRequestContext,
    owner: Option<HumanManagementPrincipal>,
    cutex_session_id: &str,
    session: &Value,
    request: &ValidatedCutexRequest,
) -> Result<Value, UserInputExecutionError> {
    let pending = || {
        super::agent_bus_state::agent_bus_message_repository()
            .and_then(|repository| repository.pending_note_count(cutex_session_id))
            .map_err(|error| UserInputExecutionError {
                stage: "route".into(),
                code: "event_persistence_unavailable".into(),
                message: format!("agent bus state unavailable: {error:#}"),
                retryable: true,
                details: json!({}),
                outcome_unknown: false,
            })
    };
    dispatch_with(owner.is_some(), session, request, pending, |submit| {
        super::offline_submit::dispatch(event_repository, context, owner, cutex_session_id, session, submit)
    })
}

pub(super) fn dispatch_with(
    root: bool,
    session: &Value,
    request: &ValidatedCutexRequest,
    pending: impl FnOnce() -> Result<usize, UserInputExecutionError>,
    submit: impl FnOnce(&ValidatedCutexRequest) -> Result<Value, UserInputExecutionError>,
) -> Result<Value, UserInputExecutionError> {
    if !root {
        return Err(UserInputExecutionError {
            stage: "route".into(),
            code: "cutex_method_denied".into(),
            message: "cutex/agentBus/wake requires the Management root credential".into(),
            retryable: false,
            details: json!({}),
            outcome_unknown: false,
        });
    }
    if !(request.params.is_null() || request.params.as_object().is_some_and(|params| params.is_empty())) {
        return Err(super::server::invalid_user_input_error("cutex/agentBus/wake params must be {}"));
    }
    let pending_notes = pending()?;
    // Busy: after the current turn (never steered into it). Idle: now.
    let busy = session.pointer("/runtime/activeTurnId").is_some_and(|turn| !turn.is_null());
    let input = ValidatedCutexRequest {
        request_id: request.request_id.clone(),
        method: "cutex/userInput/submit".into(),
        params: json!({
            "clientUserMessageId": format!("agent-bus-wake-{}", request.request_id),
            "origin": {"kind": "other", "clientId": CLIENT_ID},
            "strategy": if busy { "queue" } else { "auto" },
            "input": [{"type": "text", "text": WAKE_TEXT, "text_elements": []}],
            "startIfOffline": true,
        }),
    };
    let submitted = submit(&input)?;
    let started = matches!(submitted.get("disposition").and_then(Value::as_str), Some("started" | "steered"));
    Ok(json!({"pendingNotes": pending_notes, "started": started, "submit": submitted}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(params: Value) -> ValidatedCutexRequest {
        ValidatedCutexRequest { request_id: "req-1".into(), method: METHOD.into(), params }
    }

    fn session(active_turn: Option<&str>) -> Value {
        json!({"runtime": {"activeTurnId": active_turn, "appServerConnected": true}, "native": {"threadId": "t"}})
    }

    /// Owner decision 2026-10-08 item 5.
    #[test]
    fn wake_is_root_only_and_queues_one_owner_input() {
        let denied = dispatch_with(false, &session(None), &request(json!({})), || Ok(0), |_| unreachable!()).unwrap_err();
        assert_eq!(denied.code, "cutex_method_denied");
        let invalid = dispatch_with(true, &session(None), &request(json!({"x": 1})), || Ok(0), |_| unreachable!()).unwrap_err();
        assert_eq!(invalid.code, "invalid_request");
        // Idle: auto (starts a turn now); offline sessions start first.
        let mut seen = None;
        let result = dispatch_with(true, &session(None), &request(json!({})), || Ok(2), |submit| {
            seen = Some(submit.clone());
            Ok(json!({"disposition": "started"}))
        })
        .unwrap();
        assert_eq!(result["pendingNotes"], 2);
        assert_eq!(result["started"], true);
        let seen = seen.unwrap();
        assert_eq!(seen.method, "cutex/userInput/submit");
        assert_eq!(seen.params["strategy"], "auto");
        assert_eq!(seen.params["startIfOffline"], true);
        assert_eq!(seen.params["input"][0]["text"], WAKE_TEXT);
        assert_eq!(seen.params["clientUserMessageId"], "agent-bus-wake-req-1");
        // Busy: queued after the current turn, never steered.
        let result = dispatch_with(true, &session(Some("turn-1")), &request(Value::Null), || Ok(0), |submit| {
            assert_eq!(submit.params["strategy"], "queue");
            Ok(json!({"disposition": "queued"}))
        })
        .unwrap();
        assert_eq!((result["pendingNotes"].as_u64(), result["started"].as_bool()), (Some(0), Some(false)));
    }
}
