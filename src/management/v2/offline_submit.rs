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

/// How long one background start keeps retrying a busy host (r271.1): the
/// 050 `admission_busy` / `hostBusy` answer means nothing started, so the
/// start is tried again after `retryAfterMs` instead of failing the queue.
#[cfg(not(test))]
const HOST_BUSY_START_BUDGET: std::time::Duration = std::time::Duration::from_secs(300);
#[cfg(test)]
const HOST_BUSY_START_BUDGET: std::time::Duration = std::time::Duration::from_millis(300);

/// The last background start of a session (this process), for callers that
/// must not call the queued input done before its runtime is up (owner
/// rotation's first message to the new Director).
#[derive(Clone, Debug, PartialEq)]
pub enum StartOutcome {
    Starting,
    Online,
    Failed { code: String, message: String, retryable: bool },
}

static START_OUTCOMES: OnceLock<Mutex<std::collections::HashMap<String, StartOutcome>>> = OnceLock::new();

fn note_start_outcome(cutex_session_id: &str, outcome: StartOutcome) {
    START_OUTCOMES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(cutex_session_id.to_string(), outcome);
}

/// See [`StartOutcome`]; `None` when no background start ran in this process.
pub fn start_outcome(cutex_session_id: &str) -> Option<StartOutcome> {
    START_OUTCOMES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(cutex_session_id)
        .cloned()
}

/// `online` again while the host answers `admission_busy` (nothing started),
/// pausing `retryAfterMs` plus jitter, holding no lane or fence, within
/// [`HOST_BUSY_START_BUDGET`]. Any other answer is returned as is.
fn retry_host_busy(
    mut online: impl FnMut() -> Result<Value, UserInputExecutionError>,
) -> Result<Value, UserInputExecutionError> {
    let deadline = std::time::Instant::now() + HOST_BUSY_START_BUDGET;
    loop {
        match online() {
            Err(error)
                if error.code == crate::asks::pause_admission::ADMISSION_BUSY_CODE
                    && error.retryable
                    && std::time::Instant::now() < deadline =>
            {
                let after = error.details.get("retryAfterMs").and_then(Value::as_u64).unwrap_or(1_000);
                let jitter = (uuid::Uuid::new_v4().as_u128() % 500) as u64;
                let pause = std::time::Duration::from_millis(after + jitter);
                #[cfg(test)]
                let pause = pause.min(std::time::Duration::from_millis(20));
                std::thread::sleep(pause);
            }
            other => return other,
        }
    }
}

/// What the background start needs, captured from the submit request.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct StartPlan {
    pub cutex_session_id: String,
    pub session: Value,
    pub submit_request_id: String,
}

/// `cutex/userInput/submit` from the HTTP route. With the input ledger on, an
/// owner message ALWAYS starts an offline agent (owner decision 8, r274
/// Director decision 2), with or without `startIfOffline`; other callers and
/// the switch off keep the flag's behaviour.
pub(super) fn dispatch_owner_submit(
    event_repository: &EventRepository,
    context: ManagementRequestContext,
    owner: Option<HumanManagementPrincipal>,
    cutex_session_id: &str,
    session: &Value,
    request: &ValidatedCutexRequest,
) -> Result<Value, UserInputExecutionError> {
    if let Some(id)=request.params.get("clientUserMessageId").and_then(Value::as_str) {
        super::input_disposal::check(cutex_session_id,[id])
            .map_err(|error|super::input_ledger::route_error("input_disposed",error.to_string(),false))?;
    }
    let request = owner_starts_offline(owner.is_some() && super::input_ledger::enabled(), request);
    dispatch(event_repository, context, owner, cutex_session_id, session, &request)
}

/// `request` with `startIfOffline: true` when `always` (an owner message,
/// switch on); unchanged otherwise. A malformed flag is left for the usual
/// validation error.
fn owner_starts_offline(always: bool, request: &ValidatedCutexRequest) -> ValidatedCutexRequest {
    let mut request = request.clone();
    if always {
        if let Some(params) = request.params.as_object_mut() {
            if params.get(START_IF_OFFLINE).is_none_or(Value::is_boolean) {
                params.insert(START_IF_OFFLINE.to_string(), Value::Bool(true));
            }
        }
    }
    request
}

