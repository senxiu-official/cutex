//! Atomic shared admission reordering and immediate owner delivery.

use serde_json::{json, Value};

use super::model::EventCorrelation;
use super::repository::EventRepository;
use super::server::{
    allowed_params, append_cutex_event, exact_params, invalid_user_input_error,
    persistence_user_input_error, post_operation_persistence_error, queue_not_found_error,
    queue_repository_error, required_safe_integer_param, required_string_param,
    ValidatedCutexRequest,
};
use super::user_input::{
    session_input_lock, QueueInputAlreadySubmitted, QueueMoveError, QueueRevisionConflict,
    UserInputDisposition, UserInputExecutionError, UserInputRepository, UserInputStrategy,
    UserInputSubmitCommand, UserInputSubmitExecution, UserInputSubmitParams,
};

pub const MOVE_METHOD: &str = "cutex/userInput/queue/move";
pub const STEER_METHOD: &str = "cutex/userInput/queue/steer";
/// The owner waives the next owner-first opportunity once ("let the
/// delegation results go first this time"); see `admission::owner_yield`.
pub const YIELD_METHOD: &str = "cutex/userInput/queue/yield";
pub const REORDERED_EVENT: &str = "cutex/userInput/queueReordered";

pub fn is_method(method: &str) -> bool {
    matches!(method, MOVE_METHOD | STEER_METHOD | YIELD_METHOD)
}

/// The caller already holds session_input_lock; the backend must never route
/// a refused immediate send back through ordinary submit's enqueue fallback.
pub type QueueSteerHandler = fn(
    &UserInputSubmitCommand,
    bool,
    &Value,
) -> Result<UserInputSubmitExecution, UserInputExecutionError>;

