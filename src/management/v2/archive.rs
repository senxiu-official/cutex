//! Management v2 archive query and Retire/Restore dispatch.

use std::net::TcpStream;

use serde_json::json;
use serde_json::Value;

use crate::http::server::write_json_response;
use crate::http::server::SimpleHttpRequest;
use crate::management::server::ManagementRequestContext;

use super::repository::EventRepository;
use super::server::append_cutex_event;
use super::server::invalid_user_input_error;
use super::server::post_operation_persistence_error;
use super::server::write_v2_error;
use super::server::ValidatedCutexRequest;
use super::session::retired_session_list_resource;
use super::session::session_list_resource_with_hidden;
use super::user_input::UserInputExecutionError;
use crate::management::server::ManagementSessionMutationHandler;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionLifecycleQuery {
    Active,
    Retired,
}

pub(super) fn session_lifecycle_query(path: &str) -> Result<SessionLifecycleQuery, anyhow::Error> {
    let Some((_, query)) = path.split_once('?') else {
        return Ok(SessionLifecycleQuery::Active);
    };
    let mut lifecycle = None;
    for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if name != "lifecycle" {
            anyhow::bail!("unsupported sessions query parameter: {name}");
        }
        if lifecycle.is_some() {
            anyhow::bail!("sessions query parameter lifecycle must not be repeated");
        }
        lifecycle = Some(value.into_owned());
    }
    match lifecycle.as_deref() {
        None | Some("active") => Ok(SessionLifecycleQuery::Active),
        Some("retired") => Ok(SessionLifecycleQuery::Retired),
        Some(value) => anyhow::bail!("lifecycle must be either active or retired, got {value}"),
    }
}

/// Collection query for `GET /v2/sessions`: `lifecycle` plus the root-only
/// `includeHidden` switch. Per-ID routes keep using [`session_lifecycle_query`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SessionCollectionQuery {
    pub(super) lifecycle: SessionLifecycleQuery,
    pub(super) include_hidden: bool,
}

pub(super) fn session_collection_query(
    path: &str,
) -> Result<SessionCollectionQuery, anyhow::Error> {
    let Some((_, query)) = path.split_once('?') else {
        return Ok(SessionCollectionQuery {
            lifecycle: SessionLifecycleQuery::Active,
            include_hidden: false,
        });
    };
    let mut lifecycle = None;
    let mut include_hidden = None;
    for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
        let slot = match name.as_ref() {
            "lifecycle" => &mut lifecycle,
            "includeHidden" => &mut include_hidden,
            _ => anyhow::bail!("unsupported sessions query parameter: {name}"),
        };
        if slot.is_some() {
            anyhow::bail!("sessions query parameter {name} must not be repeated");
        }
        *slot = Some(value.into_owned());
    }
    let lifecycle = match lifecycle.as_deref() {
        None | Some("active") => SessionLifecycleQuery::Active,
        Some("retired") => SessionLifecycleQuery::Retired,
        Some(value) => anyhow::bail!("lifecycle must be either active or retired, got {value}"),
    };
    let include_hidden = match include_hidden.as_deref() {
        None | Some("false") => false,
        Some("true") => true,
        Some(value) => anyhow::bail!("includeHidden must be true or false, got {value}"),
    };
    Ok(SessionCollectionQuery {
        lifecycle,
        include_hidden,
    })
}

pub(super) fn handle_session_collection_get(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    context: ManagementRequestContext,
    repository: &EventRepository,
    root: bool,
) -> anyhow::Result<()> {
    let query = match session_collection_query(&request.path) {
        Ok(query) => query,
        Err(error) => {
            return write_v2_error(
                stream,
                400,
                "Bad Request",
                "invalid_request",
                &error.to_string(),
                false,
                json!({}),
            )
        }
    };
    if query.include_hidden && !root {
        return write_v2_error(
            stream,
            403,
            "Forbidden",
            "root_credential_required",
            "includeHidden requires the Management root credential",
            false,
            json!({}),
        );
    }
    let resource = match query.lifecycle {
        SessionLifecycleQuery::Active => {
            let registry = (context.load_registry)()?;
            session_list_resource_with_hidden(
                &registry,
                context.load_runtime_status,
                repository,
                query.include_hidden,
            )?
        }
        SessionLifecycleQuery::Retired => retired_session_list_resource()?,
    };
    write_json_response(stream, 200, "OK", &resource)
}