/// `cutex/userInput/queue/steer` refused because the agent is offline
/// (`session_offline` / `app_server_unavailable`): with the input ledger on
/// and the owner asking, the agent is started (Director decision 2): the item
/// stays queued, goes past a queue pause like an offline submit, waits
/// `waiting{agentOffline}` (nextUp) and is flushed when the runtime connects.
/// The refusal itself is unchanged on the wire except `details.runtimeStarting:
/// true` (the steer result schema has no queued disposition).
pub(super) fn start_for_offline_steer(
    context: ManagementRequestContext,
    owner: Option<HumanManagementPrincipal>,
    cutex_session_id: &str,
    session: &Value,
    request: &ValidatedCutexRequest,
    error: UserInputExecutionError,
) -> UserInputExecutionError {
    let Some(queue_id) = offline_steer_item(owner.is_some() && super::input_ledger::enabled(), session, request, &error) else {
        return error;
    };
    let Some(item) = user_input_repository().ok()
        .and_then(|queue| queue.list(cutex_session_id).ok())
        .and_then(|(_, items)| items.into_iter().find(|item| item.queue_id == queue_id))
    else {
        return error;
    };
    if let Err(mark) = user_input_repository().and_then(|queue| queue.mark_bypass_pause(cutex_session_id, &queue_id)) {
        eprintln!("\x1b[33mwarning:\x1b[0m failed to let offline steer {queue_id} through the queue pause: {mark:#}");
    }
    super::input_ledger_owner::offline_waiting(cutex_session_id, &item.client_user_message_id);
    if session.pointer("/runtime/status").and_then(Value::as_str) != Some("starting") {
        spawn_start(context, owner, StartPlan {
            cutex_session_id: cutex_session_id.to_string(),
            session: session.clone(),
            submit_request_id: request.request_id.clone(),
        });
    }
    let mut error = error;
    if !error.details.is_object() {
        error.details = json!({});
    }
    error.details["runtimeStarting"] = Value::Bool(true);
    error
}

/// `cutex/input/interruptSend` found the runtime gone (owner decision 8):
/// its input waits first in the queue (`waiting{agentOffline}`); start the
/// agent, unless a start is running already.
pub(super) fn start_for_interrupt_send(
    context: ManagementRequestContext,
    owner: Option<HumanManagementPrincipal>,
    cutex_session_id: &str,
    session: &Value,
    request_id: &str,
) {
    if session.pointer("/runtime/status").and_then(Value::as_str) == Some("starting") {
        return;
    }
    spawn_start(context, owner, StartPlan {
        cutex_session_id: cutex_session_id.to_string(),
        session: session.clone(),
        submit_request_id: request_id.to_string(),
    });
}

