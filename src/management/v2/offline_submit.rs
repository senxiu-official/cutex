//! `cutex/userInput/submit` with `startIfOffline: true`.
//!
//! When the session's app-server is not connected (but a native thread is
//! bound), the input is put into the durable Cutex user-input queue instead of
//! failing, and the runtime is started in the background through the same
//! path as `cutex/runtime/online`. Connecting a runtime already flushes the
//! queue (`connect_runtime` -> `flush_queued_if_idle`), one item per idle
//! turn, so queued items are delivered in order with the usual queue
//! semantics; after the start returns, one more flush picks up items queued
//! while it was running. The queued item stays listable, editable and
//! removable through `cutex/userInput/queue/*` until it is sent, which is how
//! a user withdraws it. If the start fails, the items stay queued and
//! `cutex/userInput/startFailed` is emitted (after `cutex/runtime/failed`).
//!
//! With the runtime connected, or without the flag, the request behaves
//! exactly like a plain submit.

use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::OnceLock;

use serde_json::json;
use serde_json::Value;

use super::repository::management_v2_repository;
use super::repository::EventRepository;
use super::server::append_cutex_event;
use super::server::dispatch_runtime_mutation;
use super::server::dispatch_user_input_submit;
use super::server::invalid_user_input_error;
use super::server::ValidatedCutexRequest;
use super::user_input::user_input_repository;
use super::user_input::UserInputExecutionError;
use crate::management::control_plane::HumanManagementPrincipal;
use crate::management::server::ManagementRequestContext;
use crate::management::server::ManagementUserInputHandler;

pub const START_IF_OFFLINE: &str = "startIfOffline";
pub const START_REASON: &str = "offline-submit";

static STARTS_IN_FLIGHT: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

/// What the background start needs, captured from the submit request.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct StartPlan {
    pub cutex_session_id: String,
    pub session: Value,
    pub submit_request_id: String,
}

pub(super) fn dispatch(
    event_repository: &EventRepository,
    context: ManagementRequestContext,
    owner: Option<HumanManagementPrincipal>,
    cutex_session_id: &str,
    session: &Value,
    request: &ValidatedCutexRequest,
) -> Result<Value, UserInputExecutionError> {
    dispatch_marking_bypass(
        event_repository,
        context.handle_user_input,
        cutex_session_id,
        session,
        request,
        |plan| spawn_start(context, owner, plan),
        |queue_id| {
            if let Err(error) = user_input_repository()
                .and_then(|queue| queue.mark_bypass_pause(cutex_session_id, queue_id))
            {
                eprintln!(
                    "\x1b[33mwarning:\x1b[0m failed to let offline input {queue_id} through the queue pause: {error:#}"
                );
            }
        },
    )
}

#[cfg(test)]
pub(super) fn dispatch_with(
    event_repository: &EventRepository,
    handle_user_input: ManagementUserInputHandler,
    cutex_session_id: &str,
    session: &Value,
    request: &ValidatedCutexRequest,
    start: impl FnOnce(StartPlan),
) -> Result<Value, UserInputExecutionError> {
    dispatch_marking_bypass(
        event_repository,
        handle_user_input,
        cutex_session_id,
        session,
        request,
        start,
        |_| {},
    )
}

