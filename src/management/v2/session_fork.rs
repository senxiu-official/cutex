//! `cutex/session/fork {afterTurnId?, name?}`: fork an agent as a NEW agent.
//!
//! The app layer (`ManagementRequestContext::fork_session`) forks the bound
//! native thread from storage (native `thread/fork` with `lastTurnId` =
//! `afterTurnId`, inclusive) on a private short-lived app-server on the
//! session's native home, adopts the forked thread as a new cutex session
//! with the source's kind (agent / quick chat), profile, model, reasoning and
//! launch permissions, cwd and project, and leaves the source untouched
//! (its runtime, if online, keeps running). Without `afterTurnId` the fork
//! holds the source's whole history up to its newest completed turn.
//!
//! The result carries the new session resource; `cutex/session/forked` is
//! emitted on the SOURCE session's stream.

use serde_json::{json, Value};

use crate::management::control_plane::HumanManagementPrincipal;
use crate::management::server::ManagementRequestContext;

use super::repository::EventRepository;
use super::server::{
    append_cutex_event, invalid_user_input_error, post_operation_persistence_error,
    ValidatedCutexRequest,
};
use super::user_input::UserInputExecutionError;

pub const METHOD: &str = "cutex/session/fork";
pub const EVENT: &str = "cutex/session/forked";
pub const MAX_NAME_CHARS: usize = 128;

/// Input of the app-layer fork handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionForkRequest {
    /// Last turn the fork keeps (inclusive); `None`: newest completed turn.
    pub after_turn_id: Option<String>,
    /// Name of the new agent; `None`: `"<source name> (fork)"`.
    pub name: Option<String>,
}