/// The queue item an offline `queue/steer` refusal should start the agent
/// for (pure): the owner asked with the switch on, the refusal says the
/// runtime is offline, a native thread is bound (a start can resume it) and
/// the runtime is not connected.
fn offline_steer_item(owner_on: bool, session: &Value, request: &ValidatedCutexRequest, error: &UserInputExecutionError) -> Option<String> {
    if !owner_on || error.outcome_unknown || !matches!(error.code.as_str(), "session_offline" | "app_server_unavailable") {
        return None;
    }
    let connected = session.pointer("/runtime/appServerConnected").and_then(Value::as_bool).unwrap_or(false);
    let thread_bound = session.pointer("/native/threadId").and_then(Value::as_str).is_some_and(|thread| !thread.is_empty());
    if connected || !thread_bound {
        return None;
    }
    request.params.get("queueId").and_then(Value::as_str).filter(|id| !id.is_empty()).map(str::to_string)
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
    // An offline start queues the input; `allowHeld` only shapes an online
    // submit's busy-host answer.
    if let Some(object) = request.params.as_object_mut() { object.remove("allowHeld"); }
    // Claude agents too: `cutex/runtime/online` starts the adapter runtime and
    // the post-start flush submits through the Claude runtime manager.
    let params = request
        .params
        .as_object_mut()
        .expect("validated submit params object");
    let follow_up = params.get("strategy").and_then(Value::as_str) == Some("queue");
    let original_strategy = match params.get("strategy").and_then(Value::as_str) {
        Some("queue") => super::user_input::UserInputStrategy::Queue,
        Some("interrupt") => super::user_input::UserInputStrategy::Interrupt,
        _ => super::user_input::UserInputStrategy::Auto,
    };
    let agent_paused = session.get("agentPause").is_some_and(Value::is_object);
    // Offline there is no active turn to steer, interrupt or match.
    params.insert("strategy".to_string(), json!("queue"));
    params.remove("expectedTurnId");
    // Owner decision 8: the input ledger records it on nextUp,
    // waiting{agentOffline} (no-op while the switch is off).
    let mut result = super::input_ledger_owner::with_offline_start(original_strategy, agent_paused, || {
        dispatch_user_input_submit(
            event_repository,
            handle_user_input,
            cutex_session_id,
            session,
            &request,
        )
    })?;
    if result.get("queue").is_some_and(Value::is_object) {
        if agent_paused {
            return Ok(result);
        }
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
    note_start_outcome(&plan.cutex_session_id, StartOutcome::Starting);
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
                note_start_outcome(&plan.cutex_session_id, StartOutcome::Failed {
                    code: "persistence_unavailable".into(), message: format!("{error:#}"), retryable: true,
                });
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
        // One start: a busy host is retried inside it, so its events are one
        // `onlineRequested` and one outcome (r271.1).
        |session_id, _method, params| retry_host_busy(|| online(session_id, params.clone())),
        id,
        &plan.session,
        &start_request,
    ) {
        Ok(_) => {
            // The owner's submit started it: no longer owner-stopped
            // (`asks::wake::note_owner_start`; after the start, like every
            // owner start, so no asks relaunch races it).
            clear_owner_stop(id);
            note_start_outcome(id, StartOutcome::Online);
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
            note_start_outcome(id, StartOutcome::Failed {
                code: error.code.clone(), message: error.message.clone(), retryable: error.retryable,
            });
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
                join_refusal: None, queued_reason: None,
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
            join_refusal: None, queued_reason: None,
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

    fn host_busy() -> UserInputExecutionError {
        crate::management::v2::server::admission_busy_user_input_error(
            "route",
            anyhow::anyhow!("hostBusy: fence contention"),
        )
    }

    /// r271.1: a busy host (`admission_busy`, nothing started) is retried
    /// inside the one background start: one `onlineRequested`, no failure
    /// events, the queue flushed once it is up.
    #[test]
    fn a_busy_host_is_retried_inside_the_start() {
        let (root, repository) = repository();
        let calls = RefCell::new(0);
        let flushed = RefCell::new(0);
        let online = |_: &str, _: Value| {
            *calls.borrow_mut() += 1;
            if *calls.borrow() <= 2 {
                return Err(host_busy());
            }
            Ok(json!({ "runtimeGeneration": 4, "status": "online", "runtimeAgentId": "agent-1" }))
        };
        let started = run_start(
            &repository,
            &plan("busy-f"),
            online,
            |_| {
                *flushed.borrow_mut() += 1;
                Ok(1)
            },
            |_| vec!["queue-1".into()],
            |_| {},
        )
        .expect("start");
        assert!(started);
        assert_eq!(*calls.borrow(), 3);
        assert_eq!(*flushed.borrow(), 1);
        assert_eq!(
            methods(&repository, "busy-f"),
            ["cutex/runtime/onlineRequested", "cutex/runtime/online"]
        );
        assert_eq!(start_outcome("busy-f"), Some(StartOutcome::Online));
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Past the budget the busy host is the start's real outcome, reported
    /// once (and recorded for owner rotation as a retryable failure).
    #[test]
    fn a_host_busy_past_the_budget_fails_the_start_once() {
        let (root, repository) = repository();
        let calls = RefCell::new(0);
        let online = |_: &str, _: Value| {
            *calls.borrow_mut() += 1;
            Err(host_busy())
        };
        let started = run_start(
            &repository,
            &plan("busy-g"),
            online,
            |_| panic!("no flush"),
            |_| vec!["queue-1".into()],
            |_| panic!("a failed start keeps the owner-stop mark"),
        )
        .expect("failure is reported, not raised");
        assert!(!started);
        assert!(*calls.borrow() > 2, "retried: {}", calls.borrow());
        assert_eq!(
            methods(&repository, "busy-g"),
            [
                "cutex/runtime/onlineRequested",
                "cutex/runtime/failed",
                "cutex/userInput/startFailed"
            ]
        );
        let Some(StartOutcome::Failed { code, retryable, .. }) = start_outcome("busy-g") else {
            panic!("failed outcome")
        };
        assert_eq!(code, crate::asks::pause_admission::ADMISSION_BUSY_CODE);
        assert!(retryable);
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

    /// Owner decision 8 / r274 decision 2 (switch on): an owner submit to an
    /// offline agent always asks for the start, with or without the flag;
    /// with the switch off (or another caller) the request is unchanged.
    #[test]
    fn owner_submits_always_start_an_offline_agent_with_the_switch_on() {
        for start in [None, Some(json!(false)), Some(json!(true))] {
            let request = request(start.clone());
            let owner = owner_starts_offline(true, &request);
            assert_eq!(owner.params[START_IF_OFFLINE], json!(true), "{start:?}");
            assert_eq!(owner_starts_offline(false, &request).params, request.params, "{start:?}: unchanged");
        }
        // A malformed flag keeps its validation error.
        let malformed = owner_starts_offline(true, &request(Some(json!("yes"))));
        assert_eq!(malformed.params[START_IF_OFFLINE], json!("yes"));
        // Through the dispatch: no flag, offline, owner default → queued + start.
        let (root, repository) = repository();
        let starts = RefCell::new(0);
        let result = dispatch_marking_bypass(&repository, fake_submit, "owner-default", &session(false, "offline"),
            &owner_starts_offline(true, &request(None)), |_| *starts.borrow_mut() += 1, |_| {}).expect("queued");
        assert_eq!(result["runtimeStarting"], json!(true));
        assert_eq!(*starts.borrow(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// r274 decision 2: an offline `queue/steer` refusal starts the agent for
    /// its item only for the owner with the switch on, a bound thread and a
    /// disconnected runtime; an unknown outcome or another error never does.
    #[test]
    fn an_offline_queue_steer_starts_the_agent_only_for_the_owner() {
        let steer = |queue_id: &str| ValidatedCutexRequest {
            request_id: "steer-1".into(),
            method: super::super::queue_order::STEER_METHOD.into(),
            params: json!({"queueId": queue_id, "expectedRevision": 1}),
        };
        let offline = super::super::queue_order::session_offline();
        let unavailable = UserInputExecutionError { stage: "queue".into(), code: "app_server_unavailable".into(),
            message: String::new(), retryable: true, details: json!({}), outcome_unknown: false };
        assert_eq!(offline_steer_item(true, &session(false, "offline"), &steer("q1"), &offline), Some("q1".into()));
        assert_eq!(offline_steer_item(true, &session(false, "offline"), &steer("q1"), &unavailable), Some("q1".into()));
        assert_eq!(offline_steer_item(false, &session(false, "offline"), &steer("q1"), &offline), None, "switch off / not the owner");
        assert_eq!(offline_steer_item(true, &session(true, "online"), &steer("q1"), &offline), None, "connected");
        let mut unbound = session(false, "offline");
        unbound["native"]["threadId"] = json!("");
        assert_eq!(offline_steer_item(true, &unbound, &steer("q1"), &offline), None, "no bound thread: nothing to resume");
        let mut unknown = unavailable.clone();
        unknown.outcome_unknown = true;
        assert_eq!(offline_steer_item(true, &session(false, "offline"), &steer("q1"), &unknown), None);
        let refused = super::super::queue_order::steer_unavailable("turnFinishing");
        assert_eq!(offline_steer_item(true, &session(false, "offline"), &steer("q1"), &refused), None);
    }
}