/// `mark_bypass(queueId)`: the input was not a queued follow-up (strategy
/// `auto` / `interrupt`, queued only because the runtime is offline), so it
/// must go out even when the session's queue is paused after a stop. Called
/// before the start is requested, so the connect-time flush already sees it.
pub(super) fn dispatch_marking_bypass(
    event_repository: &EventRepository,
    handle_user_input: ManagementUserInputHandler,
    cutex_session_id: &str,
    session: &Value,
    request: &ValidatedCutexRequest,
    start: impl FnOnce(StartPlan),
    mark_bypass: impl FnOnce(&str),
) -> Result<Value, UserInputExecutionError> {
    let mut request = request.clone();
    let start_if_offline = take_start_flag(&mut request.params)?;
    let connected = session
        .pointer("/runtime/appServerConnected")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let thread_bound = session
        .pointer("/native/threadId")
        .and_then(Value::as_str)
        .is_some_and(|thread_id| !thread_id.is_empty());
    // Without a bound thread there is nothing to resume; the plain path
    // returns the established `session_offline` error.
    if !start_if_offline || connected || !thread_bound {
        return dispatch_user_input_submit(
            event_repository,
            handle_user_input,
            cutex_session_id,
            session,
            &request,
        );
    }
    // Claude agents too: `cutex/runtime/online` starts the adapter runtime and
    // the post-start flush submits through the Claude runtime manager.
    let params = request
        .params
        .as_object_mut()
        .expect("validated submit params object");
    let follow_up = params.get("strategy").and_then(Value::as_str) == Some("queue");
    // Offline there is no active turn to steer, interrupt or match.
    params.insert("strategy".to_string(), json!("queue"));
    params.remove("expectedTurnId");
    let mut result = dispatch_user_input_submit(
        event_repository,
        handle_user_input,
        cutex_session_id,
        session,
        &request,
    )?;
    if result.get("queue").is_some_and(Value::is_object) {
        if !follow_up {
            if let Some(queue_id) = result.pointer("/queue/queueId").and_then(Value::as_str) {
                mark_bypass(queue_id);
            }
            // It goes out past a pause, so it is not held by one.
            if result.get("paused").is_some() {
                result["paused"] = Value::Bool(false);
            }
        }
        // A start already requested elsewhere flushes the queue itself.
        if session.pointer("/runtime/status").and_then(Value::as_str) != Some("starting") {
            start(StartPlan {
                cutex_session_id: cutex_session_id.to_string(),
                session: session.clone(),
                submit_request_id: request.request_id.clone(),
            });
        }
        result
            .as_object_mut()
            .expect("submit result object")
            .insert("runtimeStarting".to_string(), Value::Bool(true));
    }
    Ok(result)
}

fn take_start_flag(params: &mut Value) -> Result<bool, UserInputExecutionError> {
    let object = params
        .as_object_mut()
        .ok_or_else(|| invalid_user_input_error("params must be an object"))?;
    match object.remove(START_IF_OFFLINE) {
        None => Ok(false),
        Some(Value::Bool(flag)) => Ok(flag),
        Some(_) => Err(invalid_user_input_error("startIfOffline must be a boolean")),
    }
}

fn spawn_start(
    context: ManagementRequestContext,
    owner: Option<HumanManagementPrincipal>,
    plan: StartPlan,
) {
    {
        let mut starts = STARTS_IN_FLIGHT
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !starts.insert(plan.cutex_session_id.clone()) {
            return;
        }
    }
    let spawned = std::thread::Builder::new()
        .name("cutex-offline-submit-start".to_string())
        .spawn(move || {
            let outcome = management_v2_repository().and_then(|repository| {
                run_start(
                    repository,
                    &plan,
                    |id, params| (context.online_session)(owner.as_ref(), id, params),
                    |id| (context.flush_user_input_queue)(id, 100, false),
                    queued_queue_ids,
                    crate::asks::wake::note_owner_start,
                )
            });
            if let Err(error) = outcome {
                eprintln!(
                    "\x1b[33mwarning:\x1b[0m offline submit start for {} failed: {error:#}",
                    plan.cutex_session_id
                );
            }
            STARTS_IN_FLIGHT
                .get_or_init(|| Mutex::new(HashSet::new()))
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(&plan.cutex_session_id);
        });
    if let Err(error) = spawned {
        eprintln!("\x1b[33mwarning:\x1b[0m failed to spawn offline submit start: {error}");
    }
}