pub(super) fn dispatch(
    events: &EventRepository,
    queue: &UserInputRepository,
    steer: impl FnOnce(
        &UserInputSubmitCommand,
        bool,
        &Value,
    ) -> Result<UserInputSubmitExecution, UserInputExecutionError>,
    session_id: &str,
    session: &Value,
    request: &ValidatedCutexRequest,
) -> Result<Value, UserInputExecutionError> {
    let lock = session_input_lock(session_id).map_err(persistence_user_input_error)?;
    let _guard = lock.lock().map_err(|_| {
        persistence_user_input_error(anyhow::anyhow!("session input lock poisoned"))
    })?;
    match request.method.as_str() {
        MOVE_METHOD => {
            let params = exact_params(
                &request.params,
                &["queueId", "expectedRevision", "before"],
            )?;
            let queue_id = required_string_param(params, "queueId")?;
            let expected = required_safe_integer_param(params, "expectedRevision")?;
            let before = if params["before"].is_null() { None } else {
                let reference: super::admission::AdmissionRef = serde_json::from_value(params["before"].clone())
                    .map_err(|_| invalid_user_input_error("before must be a kind/id reference or null"))?;
                if !matches!(reference.kind.as_str(), "owner" | "ask") || reference.id.is_empty() {
                    return Err(invalid_user_input_error("before must be a kind/id reference or null"));
                }
                Some(reference)
            };
            let may_pass = crate::config::store::load_codez_config().queue.owner_may_pass_asks;
            let (admission, _queue_revision, items, _changed) = queue
                .move_admission_item(session_id, queue_id, expected, before.as_ref(), may_pass)
                .map_err(move_error)?;
            Ok(json!({"revision": admission.revision, "orderRevision": admission.order_revision, "items": items, "admission": admission}))
        }
        YIELD_METHOD => {
            exact_params(&request.params, &[])?;
            // Idempotent: a repeated yield only re-arms the same one-shot flag.
            // `changed` publishes `cutex/userInput/queueReordered` through the
            // admission driver like every other admission change.
            super::admission::owner_yield(session_id);
            let (_, _, _, admission) = queue.list_admission(session_id).map_err(persistence_user_input_error)?;
            Ok(json!({"admission": admission}))
        }
        STEER_METHOD => {
            let params = allowed_params(
                &request.params,
                &["queueId", "expectedRevision"],
                &["expectedTurnId", "ifIdle", "joinDelegation"],
            )?;
            let queue_id = required_string_param(params, "queueId")?;
            let expected = required_safe_integer_param(params, "expectedRevision")?;
            let start_if_idle = match params.get("ifIdle").and_then(Value::as_str) {
                None if !params.contains_key("ifIdle") => true,
                Some("start") => true,
                Some("fail") => false,
                _ => return Err(invalid_user_input_error("ifIdle must be start or fail")),
            };
            let expected_turn_id = match params.get("expectedTurnId") {
                None => None,
                Some(_) => Some(required_string_param(params, "expectedTurnId")?.to_string()),
            };
            let (_, items) = queue
                .list(session_id)
                .map_err(persistence_user_input_error)?;
            let item = items
                .into_iter()
                .find(|item| item.queue_id == queue_id)
                .ok_or_else(|| queue_not_found_error(queue_id))?;
            if item.revision != expected {
                return Err(queue_repository_error(
                    QueueRevisionConflict {
                        expected,
                        current: item.revision,
                    }
                    .into(),
                ));
            }
            if queue
                .delivery(session_id, &item.client_user_message_id)
                .map_err(persistence_user_input_error)?
                .is_some()
            {
                return Err(queue_repository_error(QueueInputAlreadySubmitted.into()));
            }
            let thread_id = session
                .pointer("/native/threadId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or_else(session_offline)?;
            let command = UserInputSubmitCommand {
                management_request_id: request.request_id.clone(),
                cutex_session_id: session_id.to_string(),
                thread_id: thread_id.to_string(),
                params: UserInputSubmitParams {
                                        join_delegation: params.get("joinDelegation")
                        .map(super::user_input::parse_owner_join).transpose()
                        .map_err(|message| invalid_user_input_error(&message))?,
                    client_user_message_id: item.client_user_message_id.clone(),
                    origin: item.origin.clone(),
                    strategy: UserInputStrategy::Auto,
                    input: item.input.clone(),
                    expected_turn_id,
                },
            };
            // No queue mutation precedes native acceptance. Sending/unknown is
            // durably recorded by the backend; failed calls leave this item intact.
            let execution = steer(&command, start_if_idle, session)?;
            if !execution.app_server_accepted
                || !matches!((execution.disposition, execution.native_method.as_deref()),
                    (UserInputDisposition::Steered, Some("turn/steer")) |
                    (UserInputDisposition::Started, Some("turn/start"))) {
                return Err(UserInputExecutionError {
                    stage: "native_request".into(), code: "outcome_unknown".into(),
                    message: "runtime did not return a valid acceptance receipt; the queued input was kept".into(),
                    retryable: false, details: json!({}), outcome_unknown: true,
                });
            }
            let image_delivery = super::user_input::image_steering::accepted_delivery(
                &item.input,
                session.pointer("/runtime/backend").and_then(Value::as_str) == Some("claude_subscription"),
                &execution,
            );
            let correlation = EventCorrelation {
                thread_id: Some(command.thread_id.clone()),
                turn_id: execution.turn_id.clone(),
                client_user_message_id: Some(item.client_user_message_id.clone()),
                management_request_id: Some(request.request_id.clone()),
                queue_id: Some(item.queue_id.clone()),
                native_request_id: execution.native_request_id.clone(),
                ..Default::default()
            };
            append_cutex_event(
                events,
                session_id,
                correlation.clone(),
                "cutex/userInput/submitted",
                with_image_delivery(json!({
                    "managementRequestId": request.request_id,
                    "clientUserMessageId": item.client_user_message_id,
                    "origin": item.origin,
                    "input": item.input,
                    "disposition": execution.disposition,
                    "nativeRequestId": execution.native_request_id,
                    "nativeMethod": execution.native_method,
                    "turnId": execution.turn_id,
                    "appServerAccepted": true,
                    "submittedAt": chrono::Utc::now().to_rfc3339(),
                }), image_delivery),
            )
            .map_err(post_operation_persistence_error)?;
            let removed = queue
                .remove(session_id, queue_id, expected)
                .map_err(post_operation_persistence_error)?
                .ok_or_else(|| {
                    post_operation_persistence_error(anyhow::anyhow!(
                        "accepted queue item disappeared"
                    ))
                })?;
            let (revision, _) = queue
                .list(session_id)
                .map_err(post_operation_persistence_error)?;
            append_cutex_event(events, session_id, correlation, "cutex/userInput/queueRemoved", with_image_delivery(json!({
                "queueId": queue_id,
                "clientUserMessageId": item.client_user_message_id,
                "revision": removed.revision,
                "queueRevision": revision,
                "reason": if execution.disposition == UserInputDisposition::Steered { "steered" } else { "submitted" },
                "removedAt": chrono::Utc::now().to_rfc3339(),
            }), image_delivery)).map_err(post_operation_persistence_error)?;
            Ok(with_image_delivery(json!({
                "disposition": execution.disposition,
                "turnId": execution.turn_id,
                "clientUserMessageId": item.client_user_message_id,
                "queueRevision": revision,
            }), image_delivery))
        }
        _ => unreachable!("not a queue order method"),
    }
}

fn with_image_delivery(mut value: Value, delivery: Option<&str>) -> Value {
    if let Some(delivery) = delivery { value["imageDelivery"] = json!(delivery); }
    value
}

fn move_error(error: anyhow::Error) -> UserInputExecutionError {
    match error.downcast_ref::<QueueMoveError>() {
        Some(QueueMoveError::PassingNotAllowed) => UserInputExecutionError {
            stage: "queue".into(), code: "passing_not_allowed".into(),
            message: "moving ahead of a non-owner item is disabled".into(),
            retryable: false, details: json!({}), outcome_unknown: false,
        },
        Some(QueueMoveError::NotFound(id)) => queue_not_found_error(id),
        Some(QueueMoveError::AdmissionNotFound(id)) => UserInputExecutionError {
            stage: "queue".into(), code: "admission_item_not_found".into(),
            message: "the referenced admission item no longer exists".into(),
            retryable: false, details: json!({ "id": id }), outcome_unknown: false,
        },
        Some(QueueMoveError::DeliveryBlocked(id)) => {
            let mut result = queue_repository_error(QueueInputAlreadySubmitted.into());
            result.details["blockingQueueId"] = json!(id);
            result
        }
        None => queue_repository_error(error),
    }
}

pub fn session_offline() -> UserInputExecutionError {
    UserInputExecutionError {
        stage: "queue".into(),
        code: "session_offline".into(),
        message: "the runtime is offline; the queued input was kept".into(),
        retryable: true,
        details: json!({}),
        outcome_unknown: false,
    }
}

pub fn steer_unavailable(reason: &str) -> UserInputExecutionError {
    UserInputExecutionError {
        stage: "queue".into(),
        code: "steer_unavailable".into(),
        message: "the queued input cannot be sent immediately; it was kept".into(),
        retryable: true,
        details: json!({"reason": reason}),
        outcome_unknown: false,
    }
}

#[cfg(test)]
mod tests {
    use super::super::contract_validation::{
        validate_cutex_event_message, validate_cutex_request, validate_cutex_response,
    };
    use super::super::request_idempotency::{BeginRequest, RequestIdempotencyRepository};
    use super::super::user_input::{
        parse_user_input_submit_params, InputDelivery, QueueEnqueueDecision,
    };
    use super::*;

    struct Fixture {
        root: std::path::PathBuf,
        queue: UserInputRepository,
        events: EventRepository,
    }
    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("cutex-queue-order-{}", uuid::Uuid::new_v4()));
            Self {
                queue: UserInputRepository::open(root.join("queue")).unwrap(),
                events: EventRepository::open(root.join("events"), crate::platform::host::current_host_name()).unwrap(),
                root,
            }
        }
        fn enqueue(&self, client: &str) -> super::super::user_input::UserInputQueueItem {
            let params = parse_user_input_submit_params(&json!({"clientUserMessageId":client,
                "origin":{"kind":"other","clientId":"desktop"},"strategy":"queue",
                "input":[{"type":"text","text":client,"text_elements":[]}]}))
            .unwrap();
            match self
                .queue
                .enqueue("session", "thread", "enqueue", &params)
                .unwrap()
            {
                QueueEnqueueDecision::Queued(item) | QueueEnqueueDecision::Deduplicated(item) => {
                    item
                }
                _ => panic!("unexpected identity conflict"),
            }
        }
        fn list(&self) -> (u64, Vec<super::super::user_input::UserInputQueueItem>) {
            self.queue.list("session").unwrap()
        }
        fn order_revision(&self) -> u64 { self.queue.admission("session").unwrap().order_revision }
        fn move_item(&self, id: &str, expected: u64, before: Option<&str>) -> anyhow::Result<(u64, Vec<super::super::user_input::UserInputQueueItem>, bool)> {
            let before = before.map(|id| super::super::admission::AdmissionRef { kind: "owner".into(), id: id.into() });
            let (admission, _, items, changed) = self.queue.move_admission_item("session", id, expected, before.as_ref(), true)?;
            Ok((admission.order_revision, items, changed))
        }
        fn request(&self, method: &str, params: Value) -> ValidatedCutexRequest {
            validate_cutex_request(
                &json!({"requestId":"request-1","method":method,"params":params}),
            )
            .unwrap();
            ValidatedCutexRequest {
                request_id: "request-1".into(),
                method: method.into(),
                params,
            }
        }
        fn dispatch(
            &self,
            request: &ValidatedCutexRequest,
        ) -> Result<Value, UserInputExecutionError> {
            dispatch(
                &self.events,
                &self.queue,
                |command, _, _| {
                    assert!(session_input_lock("session").unwrap().try_lock().is_err());
                    assert_eq!(command.params.client_user_message_id, "b");
                    assert_eq!(command.params.origin.client_id, "desktop");
                    Ok(UserInputSubmitExecution {
                        join_refusal: None,
                        disposition: UserInputDisposition::Steered,
                        app_server_accepted: true,
                        native_request_id: Some(json!("native-1")),
                        native_method: Some("turn/steer".into()),
                        turn_id: Some("turn".into()),
                        queue: None,
                        queue_paused: false,
                    })
                },
                "session",
                &json!({"native":{"threadId":"thread"}}),
                request,
            )
        }
        fn events(&self) -> Vec<super::super::model::CutexMessage> {
            self.events
                .page(super::super::repository::ReplayQuery {
                    cutex_session_id: Some("session".into()),
                    ..Default::default()
                })
                .unwrap()
                .events
                .into_iter()
                .filter_map(|event| event.cutex)
                .collect()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn image_delivery_receipts_match_accepted_provider_mode_and_persisted_events() {
        for (backend, method, disposition, mode) in [
            ("claude_subscription", "turn/steer", UserInputDisposition::Steered, "pathText"),
            ("native", "turn/steer", UserInputDisposition::Steered, "nativeImage"),
            ("claude_subscription", "turn/start", UserInputDisposition::Started, "nativeImage"),
        ] {
            let f = Fixture::new();
            let params = parse_user_input_submit_params(&json!({"clientUserMessageId":"images",
                "origin":{"kind":"other","clientId":"desktop"},"strategy":"queue",
                "input":[{"type":"text","text":"original"},{"type":"localImage","path":"/target/upload.png"}]})).unwrap();
            f.queue.enqueue("session", "thread", "enqueue", &params).unwrap();
            let item = f.list().1.remove(0);
            let request = f.request(STEER_METHOD, json!({"queueId":item.queue_id,
                "expectedRevision":item.revision,"ifIdle":"start"}));
            let result = dispatch(&f.events, &f.queue, |command, _, _| {
                assert_eq!(f.list().1.len(), 1, "queue remains until runtime success");
                assert_eq!(command.params.input, params.input);
                Ok(UserInputSubmitExecution { join_refusal:None, disposition,
                    app_server_accepted:true, native_request_id:Some(json!("native-image")),
                    native_method:Some(method.into()), turn_id:Some("turn".into()), queue:None, queue_paused:false })
            }, "session", &json!({"native":{"threadId":"thread"},"runtime":{"backend":backend}}), &request).unwrap();
            assert_eq!(result["imageDelivery"], mode);
            assert!(f.list().1.is_empty());
            let reopened = EventRepository::open(f.root.join("events"), crate::platform::host::current_host_name()).unwrap();
            let page = reopened.page(super::super::repository::ReplayQuery {
                cutex_session_id:Some("session".into()), ..Default::default()
            }).unwrap();
            let messages: Vec<_> = page.events.into_iter().filter_map(|event| event.cutex).collect();
            for method in ["cutex/userInput/submitted", "cutex/userInput/queueRemoved"] {
                let message = messages.iter().find(|event| event.method == method).unwrap();
                assert_eq!(message.params["imageDelivery"], mode);
                if method.ends_with("submitted") { assert_eq!(message.params["input"], json!(params.input)); }
            }
        }
        let f = Fixture::new();
        let item = f.enqueue("b");
        let result = f.dispatch(&f.request(STEER_METHOD, json!({"queueId":item.queue_id,
            "expectedRevision":item.revision,"ifIdle":"fail"}))).unwrap();
        assert!(result.get("imageDelivery").is_none(), "text-only receipt stays unchanged");
        assert!(f.events().iter().all(|event| event.params.get("imageDelivery").is_none()));
    }

    #[test]
    fn unconfirmed_queue_steer_never_emits_a_success_receipt_or_removes_input() {
        for failure in ["refused", "unknown", "unaccepted", "queued", "wrong-method"] {
            let f = Fixture::new();
            let item = f.enqueue("b");
            let before = f.list();
            let request = f.request(STEER_METHOD, json!({"queueId":item.queue_id,
                "expectedRevision":item.revision,"ifIdle":"fail"}));
            let result = dispatch(&f.events, &f.queue, |_,_,_| {
                if matches!(failure, "refused" | "unknown") {
                    return Err(UserInputExecutionError { stage:"native_request".into(),code:failure.into(),
                        message:"fixture failure".into(),retryable:false,details:json!({}),outcome_unknown:failure=="unknown" });
                }
                Ok(UserInputSubmitExecution { join_refusal:None,
                    disposition:if failure=="queued" { UserInputDisposition::Queued } else { UserInputDisposition::Steered },
                    app_server_accepted:failure!="unaccepted", native_request_id:None,
                    native_method:Some(if failure=="wrong-method" { "turn/start" } else { "turn/steer" }.into()),
                    turn_id:Some("turn".into()),queue:None,queue_paused:false })
            }, "session", &json!({"native":{"threadId":"thread"},"runtime":{"backend":"claude_subscription"}}), &request);
            assert!(result.is_err(), "{failure}");
            assert_eq!(f.list(), before, "{failure}");
            assert!(f.events().is_empty(), "{failure}");
        }
    }

    #[test]
    fn explicit_owner_join_reaches_atomic_dispatch_and_removes_only_on_acceptance() {
        let h = Fixture::new();
        let item = h.enqueue("joined");
        let join = json!({"askId":"ask-a", "attempt":2, "expectedTurnId":"turn", "runtimeGeneration":7});
        let request = h.request(STEER_METHOD, json!({"queueId":item.queue_id,"expectedRevision":item.revision,"ifIdle":"fail","joinDelegation":join}));
        let result = dispatch(&h.events, &h.queue, |command, start, _| {
            assert!(!start);
            assert_eq!(serde_json::to_value(command.params.join_delegation.as_ref().unwrap()).unwrap(), join);
            Ok(UserInputSubmitExecution { join_refusal: None, disposition: UserInputDisposition::Steered,
                app_server_accepted:true, native_request_id:Some(json!("native-joined")), native_method:Some("turn/steer".into()),
                turn_id:Some("turn".into()),queue:None,queue_paused:false })
        }, "session", &json!({"native":{"threadId":"thread"}}), &request).unwrap();
        assert_eq!(result["disposition"], "steered");
        assert!(h.list().1.is_empty());
    }

    #[test]
    fn move_contract_rejects_old_parameters_and_lists_the_persisted_order() {
        assert!(validate_cutex_request(&json!({"requestId":"old","method":MOVE_METHOD,"params":{
            "queueId":"q", "expectedQueueRevision":0, "beforeQueueId":null
        }})).is_err());
        assert!(validate_cutex_request(&json!({"requestId":"snapshot-cas","method":MOVE_METHOD,"params":{
            "queueId":"q", "expectedAdmissionRevision":0, "before":null
        }})).is_err());
        let f = Fixture::new();
        let a = f.enqueue("a");
        let b = f.enqueue("b");
        f.dispatch(&f.request(MOVE_METHOD, json!({"queueId":b.queue_id,
            "expectedRevision":f.order_revision(), "before":{"kind":"owner","id":a.queue_id}}))).unwrap();
        let reopened = UserInputRepository::open(f.root.join("queue")).unwrap();
        let (revision, items, pause, admission) = reopened.list_admission("session").unwrap();
        assert_eq!(items[0].queue_id, b.queue_id);
        assert_eq!(admission.items[0].id, b.queue_id);
        validate_cutex_response(&json!({"contractVersion":2,"requestId":"list","cutexSessionId":"session",
            "cutex":{"method":"cutex/userInput/queue/list","result":{
                "revision":revision,"items":items,"pause":pause,"paused":false,"asksPaused":null,"admission":admission
            }}})).unwrap();
    }

    #[test]
    fn admission_contract_accepts_an_unlabelled_ridealong() {
        let f = Fixture::new();
        let mut admission = serde_json::to_value(f.queue.admission("session").unwrap()).unwrap();
        admission["nextStep"] = json!([{
            "id":"bus-message", "deliveryKind":"bus", "label":null,
            "enqueuedAt":"2026-10-12T00:00:00Z", "timing":"afterTurn"
        }, {
            "id":"delivery-1", "deliveryKind":"answer", "label":"Answer", "askId":"ask-1",
            "enqueuedAt":"2026-10-12T00:00:00Z", "timing":"nextStep"
        }]);
        validate_cutex_event_message(&json!({
            "method": REORDERED_EVENT, "params": {"admission":admission.clone()}
        })).unwrap();
        admission["nextStep"][1]["askId"] = json!("");
        assert!(validate_cutex_event_message(&json!({
            "method": REORDERED_EVENT, "params": {"admission":admission}
        })).is_err(), "askId is a non-empty string");
    }

    /// `cutex/userInput/queue/yield`: exact empty params, idempotent, and the
    /// snapshot shows `ownerYielded` until the running turn completes.
    #[test]
    fn yield_arms_the_one_shot_owner_yield_and_reports_it_in_the_snapshot() {
        use super::super::admission;
        assert!(is_method(YIELD_METHOD));
        assert!(validate_cutex_request(&json!({"requestId":"bad","method":YIELD_METHOD,"params":{"x":1}})).is_err());
        // The Agent Bus repository checks the process HOME: hold the shared HOME lock.
        let _home = crate::config::paths::test_home_env_lock();
        let root = std::env::temp_dir().join(format!("cutex-queue-yield-{}", uuid::Uuid::new_v4()));
        let queue = UserInputRepository::open_with_asks(root.join("queue"), root.join("queue").join("asks"), true).unwrap();
        let events = EventRepository::open(root.join("events"), crate::platform::host::current_host_name()).unwrap();
        let session = format!("queue-yield-{}", uuid::Uuid::new_v4());
        let call = |params: Value| {
            let request = ValidatedCutexRequest { request_id: "request-1".into(), method: YIELD_METHOD.into(), params };
            dispatch(&events, &queue, |_, _, _| unreachable!("yield never steers"), &session,
                &json!({"native":{"threadId":"thread"}}), &request)
        };
        admission::turn_started(&session);
        admission::note_turn(&session, "delivery", None);
        let before = queue.list_admission(&session).unwrap().3;
        assert!(!before.owner_yielded);
        assert!(serde_json::to_value(&before).unwrap().get("ownerYielded").is_none());

        assert_eq!(call(json!({"x":1})).unwrap_err().code, invalid_user_input_error("").code);
        let result = call(json!({})).unwrap();
        assert_eq!(result["admission"]["ownerYielded"], true);
        assert_eq!(result["admission"]["currentTurn"]["kind"], "delivery");
        assert!(result["admission"]["revision"].as_u64().unwrap() > before.revision);
        assert_eq!(result.as_object().unwrap().len(), 1);
        validate_cutex_request(&json!({"requestId":"request-1","method":YIELD_METHOD,"params":{}})).unwrap();
        validate_cutex_response(&json!({"contractVersion":2,"requestId":"request-1","cutexSessionId":session,
            "cutex":{"method":YIELD_METHOD,"result":result}})).unwrap();
        // Idempotent: the same flag, no new revision.
        let again = call(json!({})).unwrap();
        assert_eq!(again, result);
        let (revision, items, pause, listed) = queue.list_admission(&session).unwrap();
        assert!(listed.owner_yielded);
        validate_cutex_response(&json!({"contractVersion":2,"requestId":"list","cutexSessionId":session,
            "cutex":{"method":"cutex/userInput/queue/list","result":{
                "revision":revision,"items":items,"pause":pause,"paused":false,"asksPaused":null,"admission":listed
            }}})).unwrap();

        // The delivery turn completes: no owner priority, the flag is gone.
        assert!(!admission::turn_completed(&session));
        assert!(!admission::owner_priority(&session));
        let after = queue.list_admission(&session).unwrap().3;
        assert!(!after.owner_yielded);
        assert!(after.revision > listed.revision);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn admission_changed_event_accepts_a_revision_only_payload() {
        validate_cutex_event_message(&json!({
            "method": REORDERED_EVENT, "params": {"admission":{"revision":12}}
        })).unwrap();
    }

    #[test]
    fn move_uses_order_revision_and_distinguishes_missing_references() {
        let f = Fixture::new();
        let a = f.enqueue("a");
        let b = f.enqueue("b");
        let result = f.dispatch(&f.request(MOVE_METHOD, json!({
            "queueId": b.queue_id, "expectedRevision": f.order_revision(),
            "before": {"kind":"owner", "id": a.queue_id}
        }))).unwrap();
        assert_eq!(result["revision"], result["admission"]["revision"]);
        assert_eq!(result["orderRevision"], result["admission"]["orderRevision"]);
        let missing = f.dispatch(&f.request(MOVE_METHOD, json!({
            "queueId": b.queue_id, "expectedRevision": f.order_revision(),
            "before": {"kind":"ask", "id":"gone"}
        }))).unwrap_err();
        assert_eq!(missing.code, "admission_item_not_found");
        let missing_owner = f.dispatch(&f.request(MOVE_METHOD, json!({
            "queueId":"gone", "expectedRevision": f.order_revision(), "before":null
        }))).unwrap_err();
        assert_eq!(missing_owner.code, "queue_item_not_found");
        assert!(validate_cutex_request(&json!({"requestId":"delivery", "method":MOVE_METHOD,
            "params":{"queueId":a.queue_id, "expectedRevision":f.order_revision(),
                "before":{"kind":"delivery", "id":"bus"}}})).is_err());
    }

    #[test]
    fn move_preserves_content_revisions_timestamps_pause_and_current_positions() {
        let f = Fixture::new();
        let a = f.enqueue("a");
        let b = f.enqueue("b");
        let c = f.enqueue("c");
        f.queue.mark_bypass_pause("session", &c.queue_id).unwrap();
        f.queue.pause("session", "manual", None).unwrap();
        let pause = f.queue.pause_state("session").unwrap();
        let mut revision = f.order_revision();
        for (id, before, order, changed) in [
            (
                &c.queue_id,
                Some(a.queue_id.as_str()),
                vec!["c", "a", "b"],
                true,
            ),
            (
                &c.queue_id,
                Some(b.queue_id.as_str()),
                vec!["a", "c", "b"],
                true,
            ),
            (&c.queue_id, None, vec!["a", "b", "c"], true),
            (&c.queue_id, None, vec!["a", "b", "c"], false),
            (
                &a.queue_id,
                Some(a.queue_id.as_str()),
                vec!["a", "b", "c"],
                false,
            ),
            (
                &a.queue_id,
                Some(b.queue_id.as_str()),
                vec!["a", "b", "c"],
                false,
            ),
        ] {
            let (next, items, did_change) =
                f.move_item(id, revision, before).unwrap();
            assert_eq!(did_change, changed);
            assert_eq!(next, revision + u64::from(changed));
            revision = next;
            assert_eq!(
                items
                    .iter()
                    .map(|item| item.client_user_message_id.as_str())
                    .collect::<Vec<_>>(),
                order
            );
            for (position, item) in items.iter().enumerate() {
                let original = [&a, &b, &c]
                    .into_iter()
                    .find(|original| original.queue_id == item.queue_id)
                    .unwrap();
                assert_eq!(item.position, position as u64);
                assert_eq!(item.revision, original.revision);
                assert_eq!(item.created_at, original.created_at);
                assert_eq!(item.updated_at, original.updated_at);
                assert_eq!(item.input, original.input);
            }
        }
        assert_eq!(f.queue.pause_state("session").unwrap(), pause);
        f.move_item(&c.queue_id, revision, Some(&a.queue_id))
            .unwrap();
        assert_eq!(f.enqueue("c").position, 0);
        let updated = f
            .queue
            .update(
                "session",
                &c.queue_id,
                1,
                vec![json!({"type":"text","text":"edited"})],
            )
            .unwrap()
            .unwrap();
        assert_eq!(updated.position, 0);
        assert!(updated.bypass_pause);
        assert_eq!(
            f.queue
                .next_flushable("session", false)
                .unwrap()
                .unwrap()
                .2
                .queue_id,
            c.queue_id
        );
    }

    #[test]
    fn move_errors_are_typed_and_report_current_revision_and_blocker() {
        let f = Fixture::new();
        let a = f.enqueue("a");
        let b = f.enqueue("b");
        let c = f.enqueue("c");
        for (method, params) in [
            (
                MOVE_METHOD,
                json!({"queueId":a.queue_id,"expectedRevision":2,"before":null}),
            ),
            (
                STEER_METHOD,
                json!({"queueId":a.queue_id,"expectedRevision":0}),
            ),
        ] {
            let error = f.dispatch(&f.request(method, params)).unwrap_err();
            assert_eq!(error.code, "revision_conflict");
            assert_eq!(
                error.details["currentRevision"],
                if method == MOVE_METHOD { 3 } else { 1 }
            );
            assert_eq!(error.details["resyncRequired"], true);
            assert_eq!(
                super::super::server::cutex_operation_error_status(&error.code, false),
                409
            );
        }
        for error in [
            f.queue
                .update("session", &a.queue_id, 0, a.input.clone())
                .unwrap_err(),
            f.queue.remove("session", &a.queue_id, 0).unwrap_err(),
        ] {
            assert_eq!(queue_repository_error(error).details["currentRevision"], 1);
        }
        for (id, before) in [("missing", None), (a.queue_id.as_str(), Some("missing"))] {
            let error = f
                .dispatch(&f.request(
                    MOVE_METHOD,
                    json!({"queueId":id,"expectedRevision":3,"before":before.map(|id| json!({"kind":"owner","id":id}))}),
                ))
                .unwrap_err();
            assert_eq!(error.code, if before.is_some() { "admission_item_not_found" } else { "queue_item_not_found" });
        }
        for delivery in [
            InputDelivery::Sending {
                thread_id: "thread".into(),
                since: chrono::Utc::now().to_rfc3339(),
            },
            InputDelivery::Unknown {
                thread_id: "thread".into(),
                since: chrono::Utc::now().to_rfc3339(),
            },
        ] {
            f.queue
                .set_delivery("session", "b", None, Some(delivery.clone()))
                .unwrap();
            for (id, before) in [
                (&b.queue_id, None),
                (&c.queue_id, Some(a.queue_id.as_str())),
                (&c.queue_id, Some(b.queue_id.as_str())),
            ] {
                let error = f
                    .dispatch(&f.request(
                        MOVE_METHOD,
                        json!({"queueId":id,"expectedRevision":3,"before":before.map(|id| json!({"kind":"owner","id":id}))}),
                    ))
                    .unwrap_err();
                assert_eq!(error.code, "queue_input_already_submitted");
                assert_eq!(error.details["blockingQueueId"], b.queue_id);
            }
            let error = f
                .dispatch(&f.request(
                    STEER_METHOD,
                    json!({"queueId":b.queue_id,"expectedRevision":1}),
                ))
                .unwrap_err();
            assert_eq!(error.code, "queue_input_already_submitted");
            f.queue
                .set_delivery("session", "b", Some(&delivery), None)
                .unwrap();
        }
        assert_eq!(f.list().0, 3);
    }

    #[test]
    fn move_and_steer_emit_valid_events_and_replay_exact_request_results() {
        let f = Fixture::new();
        let a = f.enqueue("a");
        let b = f.enqueue("b");
        let idempotency =
            RequestIdempotencyRepository::open(f.root.join("requests"), "process".into()).unwrap();
        for (method, params) in [
            (
                MOVE_METHOD,
                json!({"queueId":b.queue_id,"expectedRevision":2,"before":{"kind":"owner","id":a.queue_id}}),
            ),
            (
                STEER_METHOD,
                json!({"queueId":b.queue_id,"expectedRevision":1,"ifIdle":"start"}),
            ),
        ] {
            let mut request = f.request(method, params);
            request.request_id = method.into();
            let body = json!({"method":method,"params":request.params});
            let BeginRequest::Forward(claim) = idempotency
                .begin_for_method(method, "session", method, &body)
                .unwrap()
            else {
                panic!("first request")
            };
            let result = f.dispatch(&request).unwrap();
            let response = json!({"contractVersion":2,"requestId":method,"cutexSessionId":"session","cutex":{"method":method,"result":result}});
            validate_cutex_response(&response).unwrap();
            idempotency
                .complete(&claim, 200, "OK", response.clone())
                .unwrap();
            let before = f.list();
            let event_count = f.events().len();
            let BeginRequest::Completed(replay) = idempotency
                .begin_for_method(method, "session", method, &body)
                .unwrap()
            else {
                panic!("replay")
            };
            assert_eq!(replay.body, response);
            assert_eq!(f.list(), before);
            assert_eq!(f.events().len(), event_count);
        }
        let events = f.events();
        assert_eq!(
            events
                .iter()
                .map(|event| event.method.as_str())
                .collect::<Vec<_>>(),
            [
                "cutex/userInput/submitted",
                "cutex/userInput/queueRemoved"
            ]
        );
        assert_eq!(events[1].params["reason"], "steered");
        assert_eq!(events[1].params["queueRevision"], 4);
        for event in events {
            validate_cutex_event_message(&json!({"method":event.method,"params":event.params}))
                .unwrap();
        }
        assert_eq!(f.list().1[0].queue_id, a.queue_id);
        for method in [MOVE_METHOD, STEER_METHOD] {
            assert!(super::super::session::cutex_method_is_registered(method));
        }
        drop(idempotency);
    }

    #[test]
    fn steer_failure_keeps_the_entire_queue_and_pause_unchanged() {
        let f = Fixture::new();
        f.enqueue("a");
        let b = f.enqueue("b");
        f.enqueue("c");
        f.queue.pause("session", "manual", None).unwrap();
        let before = f.list();
        let pause = f.queue.pause_state("session").unwrap();
        let request = f.request(
            STEER_METHOD,
            json!({"queueId":b.queue_id,"expectedRevision":1}),
        );
        for error in [
            session_offline(),
            steer_unavailable("runtimeNoSteer"),
            steer_unavailable("delegationTurn"),
            steer_unavailable("noActiveTurn"),
        ] {
            let returned = dispatch(
                &f.events,
                &f.queue,
                |_, _, _| Err(error.clone()),
                "session",
                &json!({"native":{"threadId":"thread"}}),
                &request,
            )
            .unwrap_err();
            assert_eq!(returned, error);
            assert_eq!(f.list(), before);
            assert_eq!(f.queue.pause_state("session").unwrap(), pause);
        }
        assert!(f.events().is_empty());
    }
}