/// Default name of a fork; `taken` names get a numbered suffix.
pub fn default_fork_name(source: &str, taken: impl Fn(&str) -> bool) -> String {
    let source = source.trim();
    let base_source = if source.is_empty() { "Agent" } else { source };
    for attempt in 1..1000u32 {
        let suffix = if attempt == 1 {
            " (fork)".to_string()
        } else {
            format!(" (fork {attempt})")
        };
        let room = MAX_NAME_CHARS.saturating_sub(suffix.chars().count());
        let mut base: String = base_source.chars().take(room).collect();
        base = base.trim_end().to_string();
        let candidate = format!("{base}{suffix}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    format!("{} (fork {})", base_source.chars().take(80).collect::<String>(), uuid::Uuid::new_v4().simple())
}

pub(super) fn parse_params(params: &Value) -> Result<SessionForkRequest, UserInputExecutionError> {
    let object = params
        .as_object()
        .ok_or_else(|| invalid_user_input_error("params must be an object"))?;
    if let Some(key) = object
        .keys()
        .find(|key| !matches!(key.as_str(), "afterTurnId" | "name"))
    {
        return Err(invalid_user_input_error(&format!("unknown parameter {key}")));
    }
    let after_turn_id = match object.get("afterTurnId") {
        None | Some(Value::Null) => None,
        Some(Value::String(turn)) if !turn.trim().is_empty() => Some(turn.trim().to_string()),
        Some(_) => {
            return Err(invalid_user_input_error(
                "afterTurnId must be a non-empty string or null",
            ))
        }
    };
    let name = match object.get("name") {
        None | Some(Value::Null) => None,
        Some(Value::String(name))
            if !name.trim().is_empty()
                && name.trim().chars().count() <= MAX_NAME_CHARS
                && !name.chars().any(char::is_control) =>
        {
            Some(name.trim().to_string())
        }
        Some(_) => {
            return Err(invalid_user_input_error(&format!(
                "name must be a non-empty string of at most {MAX_NAME_CHARS} characters without control characters"
            )))
        }
    };
    Ok(SessionForkRequest {
        after_turn_id,
        name,
    })
}

pub(super) fn dispatch(
    event_repository: &EventRepository,
    context: ManagementRequestContext,
    owner: Option<&HumanManagementPrincipal>,
    cutex_session_id: &str,
    session: &Value,
    request: &ValidatedCutexRequest,
) -> Result<Value, UserInputExecutionError> {
    let Some(owner) = owner else {
        return Err(UserInputExecutionError {
            stage: "route".into(),
            code: "cutex_method_denied".into(),
            message: "session fork requires the Management root credential".into(),
            retryable: false,
            details: json!({}),
            outcome_unknown: false,
        });
    };
    if session.get("lifecycle").and_then(Value::as_str) == Some("retired") {
        return Err(UserInputExecutionError {
            stage: "route".into(),
            code: "session_fork_unsupported".into(),
            message: "restore the archived session before forking it".into(),
            retryable: false,
            details: json!({}),
            outcome_unknown: false,
        });
    }
    let fork = parse_params(&request.params)?;
    // Asks (委托) in the runtime are native obligations: refuse, naming them.
    crate::asks::lifecycle::check_no_obligations_default(cutex_session_id, "fork")
        .map_err(super::asks::operation_error)?;
    let facts = (context.fork_session)(owner, cutex_session_id, &fork);
    super::history::invalidate_session(cutex_session_id);
    let facts = facts?;
    let new_id = facts
        .get("newCutexSessionId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty() && *id != cutex_session_id)
        .ok_or_else(|| {
            post_operation_persistence_error(anyhow::anyhow!(
                "session fork returned no new cutex session id"
            ))
        })?
        .to_string();
    let registry = (context.load_registry)().map_err(post_operation_persistence_error)?;
    let new_session = super::session::session_resource_including_hidden(
        &new_id,
        &registry,
        context.load_runtime_status,
        event_repository,
    )
    .map_err(post_operation_persistence_error)?
    .ok_or_else(|| {
        post_operation_persistence_error(anyhow::anyhow!(
            "forked session {new_id} is not readable after creation"
        ))
    })?;
    let result = fork_result(cutex_session_id, &facts, new_session);
    append_cutex_event(
        event_repository,
        cutex_session_id,
        super::model::EventCorrelation {
            thread_id: result["sourceThreadId"].as_str().map(str::to_string),
            management_request_id: Some(request.request_id.clone()),
            ..Default::default()
        },
        EVENT,
        event_params(&result),
    )
    .map_err(post_operation_persistence_error)?;
    Ok(result)
}

/// The response; `warnings` (problems after the fork was committed, e.g. the
/// Claude source runtime did not restart) is passed through when the backend
/// reports it.
pub fn fork_result(cutex_session_id: &str, facts: &Value, session: Value) -> Value {
    let field = |key: &str| facts.get(key).cloned().unwrap_or(Value::Null);
    let mut result = json!({
        "sourceCutexSessionId": cutex_session_id,
        "newCutexSessionId": field("newCutexSessionId"),
        "sourceThreadId": field("sourceThreadId"),
        "threadId": field("threadId"),
        "forkedThroughTurnId": field("forkedThroughTurnId"),
        "name": field("name"),
        "session": session,
    });
    if let Some(warnings) = facts.get("warnings").filter(|warnings| warnings.is_array()) {
        result["warnings"] = warnings.clone();
    }
    result
}

pub(super) fn event_params(result: &Value) -> Value {
    json!({
        "sourceCutexSessionId": result["sourceCutexSessionId"],
        "newCutexSessionId": result["newCutexSessionId"],
        "sourceThreadId": result["sourceThreadId"],
        "threadId": result["threadId"],
        "forkedThroughTurnId": result["forkedThroughTurnId"],
        "forkedAt": chrono::Utc::now().to_rfc3339(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_accept_optional_turn_and_name() {
        assert_eq!(
            parse_params(&json!({})).unwrap(),
            SessionForkRequest { after_turn_id: None, name: None }
        );
        assert_eq!(
            parse_params(&json!({ "afterTurnId": " turn-2 ", "name": " Side quest " })).unwrap(),
            SessionForkRequest {
                after_turn_id: Some("turn-2".into()),
                name: Some("Side quest".into())
            }
        );
        assert_eq!(
            parse_params(&json!({ "afterTurnId": null, "name": null })).unwrap(),
            SessionForkRequest { after_turn_id: None, name: None }
        );
        for bad in [
            json!({ "afterTurnId": "" }),
            json!({ "afterTurnId": 3 }),
            json!({ "name": "" }),
            json!({ "name": "a\nb" }),
            json!({ "name": "x".repeat(129) }),
            json!({ "beforeTurnId": "t" }),
            json!("t"),
        ] {
            assert!(parse_params(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn default_names_append_fork_and_number_collisions() {
        assert_eq!(default_fork_name("Researcher", |_| false), "Researcher (fork)");
        assert_eq!(
            default_fork_name("Researcher", |name| name == "Researcher (fork)"),
            "Researcher (fork 2)"
        );
        assert_eq!(default_fork_name("  ", |_| false), "Agent (fork)");
        let long = "n".repeat(200);
        let name = default_fork_name(&long, |_| false);
        assert_eq!(name.chars().count(), MAX_NAME_CHARS);
        assert!(name.ends_with(" (fork)"));
    }

    fn facts() -> Value {
        json!({
            "newCutexSessionId": "cutex.s2",
            "sourceThreadId": "018a0000-0000-7000-8000-000000000001",
            "threadId": "018a0000-0000-7000-8000-000000000002",
            "forkedThroughTurnId": "turn-2",
            "name": "Researcher (fork)",
        })
    }

    #[test]
    fn result_and_event_match_the_registry() {
        let result = fork_result("cutex.s1", &facts(), json!({ "cutexSessionId": "cutex.s2" }));
        assert_eq!(result["sourceCutexSessionId"], "cutex.s1");
        assert_eq!(result["session"]["cutexSessionId"], "cutex.s2");
        super::super::contract_validation::validate_cutex_response(&json!({
            "contractVersion": 2, "requestId": "r", "cutexSessionId": "cutex.s1",
            "cutex": { "method": METHOD, "result": result.clone() },
        }))
        .expect("fork response matches the registry");
        let mut event = event_params(&result);
        super::super::contract_validation::validate_cutex_event_message(&json!({
            "method": EVENT, "params": event.clone(),
        }))
        .expect("forked event matches the registry");
        event["forkedThroughTurnId"] = Value::Null;
        super::super::contract_validation::validate_cutex_event_message(&json!({
            "method": EVENT, "params": event,
        }))
        .expect("fork of a thread without completed turns");
        for params in [json!({}), json!({ "afterTurnId": "t", "name": "n" }), json!({ "afterTurnId": null })] {
            super::super::contract_validation::validate_cutex_request(&json!({
                "requestId": "r", "method": METHOD, "params": params,
            }))
            .expect("fork request is valid");
        }
        assert!(super::super::contract_validation::validate_cutex_request(&json!({
            "requestId": "r", "method": METHOD, "params": { "sessionId": "x" },
        }))
        .is_err());
    }

    #[test]
    fn backend_warnings_reach_the_response_and_match_the_registry() {
        let validate = |result: &Value| {
            super::super::contract_validation::validate_cutex_response(&json!({
                "contractVersion": 2, "requestId": "r", "cutexSessionId": "cutex.s1",
                "cutex": { "method": METHOD, "result": result },
            }))
        };
        // Codex forks report no warnings: the response keeps its old shape.
        let plain = fork_result("cutex.s1", &facts(), json!({}));
        assert!(plain.get("warnings").is_none());
        // Claude forks always report the list (empty or not).
        let mut claude = facts();
        claude["warnings"] = json!([]);
        assert_eq!(fork_result("cutex.s1", &claude, json!({}))["warnings"], json!([]));
        let warning = json!({
            "code": "claude_runtime_restart_failed",
            "message": "History changed successfully, but the Claude runtime could not restart.",
            "runtimeState": "failed", "restartError": "ProviderError",
        });
        claude["warnings"] = json!([warning.clone()]);
        let result = fork_result("cutex.s1", &claude, json!({ "cutexSessionId": "cutex.s2" }));
        assert_eq!(result["warnings"], json!([warning]));
        validate(&result).expect("fork response with a warning matches the registry");
        let mut unknown = result.clone();
        unknown["warnings"][0]["unexpected"] = json!(1);
        assert!(validate(&unknown).is_err());
        // The forked event is unchanged.
        assert!(event_params(&result).get("warnings").is_none());
    }
}