/// Start the runtime for queued offline input. Returns `Ok(true)` when the
/// runtime came online, `Ok(false)` when the start failed (reported through
/// events; the queue is left untouched).
pub(super) fn run_start(
    repository: &EventRepository,
    plan: &StartPlan,
    online: impl Fn(&str, Value) -> Result<Value, UserInputExecutionError>,
    flush: impl Fn(&str) -> anyhow::Result<usize>,
    queued_ids: impl Fn(&str) -> Vec<String>,
    clear_owner_stop: impl FnOnce(&str),
) -> anyhow::Result<bool> {
    let id = plan.cutex_session_id.as_str();
    let start_request = ValidatedCutexRequest {
        request_id: format!("cutex-offline-start-{}", uuid::Uuid::new_v4()),
        method: "cutex/runtime/online".to_string(),
        params: json!({
            "expectedRuntimeGeneration": plan
                .session
                .pointer("/runtime/runtimeGeneration")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            "reason": START_REASON,
            // The submitting client is a GUI; no "attach from a terminal" hint.
            "openVisibleTerminal": false,
        }),
    };
    match dispatch_runtime_mutation(
        repository,
        |session_id, _method, params| online(session_id, params),
        id,
        &plan.session,
        &start_request,
    ) {
        Ok(_) => {
            // The owner's submit started it: no longer owner-stopped
            // (`asks::wake::note_owner_start`; after the start, like every
            // owner start, so no asks relaunch races it).
            clear_owner_stop(id);
            // Items queued while the start was running may have missed the
            // connect-time flush. A flush failure is reported by the flush
            // itself (`cutex/userInput/failed`); the items stay queued.
            if let Err(error) = flush(id) {
                eprintln!(
                    "\x1b[33mwarning:\x1b[0m post-start queue flush for {id} failed: {error:#}"
                );
            }
            Ok(true)
        }
        Err(error) => {
            let queue_ids = queued_ids(id);
            let details = if error.details.is_object() {
                error.details.clone()
            } else {
                json!({})
            };
            append_cutex_event(
                repository,
                id,
                super::model::EventCorrelation {
                    thread_id: plan
                        .session
                        .pointer("/native/threadId")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    management_request_id: Some(plan.submit_request_id.clone()),
                    ..Default::default()
                },
                "cutex/userInput/startFailed",
                json!({
                    "managementRequestId": plan.submit_request_id,
                    "startRequestId": start_request.request_id,
                    "queueIds": queue_ids,
                    "error": {
                        "source": "cutex",
                        "code": error.code,
                        "message": error.message,
                        "retryable": error.retryable,
                        "details": details,
                    },
                    "failedAt": chrono::Utc::now().to_rfc3339(),
                }),
            )?;
            Ok(false)
        }
    }
}