pub(super) fn dispatch_session_retire(
    event_repository: &EventRepository,
    mutate_session: ManagementSessionMutationHandler,
    cutex_session_id: &str,
    session: &Value,
    request: &ValidatedCutexRequest,
) -> Result<Value, UserInputExecutionError> {
    let object = check_retire_fences(session, request)?;
    let result = mutate_session(cutex_session_id, &request.method, request.params.clone())?;
    validate_archive_transition_result(&result, cutex_session_id, "retired")?;
    let mut event_params = json!({
        "cutexSessionId": cutex_session_id,
        "sessionRevision": result["revision"],
        "runtimeGeneration": result["runtimeGeneration"],
        "retiredAt": result["retiredAt"],
    });
    if let Some(reason) = object.get("reason").and_then(Value::as_str) {
        event_params
            .as_object_mut()
            .expect("retired event params are an object")
            .insert("reason".to_string(), Value::String(reason.to_string()));
    }
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
        "cutex/session/retired",
        event_params,
    )
    .map_err(post_operation_persistence_error)?;
    Ok(result)
}

/// Validates retire params and their optional fences (checked when supplied,
/// otherwise the call applies to the current state).
pub(super) fn check_retire_fences<'a>(
    session: &Value,
    request: &'a ValidatedCutexRequest,
) -> Result<&'a serde_json::Map<String, Value>, UserInputExecutionError> {
    let object = retire_params(&request.params)?;
    require_archive_revision(session, object)?;
    if object
        .get("expectedRuntimeGeneration")
        .is_some_and(|value| !value.is_null())
    {
        let expected_runtime_generation =
            super::server::required_safe_integer_param(object, "expectedRuntimeGeneration")?;
        let current_runtime_generation = session
            .pointer("/runtime/runtimeGeneration")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if expected_runtime_generation != current_runtime_generation {
            return Err(stale_runtime_fence_error(
                expected_runtime_generation,
                current_runtime_generation,
            ));
        }
    }
    Ok(object)
}

fn retire_params(
    params: &Value,
) -> Result<&serde_json::Map<String, Value>, UserInputExecutionError> {
    let object = params
        .as_object()
        .ok_or_else(|| invalid_user_input_error("params must be an object"))?;
    if object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "expectedRevision" | "expectedRuntimeGeneration" | "reason"
            )
        })
        || object
            .get("reason")
            .is_some_and(|reason| !reason.is_string())
    {
        return Err(invalid_user_input_error(
            "retire params accept only optional expectedRevision, expectedRuntimeGeneration and reason",
        ));
    }
    Ok(object)
}

pub(super) fn dispatch_session_restore(
    event_repository: &EventRepository,
    mutate_session: ManagementSessionMutationHandler,
    cutex_session_id: &str,
    session: &Value,
    request: &ValidatedCutexRequest,
) -> Result<Value, UserInputExecutionError> {
    let object = request
        .params
        .as_object()
        .filter(|object| object.keys().all(|key| key == "expectedRevision"))
        .ok_or_else(|| {
            invalid_user_input_error("restore params accept only an optional expectedRevision")
        })?;
    require_archive_revision(session, object)?;
    let result = mutate_session(cutex_session_id, &request.method, request.params.clone())?;
    validate_archive_transition_result(&result, cutex_session_id, "active")?;
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
        "cutex/session/restored",
        json!({
            "cutexSessionId": cutex_session_id,
            "sessionRevision": result["revision"],
            "runtimeGeneration": result["runtimeGeneration"],
            "restoredAt": chrono::Utc::now().to_rfc3339(),
        }),
    )
    .map_err(post_operation_persistence_error)?;
    Ok(result)
}

