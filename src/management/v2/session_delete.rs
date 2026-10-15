//! `cutex/session/delete {expectedRevision?}`: permanently delete a cutex
//! session (durable record + native thread), and the delete-empty step of
//! `cutex/session/retire` (settings `archiveDeleteEmpty`, default true).
//!
//! The app layer (`ManagementRequestContext::delete_session`) refuses a
//! session that is still referenced (Director / Operator / owned services:
//! `session_referenced`; a plain project membership is dropped with it) or busy
//! (active turn or launch in progress: `session_busy`), stops a running
//! runtime, saves a recovery copy of the record and rollout(s) under
//! `runtime/session-deletes/<UTC stamp>-<id>/`, deletes the native thread
//! (native `thread/delete` on a private app-server on the session's native
//! home; storage move as fallback) and finally removes the record.
//!
//! Emits `cutex/session/deleted` (on the deleted session's stream) after a
//! confirmed delete, also when a retire deleted an empty session
//! (`viaRetire: true`, no `cutex/session/retired` then).

use serde_json::{json, Value};

use crate::management::control_plane::HumanManagementPrincipal;
use crate::management::server::{ManagementSessionDeleteHandler, ManagementSessionMutationHandler};

use super::repository::EventRepository;
use super::server::{
    append_cutex_event, invalid_user_input_error, post_operation_persistence_error,
    ValidatedCutexRequest,
};
use super::user_input::UserInputExecutionError;

pub const METHOD: &str = "cutex/session/delete";
pub const EVENT: &str = "cutex/session/deleted";

/// Input of the app-layer delete handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDeleteRequest {
    /// Durable revision the caller saw; `None` applies to the current one.
    pub expected_revision: Option<u64>,
    /// Retire path: delete only a session that never received input and
    /// return `{deleted: false}` (nothing changed) for any other session.
    pub only_if_empty: bool,
}

pub(super) fn parse_params(params: &Value) -> Result<Option<u64>, UserInputExecutionError> {
    let object = params
        .as_object()
        .ok_or_else(|| invalid_user_input_error("params must be an object"))?;
    if let Some(key) = object.keys().find(|key| key.as_str() != "expectedRevision") {
        return Err(invalid_user_input_error(&format!("unknown parameter {key}")));
    }
    match object.get("expectedRevision") {
        None | Some(Value::Null) => Ok(None),
        Some(_) => super::server::required_safe_integer_param(object, "expectedRevision").map(Some),
    }
}

fn require_owner(owner: Option<&HumanManagementPrincipal>) -> Result<(), UserInputExecutionError> {
    if owner.is_some() {
        return Ok(());
    }
    Err(UserInputExecutionError {
        stage: "route".into(),
        code: "cutex_method_denied".into(),
        message: "session delete requires the Management root credential".into(),
        retryable: false,
        details: json!({}),
        outcome_unknown: false,
    })
}

pub(super) fn dispatch(
    event_repository: &EventRepository,
    delete: ManagementSessionDeleteHandler,
    owner: Option<&HumanManagementPrincipal>,
    cutex_session_id: &str,
    session: &Value,
    request: &ValidatedCutexRequest,
) -> Result<Value, UserInputExecutionError> {
    require_owner(owner)?;
    let expected_revision = parse_params(&request.params)?;
    super::archive::require_archive_revision(
        session,
        request.params.as_object().expect("validated object"),
    )?;
    let result = delete(
        cutex_session_id,
        &SessionDeleteRequest {
            expected_revision,
            only_if_empty: false,
        },
    )?;
    validate_receipt(&result, cutex_session_id)?;
    super::attachments::remove_session_attachments(cutex_session_id);
    append_deleted_event(event_repository, cutex_session_id, session, request, &result, false)?;
    Ok(result)
}

/// `cutex/session/retire`: with `archiveDeleteEmpty` on (and the root
/// credential), an active session that never received input is deleted
/// instead of retired; the result is the delete receipt (`deleted: true`).
/// Anything else is the ordinary retire.
pub(super) fn dispatch_retire(
    event_repository: &EventRepository,
    delete: ManagementSessionDeleteHandler,
    mutate_session: ManagementSessionMutationHandler,
    owner: Option<&HumanManagementPrincipal>,
    cutex_session_id: &str,
    session: &Value,
    request: &ValidatedCutexRequest,
) -> Result<Value, UserInputExecutionError> {
    let object = super::archive::check_retire_fences(session, request)?;
    let active = session.get("lifecycle").and_then(Value::as_str) != Some("retired");
    if owner.is_some() && active && delete_empty_enabled() {
        let expected_revision = match object.get("expectedRevision") {
            None | Some(Value::Null) => None,
            Some(value) => value.as_u64(),
        };
        match delete(
            cutex_session_id,
            &SessionDeleteRequest {
                expected_revision,
                only_if_empty: true,
            },
        ) {
            Ok(result) if result.get("deleted").and_then(Value::as_bool) == Some(true) => {
                validate_receipt(&result, cutex_session_id)?;
                super::attachments::remove_session_attachments(cutex_session_id);
                append_deleted_event(event_repository, cutex_session_id, session, request, &result, true)?;
                return Ok(result);
            }
            Ok(_) => {}
            // A refused delete changed nothing: archive as usual.
            Err(error) if retire_falls_back(&error) => {}
            Err(error) => return Err(error),
        }
    }
    super::archive::dispatch_session_retire(
        event_repository,
        mutate_session,
        cutex_session_id,
        session,
        request,
    )
}

