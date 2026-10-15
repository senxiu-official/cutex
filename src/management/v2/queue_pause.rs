//! Queue pause ("stop = pause everything", capability `queue-pause`).
//!
//! Cutex auto-flushes the user-input queue whenever the native thread goes
//! idle (`turn/completed`, idle `thread/status/changed`, runtime connect).
//! Without a pause, stopping a turn therefore immediately started the next
//! queued message. A plain stop now pauses the queue instead:
//!
//! - `cutex/turn/stop {}`: records the pause (reason `interrupted`) *first*,
//!   then interrupts the active native turn and waits for it to stop, holding
//!   the session input lock (`ManagementRequestContext::interrupt_active_turn`).
//!   Because the pause is durable before the interrupt is sent, the
//!   interrupted turn's completion events can never flush the queue, whatever
//!   order they arrive in. A queue with no items is not paused (nothing to
//!   hold back).
//! - Interrupt-and-send never pauses: `cutex/userInput/submit` with strategy
//!   `interrupt` interrupts and starts the new turn under the same session
//!   lock, so the completion flush finds a running turn and the queue simply
//!   continues after that turn. Clients tell the two apart by the method they
//!   call; Cutex does not guess from a turn's `interrupted` status (a raw
//!   native `turn/interrupt`, e.g. from the TUI, keeps the old auto-flush).
//! - `cutex/userInput/queue/pause {}`: manual pause (reason `manual`).
//! - `cutex/userInput/queue/resume {}`: clears the pause, then flushes if the
//!   thread is idle (one item per idle turn, as usual).
//! - `cutex/userInput/queue/flush` still sends the front item while paused
//!   and leaves the pause in place. New submits (`auto` / `interrupt`) start
//!   turns as usual; `queue` submits join the paused queue. Offline submits
//!   (`startIfOffline`, strategy not `queue`) are marked `bypassPause`, so the
//!   runtime start delivers them past a paused queue.
//! - Removing the last item drops the pause (not the stop hold below).
//! - Stop holds everything (owner decision 2026-10-12, "停止 = everything
//!   waits", replaces the 2026-10-08 rule that a stop never pauses asks):
//!   before the interrupt, `cutex/turn/stop` also records a durable stop
//!   hold in the ask store (`crate::asks::control::owner_stop`), even with
//!   an empty queue. Until it is released no ask is admitted and no
//!   ride-along (ask results, bus wakes / tells, alarms) starts a turn.
//!   `cutex/userInput/queue/resume` releases it (before its flush), as do
//!   `cutex/asks/resume` and the owner's next message that starts or steers
//!   a turn (submit, queue steer, explicit queue flush). The admission
//!   snapshot shows it as `ridealongsHeld: "ownerStop"` and `held:
//!   "ownerStop"` on each `nextStep` item.
//!
//! State: `cutex/userInput/queue/list` returns `paused` and `pause
//! {pausedAt, reason, turnId}`; bootstrap carries `userInputQueuePause`.
//! Events: `cutex/userInput/queuePaused`, `cutex/userInput/queueResumed`.

use serde_json::json;
use serde_json::Value;

use crate::management::server::ManagementTurnInterruptHandler;
use crate::management::server::ManagementUserInputQueueFlusher;

use super::repository::EventRepository;
use super::server::append_cutex_event;
use super::server::invalid_user_input_error;
use super::server::persistence_user_input_error;
use super::server::post_operation_persistence_error;
use super::server::ValidatedCutexRequest;
use super::user_input::QueuePauseDecision;
use super::user_input::UserInputExecutionError;
use super::user_input::UserInputQueuePause;
use super::user_input::UserInputRepository;

pub const STOP_METHOD: &str = "cutex/turn/stop";
pub const PAUSE_METHOD: &str = "cutex/userInput/queue/pause";
pub const RESUME_METHOD: &str = "cutex/userInput/queue/resume";
pub const PAUSED_EVENT: &str = "cutex/userInput/queuePaused";
pub const RESUMED_EVENT: &str = "cutex/userInput/queueResumed";
pub const REASON_INTERRUPTED: &str = "interrupted";
pub const REASON_MANUAL: &str = "manual";