pub(super) fn require_archive_revision(
    session: &Value,
    params: &serde_json::Map<String, Value>,
) -> Result<(), UserInputExecutionError> {
    if params.get("expectedRevision").is_none_or(Value::is_null) {
        return Ok(());
    }
    let expected = super::server::required_safe_integer_param(params, "expectedRevision")?;
    let current = session.get("revision").and_then(Value::as_u64).unwrap_or(0);
    if expected != current {
        return Err(UserInputExecutionError {
            stage: "route".to_string(),
            code: "revision_conflict".to_string(),
            message: format!(
                "durable session revision conflict: expected {expected}, current {current}"
            ),
            retryable: true,
            details: json!({
                "expectedRevision": expected,
                "currentRevision": current,
                "resyncRequired": true,
            }),
            outcome_unknown: false,
        });
    }
    Ok(())
}

fn stale_runtime_fence_error(expected: u64, current: u64) -> UserInputExecutionError {
    UserInputExecutionError {
        stage: "runtime".to_string(),
        code: "revision_conflict".to_string(),
        message: format!("runtime generation conflict: expected {expected}, current {current}"),
        retryable: true,
        details: json!({
            "expectedRuntimeGeneration": expected,
            "currentRuntimeGeneration": current,
            "resyncRequired": true,
        }),
        outcome_unknown: false,
    }
}

fn validate_archive_transition_result(
    result: &Value,
    expected_cutex_session_id: &str,
    expected_lifecycle: &str,
) -> Result<(), UserInputExecutionError> {
    let object = result
        .as_object()
        .ok_or_else(|| invalid_archive_receipt("archive mutation returned a non-object result"))?;
    let required = [
        "cutexSessionId",
        "revision",
        "lifecycle",
        "runtimeGeneration",
        "status",
        "retiredAt",
    ];
    if object.keys().any(|key| !required.contains(&key.as_str()))
        || required.iter().any(|key| !object.contains_key(*key))
        || object.get("cutexSessionId").and_then(Value::as_str) != Some(expected_cutex_session_id)
        || object
            .get("revision")
            .and_then(Value::as_u64)
            .is_none_or(|revision| revision == 0 || revision > super::model::MAX_SAFE_SEQUENCE)
        || object
            .get("runtimeGeneration")
            .and_then(Value::as_u64)
            .is_none_or(|generation| generation > super::model::MAX_SAFE_SEQUENCE)
        || object.get("lifecycle").and_then(Value::as_str) != Some(expected_lifecycle)
        || object.get("status").and_then(Value::as_str) != Some("offline")
    {
        return Err(invalid_archive_receipt(
            "archive mutation returned an invalid transition receipt",
        ));
    }
    if expected_lifecycle == "retired" && object.get("retiredAt").and_then(Value::as_str).is_none()
    {
        return Err(invalid_archive_receipt("retire mutation omitted retiredAt"));
    }
    if expected_lifecycle == "active" && !object.get("retiredAt").is_some_and(Value::is_null) {
        return Err(invalid_archive_receipt(
            "restore mutation must clear retiredAt",
        ));
    }
    Ok(())
}

fn invalid_archive_receipt(message: &str) -> UserInputExecutionError {
    post_operation_persistence_error(anyhow::anyhow!(message.to_string()))
}

#[cfg(test)]
mod tests {
    use super::super::repository::ReplayQuery;
    use super::*;