/// Delete refusals that leave the session unchanged, so the retire proceeds.
fn retire_falls_back(error: &UserInputExecutionError) -> bool {
    !error.outcome_unknown
        && matches!(
            error.code.as_str(),
            "session_referenced" | "session_busy" | "session_delete_unsupported" | "session_delete_failed"
        )
}

fn delete_empty_enabled() -> bool {
    crate::config::store::load_codez_config_checked()
        .map(|config| config.archive_delete_empty)
        .unwrap_or(false)
}

fn validate_receipt(result: &Value, cutex_session_id: &str) -> Result<(), UserInputExecutionError> {
    let valid = result.get("deleted").and_then(Value::as_bool) == Some(true)
        && result.get("cutexSessionId").and_then(Value::as_str) == Some(cutex_session_id)
        && result.get("backupPath").and_then(Value::as_str).is_some()
        && result.get("deletedAt").and_then(Value::as_str).is_some();
    if valid {
        Ok(())
    } else {
        Err(post_operation_persistence_error(anyhow::anyhow!(
            "session delete returned an invalid receipt"
        )))
    }
}

fn append_deleted_event(
    event_repository: &EventRepository,
    cutex_session_id: &str,
    session: &Value,
    request: &ValidatedCutexRequest,
    result: &Value,
    via_retire: bool,
) -> Result<(), UserInputExecutionError> {
    append_cutex_event(
        event_repository,
        cutex_session_id,
        super::model::EventCorrelation {
            thread_id: session
                .pointer("/native/threadId")
                .and_then(Value::as_str)
                .map(str::to_string),
            management_request_id: Some(request.request_id.clone()),
            ..Default::default()
        },
        EVENT,
        event_params(cutex_session_id, result, via_retire),
    )
    .map_err(post_operation_persistence_error)
}