pub fn is_method(method: &str) -> bool {
    matches!(method, STOP_METHOD | PAUSE_METHOD | RESUME_METHOD)
}

/// `{paused, pause}` fields shared by the queue list and these methods.
pub fn pause_fields(pause: Option<&UserInputQueuePause>) -> (Value, Value) {
    (
        Value::Bool(pause.is_some()),
        pause
            .map(|pause| serde_json::to_value(pause).expect("queue pause serialization"))
            .unwrap_or(Value::Null),
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn dispatch(
    event_repository: &EventRepository,
    queue: &UserInputRepository,
    interrupt: ManagementTurnInterruptHandler,
    flush: ManagementUserInputQueueFlusher,
    cutex_session_id: &str,
    session: &Value,
    request: &ValidatedCutexRequest,
) -> Result<Value, UserInputExecutionError> {
    if !request
        .params
        .as_object()
        .is_some_and(|params| params.is_empty())
    {
        return Err(invalid_user_input_error("params must be an empty object"));
    }
    match request.method.as_str() {
        STOP_METHOD => {
            // Owner decision 2026-10-12: a stop holds everything. The hold
            // (asks, ask results, bus ride-alongs) is durable before the
            // interrupt, like the queue pause, so the interrupted turn's
            // completion can't start a delivery turn. It also ends the
            // session's delegation_wait calls and leaves the native
            // external-input gate closed until the hold is released.
            crate::asks::waits::cancel_waits_default(cutex_session_id);
            crate::asks::control::owner_stop_default(cutex_session_id);
            dispatch_stop(
                event_repository,
                queue,
                |id, thread| interrupt(id, thread),
                cutex_session_id,
                session,
                request,
            )
        }
        PAUSE_METHOD => {
            let (decision, _) = record_pause(
                event_repository,
                queue,
                cutex_session_id,
                request,
                REASON_MANUAL,
                None,
            )?;
            Ok(decision)
        }
        RESUME_METHOD => {
            // Release the stop hold first: ride-alongs then go by the usual
            // admission rules (idle ride-alongs before numbered owner turns).
            crate::asks::control::release_stop_hold_default(cutex_session_id);
            dispatch_resume(
            event_repository,
            queue,
            |id| flush(id, 100, false),
            cutex_session_id,
            request,
        )
        }
        _ => unreachable!("queue_pause::dispatch called for {}", request.method),
    }
}

/// Pause (if anything is queued) and report `{revision, paused, pause,
/// alreadyPaused}`; the bool says whether this call created the pause.
fn record_pause(
    event_repository: &EventRepository,
    queue: &UserInputRepository,
    cutex_session_id: &str,
    request: &ValidatedCutexRequest,
    reason: &str,
    turn_id: Option<String>,
) -> Result<(Value, bool), UserInputExecutionError> {
    let decision = queue
        .pause(cutex_session_id, reason, turn_id)
        .map_err(persistence_user_input_error)?;
    let (revision, pause, already, created) = match decision {
        QueuePauseDecision::Paused { pause, revision } => (revision, Some(pause), false, true),
        QueuePauseDecision::AlreadyPaused { pause, revision } => {
            (revision, Some(pause), true, false)
        }
        QueuePauseDecision::Empty { revision } => (revision, None, false, false),
    };
    if created {
        let pause = pause.as_ref().expect("created pause");
        append_cutex_event(
            event_repository,
            cutex_session_id,
            super::model::EventCorrelation {
                management_request_id: Some(request.request_id.clone()),
                turn_id: pause.turn_id.clone(),
                ..Default::default()
            },
            PAUSED_EVENT,
            json!({
                "pausedAt": pause.paused_at,
                "reason": pause.reason,
                "turnId": pause.turn_id,
                "revision": revision,
            }),
        )
        .map_err(post_operation_persistence_error)?;
    }
    let (paused, pause) = pause_fields(pause.as_ref());
    Ok((
        json!({
            "revision": revision,
            "paused": paused,
            "pause": pause,
            "alreadyPaused": already,
        }),
        created,
    ))
}

pub(super) fn dispatch_stop(
    event_repository: &EventRepository,
    queue: &UserInputRepository,
    interrupt: impl FnOnce(&str, &str) -> Result<Option<String>, UserInputExecutionError>,
    cutex_session_id: &str,
    session: &Value,
    request: &ValidatedCutexRequest,
) -> Result<Value, UserInputExecutionError> {
    let thread_id = session
        .pointer("/native/threadId")
        .and_then(Value::as_str)
        .filter(|thread| !thread.is_empty())
        .map(str::to_string);
    let connected = session
        .pointer("/runtime/appServerConnected")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let active_turn_id = session
        .pointer("/runtime/activeTurnId")
        .and_then(Value::as_str)
        .map(str::to_string);
    // 1. Pause first: from here on no idle/completion event can flush.
    let (mut result, _) = record_pause(
        event_repository,
        queue,
        cutex_session_id,
        request,
        REASON_INTERRUPTED,
        active_turn_id,
    )?;
    // 2. Interrupt (an offline runtime has no running turn).
    let interrupted = match (connected, thread_id) {
        (true, Some(thread_id)) => interrupt(cutex_session_id, &thread_id)?,
        _ => None,
    };
    result
        .as_object_mut()
        .expect("pause result object")
        .insert(
            "interruptedTurnId".to_string(),
            interrupted.map(Value::String).unwrap_or(Value::Null),
        );
    Ok(result)
}

pub(super) fn dispatch_resume(
    event_repository: &EventRepository,
    queue: &UserInputRepository,
    flush: impl FnOnce(&str) -> anyhow::Result<usize>,
    cutex_session_id: &str,
    request: &ValidatedCutexRequest,
) -> Result<Value, UserInputExecutionError> {
    let (_, before_items) = queue
        .list(cutex_session_id)
        .map_err(persistence_user_input_error)?;
    let resumed = queue
        .resume(cutex_session_id)
        .map_err(persistence_user_input_error)?;
    if let Some((_, revision)) = &resumed {
        append_cutex_event(
            event_repository,
            cutex_session_id,
            super::model::EventCorrelation {
                management_request_id: Some(request.request_id.clone()),
                ..Default::default()
            },
            RESUMED_EVENT,
            json!({
                "resumedAt": chrono::Utc::now().to_rfc3339(),
                "reason": "resumed",
                "revision": revision,
            }),
        )
        .map_err(post_operation_persistence_error)?;
    }
    // Idle now: send the next item. Busy or offline: the next completion /
    // connect flushes. A failure is reported by the flush itself
    // (`cutex/userInput/failed`) and the items stay queued, unpaused.
    if let Err(error) = flush(cutex_session_id) {
        eprintln!(
            "\x1b[33mwarning:\x1b[0m queue flush after resume for {cutex_session_id} failed: {error:#}"
        );
    }
    let (revision, after_items) = queue
        .list(cutex_session_id)
        .map_err(persistence_user_input_error)?;
    let remaining = after_items
        .iter()
        .map(|item| item.queue_id.as_str())
        .collect::<std::collections::HashSet<_>>();
    let affected = before_items
        .into_iter()
        .filter_map(|item| (!remaining.contains(item.queue_id.as_str())).then_some(item.queue_id))
        .collect::<Vec<_>>();
    Ok(json!({
        "revision": revision,
        "paused": false,
        "pause": Value::Null,
        "resumed": resumed.is_some(),
        "affectedQueueIds": affected,
    }))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::management::v2::user_input::parse_user_input_submit_params;
    use crate::management::v2::user_input::QueueEnqueueDecision;

    fn fixture() -> (std::path::PathBuf, EventRepository, UserInputRepository) {
        let root = std::env::temp_dir().join(format!("cutex-v2-queue-pause-{}", uuid::Uuid::new_v4()));
        let events = EventRepository::open(root.join("events"), crate::platform::host::current_host_name())
            .expect("event repository");
        let queue = UserInputRepository::open(root.join("queue")).expect("queue repository");
        (root, events, queue)
    }

    fn enqueue(queue: &UserInputRepository, id: &str) -> String {
        let params = parse_user_input_submit_params(&json!({
            "clientUserMessageId": id,
            "origin": { "kind": "other", "clientId": "desktop" },
            "strategy": "queue",
            "input": [{ "type": "text", "text": id, "text_elements": [] }]
        }))
        .expect("params");
        match queue.enqueue("session-a", "thread-a", "request-q", &params).expect("enqueue") {
            QueueEnqueueDecision::Queued(item) => item.queue_id,
            other => panic!("unexpected {other:?}"),
        }
    }

    fn request(method: &str) -> ValidatedCutexRequest {
        ValidatedCutexRequest {
            request_id: format!("request-{}", uuid::Uuid::new_v4()),
            method: method.to_string(),
            params: json!({}),
        }
    }

    fn online_session() -> Value {
        json!({
            "native": { "threadId": "thread-a" },
            "runtime": { "appServerConnected": true, "activeTurnId": "turn-1" }
        })
    }

    fn methods(events: &EventRepository) -> Vec<(String, Value)> {
        events
            .page(crate::management::v2::repository::ReplayQuery {
                cutex_session_id: Some("session-a".into()),
                ..Default::default()
            })
            .expect("events")
            .events
            .into_iter()
            .filter_map(|event| event.cutex.map(|cutex| (cutex.method, cutex.params)))
            .collect()
    }

    #[test]
    fn stop_pauses_before_interrupting_and_reports_both() {
        let (root, events, queue) = fixture();
        enqueue(&queue, "m-1");
        enqueue(&queue, "m-2");
        let seen_paused = RefCell::new(None);
        let result = dispatch_stop(
            &events,
            &queue,
            |id, thread| {
                assert_eq!((id, thread), ("session-a", "thread-a"));
                // The pause is durable before the interrupt is sent.
                *seen_paused.borrow_mut() = Some(queue.pause_state(id).expect("state").is_some());
                Ok(Some("turn-1".to_string()))
            },
            "session-a",
            &online_session(),
            &request(STOP_METHOD),
        )
        .expect("stop");
        assert_eq!(*seen_paused.borrow(), Some(true));
        assert_eq!(result["paused"], json!(true));
        assert_eq!(result["alreadyPaused"], json!(false));
        assert_eq!(result["interruptedTurnId"], json!("turn-1"));
        assert_eq!(result["pause"]["reason"], json!("interrupted"));
        assert_eq!(result["pause"]["turnId"], json!("turn-1"));
        let emitted = methods(&events);
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].0, PAUSED_EVENT);
        assert_eq!(emitted[0].1["reason"], json!("interrupted"));
        super::super::contract_validation::validate_cutex_event_message(&json!({
            "method": emitted[0].0, "params": emitted[0].1,
        }))
        .expect("paused event matches the schema");
        // Auto flush is now held back.
        assert!(queue.next_flushable("session-a", false).expect("next").is_none());

        // A second stop keeps the original pause (no second event).
        let again = dispatch_stop(
            &events,
            &queue,
            |_, _| Ok(None),
            "session-a",
            &online_session(),
            &request(STOP_METHOD),
        )
        .expect("stop again");
        assert_eq!(again["alreadyPaused"], json!(true));
        assert_eq!(again["interruptedTurnId"], Value::Null);
        assert_eq!(methods(&events).len(), 1);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn stop_with_empty_queue_interrupts_without_pausing() {
        let (root, events, queue) = fixture();
        let result = dispatch_stop(
            &events,
            &queue,
            |_, _| Ok(Some("turn-1".to_string())),
            "session-a",
            &online_session(),
            &request(STOP_METHOD),
        )
        .expect("stop");
        assert_eq!(result["paused"], json!(false));
        assert_eq!(result["pause"], Value::Null);
        assert_eq!(result["interruptedTurnId"], json!("turn-1"));
        assert!(methods(&events).is_empty());
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn stop_offline_pauses_without_calling_the_runtime() {
        let (root, events, queue) = fixture();
        enqueue(&queue, "m-1");
        let result = dispatch_stop(
            &events,
            &queue,
            |_, _| panic!("no runtime to interrupt"),
            "session-a",
            &json!({ "native": { "threadId": "thread-a" }, "runtime": { "appServerConnected": false } }),
            &request(STOP_METHOD),
        )
        .expect("stop");
        assert_eq!(result["paused"], json!(true));
        assert_eq!(result["interruptedTurnId"], Value::Null);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn stop_keeps_the_pause_when_the_interrupt_fails() {
        let (root, events, queue) = fixture();
        enqueue(&queue, "m-1");
        let error = dispatch_stop(
            &events,
            &queue,
            |_, _| {
                Err(UserInputExecutionError {
                    stage: "interrupt".into(),
                    code: "interrupt_timeout".into(),
                    message: "timed out".into(),
                    retryable: true,
                    details: json!({}),
                    outcome_unknown: true,
                })
            },
            "session-a",
            &online_session(),
            &request(STOP_METHOD),
        )
        .expect_err("interrupt failed");
        assert_eq!(error.code, "interrupt_timeout");
        assert!(queue.pause_state("session-a").expect("state").is_some());
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn resume_clears_the_pause_then_flushes() {
        let (root, events, queue) = fixture();
        let first = enqueue(&queue, "m-1");
        enqueue(&queue, "m-2");
        queue.pause("session-a", REASON_INTERRUPTED, None).expect("pause");
        let result = dispatch_resume(
            &events,
            &queue,
            |id| {
                // The flush runs unpaused and sends the front item.
                assert!(queue.pause_state(id).expect("state").is_none());
                let (_, _, item) = queue.next_flushable(id, false).expect("next").expect("item");
                queue.remove(id, &item.queue_id, item.revision).expect("remove");
                Ok(1)
            },
            "session-a",
            &request(RESUME_METHOD),
        )
        .expect("resume");
        assert_eq!(result["paused"], json!(false));
        assert_eq!(result["resumed"], json!(true));
        assert_eq!(result["affectedQueueIds"], json!([first]));
        let emitted = methods(&events);
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].0, RESUMED_EVENT);
        super::super::contract_validation::validate_cutex_event_message(&json!({
            "method": emitted[0].0, "params": emitted[0].1,
        }))
        .expect("resumed event matches the schema");

        // Not paused: resume is a harmless flush, no event.
        let again = dispatch_resume(&events, &queue, |_| Ok(0), "session-a", &request(RESUME_METHOD))
            .expect("resume again");
        assert_eq!(again["resumed"], json!(false));
        assert_eq!(methods(&events).len(), 1);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn manual_pause_and_response_shapes_match_the_schema() {
        let (root, events, queue) = fixture();
        enqueue(&queue, "m-1");
        let (paused, created) = record_pause(
            &events,
            &queue,
            "session-a",
            &request(PAUSE_METHOD),
            REASON_MANUAL,
            None,
        )
        .expect("pause");
        assert!(created);
        assert_eq!(paused["pause"]["reason"], json!("manual"));
        let mut stopped = paused.clone();
        stopped["interruptedTurnId"] = Value::Null;
        for (method, result) in [
            (PAUSE_METHOD, paused.clone()),
            (STOP_METHOD, stopped),
            (
                RESUME_METHOD,
                json!({ "revision": 3, "paused": false, "pause": null, "resumed": true, "affectedQueueIds": [] }),
            ),
            (
                "cutex/userInput/queue/list",
                json!({ "revision": 2, "items": queue.list("session-a").unwrap().1, "admission": queue.admission("session-a").unwrap(), "paused": true, "pause": paused["pause"].clone() }),
            ),
        ] {
            super::super::contract_validation::validate_cutex_response(&json!({
                "contractVersion": 2,
                "requestId": "request-1",
                "cutexSessionId": "session-a",
                "cutex": { "method": method, "result": result },
            }))
            .unwrap_or_else(|error| panic!("{method}: {error}"));
            super::super::contract_validation::validate_cutex_request(&json!({
                "requestId": "request-1",
                "method": method,
                "params": {},
            }))
            .unwrap_or_else(|error| panic!("{method} request: {error}"));
        }
        assert!(crate::management::v2::session::cutex_method_is_registered(STOP_METHOD));
        assert!(crate::management::v2::session::cutex_method_is_registered(PAUSE_METHOD));
        assert!(crate::management::v2::session::cutex_method_is_registered(RESUME_METHOD));
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}