fn queued_queue_ids(cutex_session_id: &str) -> Vec<String> {
    user_input_repository()
        .and_then(|queue| queue.list(cutex_session_id))
        .map(|(_, items)| items.into_iter().map(|item| item.queue_id).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::management::v2::repository::ReplayQuery;
    use crate::management::v2::user_input::UserInputDisposition;
    use crate::management::v2::user_input::UserInputQueueItem;
    use crate::management::v2::user_input::UserInputStrategy;
    use crate::management::v2::user_input::UserInputSubmitCommand;
    use crate::management::v2::user_input::UserInputSubmitExecution;
    use std::cell::RefCell;

    static SEEN: Mutex<Vec<(String, UserInputStrategy, Option<String>)>> = Mutex::new(Vec::new());

    fn seen(session: &str) -> Vec<(UserInputStrategy, Option<String>)> {
        SEEN.lock()
            .unwrap()
            .iter()
            .filter(|(id, _, _)| id == session)
            .map(|(_, strategy, turn)| (*strategy, turn.clone()))
            .collect()
    }

    fn fake_submit(
        command: UserInputSubmitCommand,
    ) -> Result<UserInputSubmitExecution, UserInputExecutionError> {
        SEEN.lock().unwrap().push((
            command.cutex_session_id.clone(),
            command.params.strategy,
            command.params.expected_turn_id.clone(),
        ));
        if command.params.strategy == UserInputStrategy::Queue {
            return Ok(UserInputSubmitExecution {
                disposition: UserInputDisposition::Queued,
                app_server_accepted: false,
                native_request_id: None,
                native_method: None,
                turn_id: None,
                queue: Some(UserInputQueueItem {
                    queue_id: "queue-1".into(),
                    client_user_message_id: command.params.client_user_message_id.clone(),
                    origin: command.params.origin.clone(),
                    input: command.params.input.clone(),
                    position: 0,
                    revision: 1,
                    state: "queued".into(),
                    created_at: "2026-10-06T00:00:00Z".into(),
                    updated_at: "2026-10-06T00:00:00Z".into(),
                    bypass_pause: false,
                    delivery: None,
                }),
                queue_paused: false,
            });
        }
        Ok(UserInputSubmitExecution {
            disposition: UserInputDisposition::Started,
            app_server_accepted: true,
            native_request_id: Some(json!("native-1")),
            native_method: Some("turn/start".into()),
            turn_id: Some("turn-1".into()),
            queue: None,
            queue_paused: false,
        })
    }

    fn repository() -> (std::path::PathBuf, EventRepository) {
        let root = std::env::temp_dir()
            .join(format!("cutex-v2-offline-submit-{}", uuid::Uuid::new_v4()));
        let repository = EventRepository::open(&root, crate::platform::host::current_host_name())
            .expect("open event repository");
        (root, repository)
    }

    fn request(start: Option<Value>) -> ValidatedCutexRequest {
        let mut params = json!({
            "clientUserMessageId": "client-1",
            "origin": { "kind": "other", "clientId": "desktop" },
            "strategy": "auto",
            "input": [{ "type": "text", "text": "hello", "text_elements": [] }],
            "expectedTurnId": "turn-old",
        });
        if let Some(start) = start {
            params[START_IF_OFFLINE] = start;
        }
        ValidatedCutexRequest {
            request_id: "management-1".into(),
            method: "cutex/userInput/submit".into(),
            params,
        }
    }

    fn session(connected: bool, status: &str) -> Value {
        json!({
            "native": { "threadId": "thread-1" },
            "runtime": {
                "backend": "host",
                "status": status,
                "appServerConnected": connected,
                "runtimeGeneration": 3,
                "runtimeAgentId": null,
            },
        })
    }

    fn methods(repository: &EventRepository, session: &str) -> Vec<String> {
        repository
            .page(ReplayQuery {
                cutex_session_id: Some(session.into()),
                ..Default::default()
            })
            .expect("read events")
            .events
            .into_iter()
            .filter_map(|event| event.cutex.map(|cutex| cutex.method))
            .collect()
    }

    #[test]
    fn offline_submit_marks_non_queue_input_to_bypass_a_queue_pause() {
        let (root, repository) = repository();
        let marked = RefCell::new(Vec::<String>::new());
        let starts = RefCell::new(0);
        dispatch_marking_bypass(
            &repository,
            fake_submit,
            "bypass-a",
            &session(false, "offline"),
            &request(Some(json!(true))),
            |_| {
                // Marked before the start is requested.
                assert_eq!(marked.borrow().len(), 1);
                *starts.borrow_mut() += 1;
            },
            |queue_id| marked.borrow_mut().push(queue_id.to_string()),
        )
        .expect("queued");
        assert_eq!(marked.borrow().as_slice(), ["queue-1"]);
        assert_eq!(*starts.borrow(), 1);

        // An explicit queue submit is a follow-up: it stays behind a pause.
        let mut queued = request(Some(json!(true)));
        queued.params["strategy"] = json!("queue");
        queued.params.as_object_mut().unwrap().remove("expectedTurnId");
        dispatch_marking_bypass(
            &repository,
            fake_submit,
            "bypass-b",
            &session(false, "offline"),
            &queued,
            |_| {},
            |_| panic!("queue submits must not bypass a pause"),
        )
        .expect("queued");
        std::fs::remove_dir_all(root).expect("remove event repository");
    }

    #[test]
    fn offline_submit_queues_and_requests_a_start() {
        let (root, repository) = repository();
        let plans = RefCell::new(Vec::new());
        let result = dispatch_with(
            &repository,
            fake_submit,
            "offline-a",
            &session(false, "offline"),
            &request(Some(json!(true))),
            |plan| plans.borrow_mut().push(plan),
        )
        .expect("queued");
        assert_eq!(result["disposition"], "queued");
        assert_eq!(result["runtimeStarting"], true);
        assert_eq!(result["queue"]["queueId"], "queue-1");
        assert_eq!(seen("offline-a"), [(UserInputStrategy::Queue, None)]);
        let plans = plans.into_inner();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].submit_request_id, "management-1");
        assert_eq!(
            methods(&repository, "offline-a"),
            ["cutex/userInput/routeAccepted", "cutex/userInput/queued"]
        );
        super::super::contract_validation::validate_cutex_response(&json!({
            "contractVersion": 2,
            "requestId": "management-1",
            "cutexSessionId": "offline-a",
            "cutex": { "method": "cutex/userInput/submit", "result": result },
        }))
        .expect("result matches the published schema");
        super::super::contract_validation::validate_cutex_request(&json!({
            "requestId": "management-1",
            "method": "cutex/userInput/submit",
            "params": request(Some(json!(true))).params,
        }))
        .expect("startIfOffline is part of the published request schema");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn flag_is_inert_when_online_or_absent() {
        let (root, repository) = repository();
        let never = |_: StartPlan| panic!("no start");
        let online = dispatch_with(
            &repository,
            fake_submit,
            "online-b",
            &session(true, "online"),
            &request(Some(json!(true))),
            never,
        )
        .expect("started");
        assert_eq!(online["disposition"], "started");
        assert!(online.get("runtimeStarting").is_none());
        assert_eq!(
            seen("online-b"),
            [(UserInputStrategy::Auto, Some("turn-old".into()))]
        );
        dispatch_with(
            &repository,
            fake_submit,
            "plain-b",
            &session(false, "offline"),
            &request(Some(json!(false))),
            never,
        )
        .expect("plain path reaches the handler");
        assert_eq!(
            seen("plain-b"),
            [(UserInputStrategy::Auto, Some("turn-old".into()))]
        );
        let invalid = dispatch_with(
            &repository,
            fake_submit,
            "bad-b",
            &session(false, "offline"),
            &request(Some(json!("yes"))),
            never,
        )
        .expect_err("non-boolean flag");
        assert_eq!(invalid.code, "invalid_request");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unbound_claude_and_already_starting_sessions() {
        let (root, repository) = repository();
        let never = |_: StartPlan| panic!("no start");
        let unbound = json!({
            "native": { "threadId": null },
            "runtime": { "backend": "host", "appServerConnected": false },
        });
        let error = dispatch_with(
            &repository,
            fake_submit,
            "unbound-c",
            &unbound,
            &request(Some(json!(true))),
            never,
        )
        .expect_err("no thread");
        assert_eq!(error.code, "session_offline");
        let mut claude = session(false, "offline");
        claude["runtime"]["backend"] = json!("claude_subscription");
        let started = RefCell::new(Vec::new());
        let queued = dispatch_with(
            &repository,
            fake_submit,
            "claude-c",
            &claude,
            &request(Some(json!(true))),
            |plan| started.borrow_mut().push(plan.cutex_session_id),
        )
        .expect("an offline Claude agent queues and starts like Codex");
        assert_eq!(queued["runtimeStarting"], true);
        assert_eq!(started.into_inner(), ["claude-c"]);
        let starting = dispatch_with(
            &repository,
            fake_submit,
            "starting-c",
            &session(false, "starting"),
            &request(Some(json!(true))),
            never,
        )
        .expect("queued behind the running start");
        assert_eq!(starting["runtimeStarting"], true);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn plan(id: &str) -> StartPlan {
        StartPlan {
            cutex_session_id: id.into(),
            session: session(false, "offline"),
            submit_request_id: "management-1".into(),
        }
    }

    #[test]
    fn start_success_emits_online_and_flushes_again() {
        let (root, repository) = repository();
        let flushed = RefCell::new(0);
        let order = RefCell::new(Vec::new());
        let online = |id: &str, params: Value| {
            order.borrow_mut().push(format!("online {id}"));
            assert_eq!(id, "start-d");
            assert_eq!(params["expectedRuntimeGeneration"], 3);
            assert_eq!(params["reason"], START_REASON);
            Ok(json!({ "runtimeGeneration": 4, "status": "online", "runtimeAgentId": "agent-1" }))
        };
        let started = run_start(
            &repository,
            &plan("start-d"),
            online,
            |_| {
                *flushed.borrow_mut() += 1;
                Ok(1)
            },
            |_| Vec::new(),
            |id| order.borrow_mut().push(format!("clear {id}")),
        )
        .expect("start");
        assert!(started);
        // The owner's start clears the asks owner-stop mark once it ran.
        assert_eq!(*order.borrow(), ["online start-d", "clear start-d"]);
        assert_eq!(*flushed.borrow(), 1);
        assert_eq!(
            methods(&repository, "start-d"),
            ["cutex/runtime/onlineRequested", "cutex/runtime/online"]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn start_failure_keeps_queue_and_reports_it() {
        let (root, repository) = repository();
        let online = |_: &str, _: Value| {
            Err(UserInputExecutionError {
                stage: "route".into(),
                code: "runtime_cutover_required".into(),
                message: "no".into(),
                retryable: true,
                details: json!({ "reason": "test" }),
                outcome_unknown: false,
            })
        };
        let started = run_start(
            &repository,
            &plan("fail-e"),
            online,
            |_| panic!("no flush"),
            |_| vec!["queue-1".into(), "queue-2".into()],
            |_| panic!("a failed start keeps the owner-stop mark"),
        )
        .expect("failure is reported, not raised");
        assert!(!started);
        assert_eq!(
            methods(&repository, "fail-e"),
            [
                "cutex/runtime/onlineRequested",
                "cutex/runtime/failed",
                "cutex/userInput/startFailed"
            ]
        );
        let page = repository
            .page(ReplayQuery {
                cutex_session_id: Some("fail-e".into()),
                ..Default::default()
            })
            .unwrap();
        let failed = page.events[2].cutex.as_ref().unwrap();
        assert_eq!(failed.params["queueIds"], json!(["queue-1", "queue-2"]));
        assert_eq!(failed.params["error"]["code"], "runtime_cutover_required");
        assert_eq!(
            page.events[2].correlation.management_request_id.as_deref(),
            Some("management-1")
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