pub(super) fn event_params(cutex_session_id: &str, result: &Value, via_retire: bool) -> Value {
    json!({
        "cutexSessionId": cutex_session_id,
        "threadId": result.get("threadId").cloned().unwrap_or(Value::Null),
        "viaRetire": via_retire,
        "backupPath": result.get("backupPath").cloned().unwrap_or(Value::Null),
        "deletedAt": result.get("deletedAt").cloned().unwrap_or(Value::Null),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static CALLS: Mutex<Vec<(String, SessionDeleteRequest)>> = Mutex::new(Vec::new());

    fn receipt(id: &str) -> Value {
        json!({
            "cutexSessionId": id,
            "deleted": true,
            "threadId": "018a0000-0000-7000-8000-000000000001",
            "nativeDelete": "thread/delete",
            "runtimeStopped": false,
            "backupPath": "C:/x/session-deletes/20261007T000000Z-cutex.s1",
            "deletedAt": "2026-10-07T00:00:00Z",
        })
    }

    fn ok_handler(id: &str, request: &SessionDeleteRequest) -> Result<Value, UserInputExecutionError> {
        CALLS.lock().unwrap().push((id.to_string(), request.clone()));
        Ok(receipt(id))
    }

    fn refusing_handler(_: &str, _: &SessionDeleteRequest) -> Result<Value, UserInputExecutionError> {
        panic!("handler must not run")
    }

    fn request(method: &str, params: Value) -> ValidatedCutexRequest {
        ValidatedCutexRequest {
            request_id: "delete-req-1".into(),
            method: method.into(),
            params,
        }
    }

    fn repository() -> (std::path::PathBuf, EventRepository) {
        let root = std::env::temp_dir().join(format!("cutex-v2-delete-{}", uuid::Uuid::new_v4()));
        let repository = EventRepository::open(&root, crate::platform::host::current_host_name())
            .expect("open event repository");
        (root, repository)
    }

    #[test]
    fn params_accept_only_optional_expected_revision() {
        assert_eq!(parse_params(&json!({})).unwrap(), None);
        assert_eq!(parse_params(&json!({ "expectedRevision": null })).unwrap(), None);
        assert_eq!(parse_params(&json!({ "expectedRevision": 4 })).unwrap(), Some(4));
        for bad in [
            json!({ "expectedRevision": "4" }),
            json!({ "sessionId": "cutex.s1" }),
            json!({ "expectedRevision": 4, "force": true }),
            json!([]),
        ] {
            assert!(parse_params(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn delete_requires_root_checks_revision_and_emits_deleted_event() {
        let (root, repository) = repository();
        let session = json!({ "revision": 7, "native": { "threadId": "t-1" } });
        let denied = dispatch(&repository, refusing_handler, None, "cutex.s1", &session,
            &request(METHOD, json!({}))).unwrap_err();
        assert_eq!(denied.code, "cutex_method_denied");
        let owner = HumanManagementPrincipal::authenticated();
        let conflict = dispatch(&repository, refusing_handler, Some(&owner), "cutex.s1", &session,
            &request(METHOD, json!({ "expectedRevision": 6 }))).unwrap_err();
        assert_eq!(conflict.code, "revision_conflict");
        let result = dispatch(&repository, ok_handler, Some(&owner), "cutex.s1", &session,
            &request(METHOD, json!({ "expectedRevision": 7 }))).unwrap();
        assert_eq!(result["deleted"], true);
        let calls = CALLS.lock().unwrap().clone();
        assert!(calls.iter().any(|(id, call)| id == "cutex.s1"
            && call == &SessionDeleteRequest { expected_revision: Some(7), only_if_empty: false }));
        let page = repository.page(super::super::repository::ReplayQuery::default()).unwrap();
        assert_eq!(page.events.len(), 1);
        let event = page.events[0].cutex.as_ref().unwrap();
        assert_eq!(event.method, EVENT);
        assert_eq!(event.params["viaRetire"], false);
        assert_eq!(event.params["threadId"], "018a0000-0000-7000-8000-000000000001");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn mismatched_receipt_is_rejected_before_the_event() {
        fn other(_: &str, _: &SessionDeleteRequest) -> Result<Value, UserInputExecutionError> {
            Ok(receipt("cutex.other"))
        }
        let (root, repository) = repository();
        let owner = HumanManagementPrincipal::authenticated();
        let error = dispatch(&repository, other, Some(&owner), "cutex.s1", &json!({ "revision": 1 }),
            &request(METHOD, json!({}))).unwrap_err();
        assert_eq!(error.code, "event_persistence_unavailable");
        assert!(repository.page(super::super::repository::ReplayQuery::default()).unwrap().events.is_empty());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn retire_without_root_or_of_a_retired_session_never_deletes() {
        fn retire(_: &str, _: &str, _: Value) -> Result<Value, UserInputExecutionError> {
            Ok(json!({
                "cutexSessionId": "cutex.s1", "revision": 8, "lifecycle": "retired",
                "runtimeGeneration": 0, "status": "offline", "retiredAt": "2026-10-07T00:00:00Z",
            }))
        }
        let (root, repository) = repository();
        let session = json!({ "revision": 7, "lifecycle": "active", "runtime": { "runtimeGeneration": 0 } });
        let result = dispatch_retire(&repository, refusing_handler, retire, None, "cutex.s1",
            &session, &request("cutex/session/retire", json!({}))).unwrap();
        assert_eq!(result["lifecycle"], "retired");
        let page = repository.page(super::super::repository::ReplayQuery::default()).unwrap();
        assert_eq!(page.events[0].cutex.as_ref().unwrap().method, "cutex/session/retired");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn only_refusals_that_changed_nothing_fall_back_to_retire() {
        let refusal = |code: &str, outcome_unknown: bool| UserInputExecutionError {
            stage: "route".into(),
            code: code.into(),
            message: String::new(),
            retryable: false,
            details: json!({}),
            outcome_unknown,
        };
        for code in ["session_referenced", "session_busy", "session_delete_unsupported", "session_delete_failed"] {
            assert!(retire_falls_back(&refusal(code, false)), "{code}");
        }
        assert!(!retire_falls_back(&refusal("persistence_uncertain", true)));
        assert!(!retire_falls_back(&refusal("session_delete_failed", true)));
        assert!(!retire_falls_back(&refusal("revision_conflict", false)));
    }

    #[test]
    fn delete_result_and_event_match_the_registry() {
        let validation = super::super::contract_validation::validate_cutex_response;
        validation(&json!({
            "contractVersion": 2, "requestId": "r", "cutexSessionId": "cutex.s1",
            "cutex": { "method": METHOD, "result": receipt("cutex.s1") },
        }))
        .expect("delete response matches the registry");
        validation(&json!({
            "contractVersion": 2, "requestId": "r", "cutexSessionId": "cutex.s1",
            "cutex": { "method": "cutex/session/retire", "result": receipt("cutex.s1") },
        }))
        .expect("retire may answer with a delete receipt");
        let mut event = event_params("cutex.s1", &receipt("cutex.s1"), true);
        super::super::contract_validation::validate_cutex_event_message(&json!({
            "method": EVENT, "params": event.clone(),
        }))
        .expect("deleted event matches the registry");
        event["unexpected"] = json!(1);
        assert!(super::super::contract_validation::validate_cutex_event_message(&json!({
            "method": EVENT, "params": event,
        }))
        .is_err());
        for params in [json!({}), json!({ "expectedRevision": 3 })] {
            super::super::contract_validation::validate_cutex_request(&json!({
                "requestId": "r", "method": METHOD, "params": params,
            }))
            .expect("delete request is valid");
        }
        assert!(super::super::contract_validation::validate_cutex_request(&json!({
            "requestId": "r", "method": METHOD, "params": { "force": true },
        }))
        .is_err());
    }
}