    fn retire_transition(
        cutex_session_id: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, UserInputExecutionError> {
        assert_eq!(cutex_session_id, "cutex-session-1");
        assert_eq!(method, "cutex/session/retire");
        assert_eq!(params["expectedRevision"], 4);
        assert_eq!(params["expectedRuntimeGeneration"], 7);
        assert_eq!(params["reason"], "owner_cleanup");
        Ok(json!({
            "cutexSessionId": "cutex-session-1",
            "revision": 5,
            "lifecycle": "retired",
            "runtimeGeneration": 7,
            "status": "offline",
            "retiredAt": "2026-08-10T00:01:00Z"
        }))
    }

    fn restore_transition(
        cutex_session_id: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, UserInputExecutionError> {
        assert_eq!(cutex_session_id, "cutex-session-1");
        assert_eq!(method, "cutex/session/restore");
        assert_eq!(params, json!({ "expectedRevision": 5 }));
        Ok(json!({
            "cutexSessionId": "cutex-session-1",
            "revision": 6,
            "lifecycle": "active",
            "runtimeGeneration": 7,
            "status": "offline",
            "retiredAt": null
        }))
    }

    fn mismatched_transition(
        _cutex_session_id: &str,
        _method: &str,
        _params: Value,
    ) -> Result<Value, UserInputExecutionError> {
        Ok(json!({
            "cutexSessionId": "cutex-session-other",
            "revision": 5,
            "lifecycle": "retired",
            "runtimeGeneration": 7,
            "status": "offline",
            "retiredAt": "2026-08-10T00:01:00Z"
        }))
    }

    fn session(lifecycle: &str, revision: u64) -> Value {
        json!({
            "cutexSessionId": "cutex-session-1",
            "revision": revision,
            "lifecycle": lifecycle,
            "retiredAt": if lifecycle == "retired" {
                json!("2026-08-10T00:01:00Z")
            } else {
                Value::Null
            },
            "native": { "threadId": "thread-1" },
            "runtime": {
                "runtimeGeneration": 7,
                "status": "offline",
                "runtimeAgentId": null
            }
        })
    }

    fn repository(label: &str) -> (std::path::PathBuf, EventRepository) {
        let root =
            std::env::temp_dir().join(format!("cutex-v2-archive-{label}-{}", uuid::Uuid::new_v4()));
        let repository = EventRepository::open(&root, crate::platform::host::current_host_name())
            .expect("open event repository");
        (root, repository)
    }

    #[test]
    fn lifecycle_query_is_explicit_and_rejects_ambiguous_inputs() {
        assert_eq!(
            session_lifecycle_query("/v2/sessions").expect("default query"),
            SessionLifecycleQuery::Active
        );
        assert_eq!(
            session_lifecycle_query("/v2/sessions?lifecycle=active").expect("active query"),
            SessionLifecycleQuery::Active
        );
        assert_eq!(
            session_lifecycle_query("/v2/sessions?lifecycle=retired").expect("archive query"),
            SessionLifecycleQuery::Retired
        );
        assert!(
            session_lifecycle_query("/v2/sessions?lifecycle=retired&lifecycle=active").is_err()
        );
        assert!(session_lifecycle_query("/v2/sessions?state=retired").is_err());
        assert!(session_lifecycle_query("/v2/sessions?lifecycle=all").is_err());
    }

    #[test]
    fn collection_query_accepts_root_include_hidden_switch() {
        assert_eq!(
            session_collection_query("/v2/sessions?includeHidden=true&lifecycle=active")
                .expect("hidden query"),
            SessionCollectionQuery {
                lifecycle: SessionLifecycleQuery::Active,
                include_hidden: true,
            }
        );
        assert_eq!(
            session_collection_query("/v2/sessions").expect("default query"),
            SessionCollectionQuery {
                lifecycle: SessionLifecycleQuery::Active,
                include_hidden: false,
            }
        );
        assert!(
            !session_collection_query("/v2/sessions?includeHidden=false")
                .expect("explicit false")
                .include_hidden
        );
        assert!(session_collection_query("/v2/sessions?includeHidden=yes").is_err());
        assert!(
            session_collection_query("/v2/sessions?includeHidden=true&includeHidden=true")
                .is_err()
        );
        assert!(session_collection_query("/v2/sessions?state=retired").is_err());
        // Per-ID routes never accept the collection switch.
        assert!(session_lifecycle_query("/v2/sessions/x?includeHidden=true").is_err());
    }

    #[test]
    fn retire_receipt_and_event_use_lifecycle_and_preserve_optional_reason() {
        let (root, repository) = repository("retire");
        let request = ValidatedCutexRequest {
            request_id: "retire-request-1".to_string(),
            method: "cutex/session/retire".to_string(),
            params: json!({
                "expectedRevision": 4,
                "expectedRuntimeGeneration": 7,
                "reason": "owner_cleanup"
            }),
        };

        let result = dispatch_session_retire(
            &repository,
            retire_transition,
            "cutex-session-1",
            &session("active", 4),
            &request,
        )
        .expect("dispatch retire");

        assert_eq!(result["lifecycle"], "retired");
        assert!(result.get("state").is_none());
        let page = repository
            .page(ReplayQuery::default())
            .expect("read events");
        assert_eq!(page.events.len(), 1);
        let event = page.events[0].cutex.as_ref().expect("retired event");
        assert_eq!(event.method, "cutex/session/retired");
        assert_eq!(event.params["reason"], "owner_cleanup");
        assert_eq!(event.params["sessionRevision"], 5);
        assert_eq!(
            page.events[0].correlation.management_request_id.as_deref(),
            Some("retire-request-1")
        );
        std::fs::remove_dir_all(root).expect("remove event repository");
    }

    #[test]
    fn restore_returns_offline_active_receipt_and_only_emits_restore_event() {
        let (root, repository) = repository("restore");
        let request = ValidatedCutexRequest {
            request_id: "restore-request-1".to_string(),
            method: "cutex/session/restore".to_string(),
            params: json!({ "expectedRevision": 5 }),
        };

        let result = dispatch_session_restore(
            &repository,
            restore_transition,
            "cutex-session-1",
            &session("retired", 5),
            &request,
        )
        .expect("dispatch restore");

        assert_eq!(result["lifecycle"], "active");
        assert_eq!(result["status"], "offline");
        assert!(result["retiredAt"].is_null());
        let page = repository
            .page(ReplayQuery::default())
            .expect("read events");
        assert_eq!(page.events.len(), 1);
        assert_eq!(
            page.events[0]
                .cutex
                .as_ref()
                .expect("restored event")
                .method,
            "cutex/session/restored"
        );
        std::fs::remove_dir_all(root).expect("remove event repository");
    }

    #[test]
    fn provider_receipt_for_another_session_is_rejected_before_event_append() {
        let (root, repository) = repository("mismatched-receipt");
        let request = ValidatedCutexRequest {
            request_id: "retire-request-mismatch".to_string(),
            method: "cutex/session/retire".to_string(),
            params: json!({
                "expectedRevision": 4,
                "expectedRuntimeGeneration": 7
            }),
        };

        let error = dispatch_session_retire(
            &repository,
            mismatched_transition,
            "cutex-session-1",
            &session("active", 4),
            &request,
        )
        .expect_err("mismatched receipt must fail");

        assert_eq!(error.code, "event_persistence_unavailable");
        assert!(error.outcome_unknown);
        assert!(repository
            .page(ReplayQuery::default())
            .expect("read events")
            .events
            .is_empty());
        std::fs::remove_dir_all(root).expect("remove event repository");
    }

    #[test]
    fn retire_reason_is_optional_but_unknown_or_typed_wrong_fields_are_rejected() {
        let without_reason = json!({
            "expectedRevision": 1,
            "expectedRuntimeGeneration": 0
        });
        assert!(retire_params(&without_reason).is_ok());
        assert!(retire_params(&json!({
            "expectedRevision": 1,
            "expectedRuntimeGeneration": 0,
            "reason": false
        }))
        .is_err());
        assert!(retire_params(&json!({
            "expectedRevision": 1,
            "expectedRuntimeGeneration": 0,
            "extra": true
        }))
        .is_err());
    }
}
