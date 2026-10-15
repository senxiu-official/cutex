use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use chrono::Utc;
use serde_json::json;
use serde_json::Value;
use uuid::Uuid;

use cutex::app_server::client::AppServerEvent;
use cutex::app_server::commands::ThreadReadParams;
use cutex::app_server::commands::TurnInterruptParams;
use cutex::management::v2::model::CutexMessage;
use cutex::management::v2::model::EventCorrelation;
use cutex::management::v2::model::EventSource;
use cutex::management::v2::model::PendingEvent;
use cutex::management::v2::repository::management_v2_repository;
use cutex::management::v2::user_input::user_input_repository;
use cutex::management::v2::user_input::ClientIdentityDecision;
use cutex::management::v2::user_input::InputDelivery;
use cutex::management::v2::user_input::QueueEnqueueDecision;
use cutex::management::v2::user_input::UserInputDisposition;
use cutex::management::v2::user_input::UserInputExecutionError;
use cutex::management::v2::user_input::UserInputStrategy;
use cutex::management::v2::user_input::UserInputSubmitCommand;
use cutex::management::v2::user_input::UserInputSubmitExecution;
use cutex::platform::host::current_host_name;

use super::app_server_runtime;

const INTERRUPT_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
const INTERRUPT_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// How long a `sending` / `unknown` delivery holds back a resend while the
/// native thread is idle and does not show the input (C-010): a native
/// request that timed out may still be processed shortly after. Past this,
/// an idle thread without the input proves it never landed.
const UNRESOLVED_DELIVERY_GRACE: Duration = Duration::from_secs(30);
/// `retryAfterMs` of `user_input_in_progress`.
const DELIVERY_RETRY_AFTER_MS: u64 = 2_000;

pub(crate) fn submit_v2(
    command: UserInputSubmitCommand,
) -> Result<UserInputSubmitExecution, UserInputExecutionError> {
    let session_lock = session_input_lock(&command.cutex_session_id)
        .map_err(|error| execution_error("route", "internal_error", error, true, false))?;
    let _session_guard = session_lock.lock().map_err(|_| {
        execution_error_message(
            "route",
            "internal_error",
            "app-server session input lock was poisoned",
            true,
            false,
        )
    })?;
    let repository = user_input_repository().map_err(|error| {
        execution_error("route", "event_persistence_unavailable", error, true, false)
    })?;
    submit_v2_with_repository(
        command,
        repository,
        runtime_active_turn,
        super::active_turn::active_turn_id,
        read_thread,
        send_user_input_native_request,
        |cutex_session_id: &str, stop_before: Option<&str>| {
            // The session input lock is already held (see above).
            flush_queued_locked(
                cutex_session_id,
                false,
                repository,
                stop_before,
                read_thread,
                send_user_input_native_request,
                append_input_event,
            )
        },
    )
}

/// The bound thread's active turn as tracked by the connected runtime
/// (`turn/started` / `turn/completed` / status events): `None` when the runtime
/// is not connected to this thread, `Some(None)` when it is idle.
fn runtime_active_turn(cutex_session_id: &str, thread_id: &str) -> Option<Option<String>> {
    let status = session_manager(cutex_session_id)
        .status(cutex_session_id)
        .ok()
        .flatten()?;
    (status.connected && status.thread_id == thread_id).then_some(status.active_turn_id)
}

/// queue_order holds the shared input lock throughout this call and the
/// subsequent queue removal/events. This path deliberately has no enqueue fallback.
pub(crate) fn steer_queued_v2(
    command: &UserInputSubmitCommand,
    start_if_idle: bool,
    _session: &Value,
) -> Result<UserInputSubmitExecution, UserInputExecutionError> {
    let repository = user_input_repository().map_err(queue_persistence_error)?;
    let status = session_manager(&command.cutex_session_id)
        .status(&command.cutex_session_id)
        .map_err(|_| cutex::management::v2::queue_order::session_offline())?
        .filter(|status| status.connected && status.thread_id == command.thread_id)
        .ok_or_else(cutex::management::v2::queue_order::session_offline)?;
    let lease_blocks_input = admission_lease_blocks_owner_input(
        &command.cutex_session_id,
        status.active_turn_id.as_deref(),
    );
    let execution = steer_queued_with_repository(
        command,
        start_if_idle,
        repository,
        status.active_turn_id,
        lease_blocks_input,
        || {
            if is_claude_session(&command.cutex_session_id) {
                // Read the live generation's capabilities, never a static
                // feature file. A guarded adapter can still refuse this turn
                // or phase; that refusal keeps the original queue item.
                super::asks_claude::adapter_capabilities(
                    &command.cutex_session_id,
                    status.runtime_generation,
                )
                .is_some_and(|caps| {
                    caps.pointer("/input/sameTurnSteering")
                        .and_then(Value::as_bool)
                        == Some(true)
                })
            } else {
                // The Codex app-server implements `turn/steer` itself; its
                // refusals (no active turn, review / compact turn) are
                // mapped by `steer_failure`.
                true
            }
        },
        send_user_input_native_request,
    )?;
    // Owner input ends the agent's `agent_wait` at once (asks §5.8), as for
    // a submit: a steered input is read by the running turn, which may be
    // waiting.
    cutex::asks::waits::cancel_waits_default(&command.cutex_session_id);
    Ok(execution)
}

fn steer_queued_with_repository(
    command: &UserInputSubmitCommand,
    start_if_idle: bool,
    repository: &cutex::management::v2::user_input::UserInputRepository,
    active_turn_id: Option<String>,
    lease_blocks_input: bool,
    supports_steer: impl FnOnce() -> bool,
    send_native: impl Fn(
        &UserInputSubmitCommand,
        &str,
        Value,
        UserInputDisposition,
    ) -> Result<UserInputSubmitExecution, UserInputExecutionError>,
) -> Result<UserInputSubmitExecution, UserInputExecutionError> {
    if let Some(expected) = &command.params.expected_turn_id {
        if active_turn_id.as_ref() != Some(expected) {
            return Err(UserInputExecutionError {
                stage: "queue".into(),
                code: "turn_conflict".into(),
                message: "the active turn changed; the queued input was kept".into(),
                retryable: true,
                outcome_unknown: false,
                details: json!({"expectedTurnId": expected, "activeTurnId": active_turn_id}),
            });
        }
    }
    if lease_blocks_input {
        return Err(cutex::management::v2::queue_order::steer_unavailable(
            "delegationTurn",
        ));
    }
    let (method, disposition) = match active_turn_id.as_ref() {
        Some(_) if !supports_steer() => {
            return Err(cutex::management::v2::queue_order::steer_unavailable(
                "runtimeNoSteer",
            ))
        }
        Some(_) => ("turn/steer", UserInputDisposition::Steered),
        None if !start_if_idle => {
            return Err(cutex::management::v2::queue_order::steer_unavailable(
                "noActiveTurn",
            ))
        }
        None => ("turn/start", UserInputDisposition::Started),
    };
    let mut params = json!({
        "threadId": command.thread_id,
        "input": command.params.input,
        "clientUserMessageId": command.params.client_user_message_id,
    });
    if let Some(turn_id) = active_turn_id {
        params["expectedTurnId"] = json!(turn_id);
    }
    send_recorded(
        repository,
        &send_native,
        command,
        method,
        params,
        disposition,
    )
    .map_err(|error| steer_failure(method, error))
}

/// Map a failed `turn/steer` / idle `turn/start` to the queue contract (the
/// queued input is kept in every case but `outcome_unknown`):
/// - outcome unknown → `outcome_unknown` (never auto-resent);
/// - runtime gone mid-request → `session_offline`;
/// - Codex "no active turn" / expected-turn mismatch, and any refused idle
///   `turn/start` (a turn started meanwhile) → `turn_conflict` (retry);
/// - a runtime without steer (-32601 / `data.unsupported`) →
///   `steer_unavailable{runtimeNoSteer}`;
/// - any other refusal (review / compact turn, draining server, guarded
///   adapter phase, ...) → `steer_unavailable{runtimeRefused}` with the
///   native error.
fn steer_failure(method: &str, mut error: UserInputExecutionError) -> UserInputExecutionError {
    if error.outcome_unknown {
        error.code = "outcome_unknown".into();
        error.retryable = false;
        return error;
    }
    match error.code.as_str() {
        "app_server_unavailable" => {
            let mut offline = cutex::management::v2::queue_order::session_offline();
            offline.details = json!({"cause": error.message});
            offline
        }
        "native_request_failed" => {
            let native = error.details.get("nativeError").cloned().unwrap_or(Value::Null);
            let message = native.get("message").and_then(Value::as_str).unwrap_or(&error.message);
            let unsupported = native.get("code").and_then(Value::as_i64) == Some(-32601)
                || native.pointer("/data/unsupported").and_then(Value::as_bool) == Some(true);
            let turn_changed = message.starts_with("no active turn to steer")
                || message.starts_with("expected active turn id");
            if unsupported {
                let mut refused = cutex::management::v2::queue_order::steer_unavailable("runtimeNoSteer");
                refused.details["nativeError"] = native;
                refused
            } else if turn_changed || method == "turn/start" {
                error.code = "turn_conflict".into();
                error.retryable = true;
                error
            } else {
                let mut refused = cutex::management::v2::queue_order::steer_unavailable("runtimeRefused");
                refused.details["nativeError"] = native;
                refused
            }
        }
        _ => error,
    }
}

/// New identities need only an active-turn lookup, including when runtime
/// tracking is missing or a native refusal proves the tracked state stale.
/// Full history is reserved for repeated identities: C-010 must still detect
/// late acceptance, unresolved sends, and turns removed by a revert.
fn submit_v2_with_repository(
    command: UserInputSubmitCommand,
    repository: &cutex::management::v2::user_input::UserInputRepository,
    tracked_active_turn: impl Fn(&str, &str) -> Option<Option<String>>,
    read_active_turn: impl Fn(&str, &str) -> anyhow::Result<Option<String>>,
    read_native_thread: impl Fn(&str, &str) -> anyhow::Result<Value>,
    send_native_raw: impl Fn(
        &UserInputSubmitCommand,
        &str,
        Value,
        UserInputDisposition,
    ) -> Result<UserInputSubmitExecution, UserInputExecutionError>,
    flush_ahead: impl Fn(&str, Option<&str>) -> anyhow::Result<bool>,
) -> Result<UserInputSubmitExecution, UserInputExecutionError> {
    let send_native = |command: &UserInputSubmitCommand,
                       method: &str,
                       params: Value,
                       disposition: UserInputDisposition| {
        send_recorded(repository, &send_native_raw, command, method, params, disposition)
    };
    // Owner input ends the agent's `agent_wait` at once (asks §5.8).
    cutex::asks::waits::cancel_waits_default(&command.cutex_session_id);
    let new_identity = match repository
        .register_identity(&command.cutex_session_id, &command.params)
        .map_err(|error| {
            execution_error("route", "event_persistence_unavailable", error, true, false)
        })? {
        ClientIdentityDecision::Conflict => {
            return Err(execution_error_message(
                "route",
                "idempotency_conflict",
                "clientUserMessageId was already used with different input or origin",
                false,
                false,
            ));
        }
        ClientIdentityDecision::New => true,
        ClientIdentityDecision::Existing => false,
    };

    if new_identity {
        if let Some(active_turn_id) =
            tracked_active_turn(&command.cutex_session_id, &command.thread_id)
        {
            let expected_matches = command
                .params
                .expected_turn_id
                .as_deref()
                .is_none_or(|expected| active_turn_id.as_deref() == Some(expected));
            if expected_matches {
                match route_submit(
                    &command,
                    repository,
                    active_turn_id,
                    true,
                    &send_native,
                    &flush_ahead,
                ) {
                    Err(error)
                        if error.code == "native_request_failed" && !error.outcome_unknown => {}
                    result => return result,
                }
            }
        }
        // The identity was just created, and any request above was definitely
        // refused. There is no earlier input to find in history. A fresh,
        // bounded turn summary is sufficient to route this input safely.
        let (active_turn_id, turn_state_known) =
            match read_active_turn(&command.cutex_session_id, &command.thread_id) {
                Ok(active_turn_id) => (active_turn_id, true),
                Err(_)
                    if command.params.strategy == UserInputStrategy::Queue
                        && command.params.expected_turn_id.is_none() => (None, false),
                Err(error) => {
                    return Err(execution_error(
                        "native_request", "app_server_unavailable", error, true, false,
                    ));
                }
            };
        return route_submit(
            &command, repository, active_turn_id, turn_state_known, &send_native, &flush_ahead,
        );
    }

    let thread = match read_native_thread(&command.cutex_session_id, &command.thread_id) {
        Ok(thread) => Some(thread),
        Err(_)
            if command.params.strategy == UserInputStrategy::Queue
                && command.params.expected_turn_id.is_none() =>
        {
            None
        }
        Err(error) => {
            return Err(execution_error(
                "native_request",
                "app_server_unavailable",
                error,
                true,
                false,
            ))
        }
    };
    let visible_turn_id = thread
        .as_ref()
        .and_then(|thread| client_message_turn_id(thread, &command.params.client_user_message_id));
    let deduplicated_turn = match visible_turn_id {
        Some(turn_id) => Some(Some(turn_id)),
        // Not (yet) in the history: what cutex recorded about an earlier
        // send of this clientUserMessageId decides (C-010).
        None => match resolve_prior_delivery(
            repository,
            &command.cutex_session_id,
            &command.params.client_user_message_id,
            &command.thread_id,
            thread.as_ref(),
            Utc::now(),
            UNRESOLVED_DELIVERY_GRACE,
        )
        .map_err(|error| {
            execution_error("route", "event_persistence_unavailable", error, true, false)
        })? {
            PriorDelivery::None => None,
            PriorDelivery::Deduplicated(turn_id) => Some(turn_id),
            PriorDelivery::InProgress(delivery) => {
                return Err(delivery_in_progress_error(
                    &command.params.client_user_message_id,
                    Some(&delivery),
                ))
            }
        },
    };
    if let Some(turn_id) = deduplicated_turn {
        return Ok(UserInputSubmitExecution {
            disposition: UserInputDisposition::Deduplicated,
            app_server_accepted: true,
            native_request_id: None,
            native_method: None,
            turn_id,
            queue: None,
            queue_paused: false,
        });
    }
    let active_turn_id = thread.as_ref().and_then(active_turn_id_from_thread);
    route_submit(
        &command,
        repository,
        active_turn_id,
        thread.is_some(),
        &send_native,
        &flush_ahead,
    )
}

/// Routes a submit given the native thread's active turn: queue, steer,
/// interrupt + start, or start. `turn_state_known`: `active_turn_id` comes
/// from the connected runtime or a fresh thread read (not an offline guess),
/// so `None` really means idle.
fn route_submit(
    command: &UserInputSubmitCommand,
    repository: &cutex::management::v2::user_input::UserInputRepository,
    active_turn_id: Option<String>,
    turn_state_known: bool,
    send_native: &impl Fn(
        &UserInputSubmitCommand,
        &str,
        Value,
        UserInputDisposition,
    ) -> Result<UserInputSubmitExecution, UserInputExecutionError>,
    flush_ahead: &impl Fn(&str, Option<&str>) -> anyhow::Result<bool>,
) -> Result<UserInputSubmitExecution, UserInputExecutionError> {
    if let Some(expected_turn_id) = command.params.expected_turn_id.as_deref() {
        if active_turn_id.as_deref() != Some(expected_turn_id) {
            return Err(UserInputExecutionError {
                stage: "route".to_string(),
                code: "turn_conflict".to_string(),
                message: "expectedTurnId does not match the active native turn".to_string(),
                retryable: true,
                details: json!({
                    "expectedTurnId": expected_turn_id,
                    "activeTurnId": active_turn_id,
                }),
                outcome_unknown: false,
            });
        }
    }

    // Claude runtimes have no same-turn steering: an `auto` submit while a
    // turn runs becomes an ordinary Management queue item, flushed as the
    // next turn when the active one completes.
    let claude_queue = command.params.strategy == UserInputStrategy::Auto
        && active_turn_id.is_some()
        && is_claude_session(&command.cutex_session_id);
    // Ask leases keep owner input separate (asks §5.3). Event/Owner leases
    // only block starts; they allow steering into an already running turn.
    let ask_queue = command.params.strategy == UserInputStrategy::Auto
        && admission_lease_blocks_owner_input(
            &command.cutex_session_id,
            active_turn_id.as_deref(),
        );
    if matches!(command.params.strategy, UserInputStrategy::Queue | UserInputStrategy::Auto)
        && turn_state_known
        && active_turn_id.is_none()
    {
        if let Some(execution) = queue_on_idle(command, repository, flush_ahead)? {
            return Ok(execution);
        }
        // Nothing queued, nothing held: start it below like `auto`.
    } else if command.params.strategy == UserInputStrategy::Queue || claude_queue || ask_queue {
        return enqueue_submit(repository, command, active_turn_id);
    }

    if let Some(active_turn_id) = active_turn_id {
        match command.params.strategy {
            UserInputStrategy::Auto => {
                return send_native(
                    command,
                    "turn/steer",
                    json!({
                        "threadId": command.thread_id,
                        "input": command.params.input,
                        "clientUserMessageId": command.params.client_user_message_id,
                        "expectedTurnId": active_turn_id,
                    }),
                    UserInputDisposition::Steered,
                );
            }
            UserInputStrategy::Interrupt => {
                session_manager(&command.cutex_session_id)
                    .commands(&command.cutex_session_id)
                    .and_then(|commands| {
                        commands
                            .turn_interrupt(&TurnInterruptParams {
                                thread_id: command.thread_id.clone(),
                                turn_id: active_turn_id.clone(),
                            })
                            .map(|_| ())
                            .map_err(Into::into)
                    })
                    .map_err(|error| {
                        execution_error("interrupt", "app_server_unavailable", error, true, true)
                    })?;
                wait_for_turn_to_stop(&command.cutex_session_id, &active_turn_id).map_err(
                    |error| execution_error("interrupt", "interrupt_timeout", error, true, true),
                )?;
            }
            UserInputStrategy::Queue => unreachable!("explicit queue returned above"),
        }
    }

    match send_native(
        command,
        "turn/start",
        json!({
            "threadId": command.thread_id,
            "input": command.params.input,
            "clientUserMessageId": command.params.client_user_message_id,
        }),
        UserInputDisposition::Started,
    ) {
        // The Claude adapter is about to run input it queued itself (an Agent
        // Bus message): keep this one in the Management queue instead of
        // failing it; it is flushed when that turn completes.
        Err(error) if is_native_thread_busy(&error) => enqueue_submit(repository, command, None),
        result => result,
    }
}

/// `queue` while the agent is idle. A queued follow-up waits for the running
/// turn, but with no turn running nothing would ever flush it: the idle
/// flush (turn/completed, thread idle) may already have run on an empty
/// queue just before this submit (the client still saw the turn running).
/// Runs under the session input lock, like every flush, so no flush can
/// interleave:
/// - explicit queue paused (`cutex/turn/stop`) or an ask (委托) goes first: queued and
///   held (`queue_paused` tells the client when it is the pause);
/// - nothing eligible queued (including paused owners for `auto`): `None`, start now;
/// - older items waiting (an earlier flush failed, or the runtime just
///   connected): queued behind them, and the front item goes out now, as a
///   turn end would send it.
fn queue_on_idle(
    command: &UserInputSubmitCommand,
    repository: &cutex::management::v2::user_input::UserInputRepository,
    flush_ahead: &impl Fn(&str, Option<&str>) -> anyhow::Result<bool>,
) -> Result<Option<UserInputSubmitExecution>, UserInputExecutionError> {
    let session = &command.cutex_session_id;
    let paused = repository
        .pause_state(session)
        .map_err(queue_persistence_error)?
        .is_some();
    let (_, waiting) = repository.list(session).map_err(queue_persistence_error)?;
    let owner_head = (!paused).then(|| waiting.first()).flatten().map(|item| item.queue_id.as_str());
    let asks_first = ask_lease_held(session)
        || !repository.owner_may_start(session, owner_head).map_err(queue_persistence_error)?;
    if (paused && command.params.strategy == UserInputStrategy::Queue) || asks_first {
        return enqueue_submit(repository, command, None).map(Some);
    }
    if paused || waiting.is_empty() {
        return Ok(None);
    }
    let execution = enqueue_submit(repository, command, None)?;
    // A newly queued item is announced (`cutex/userInput/queued`) only after
    // this returns, so the flush stops before it; a repeated submit of an
    // item already waiting may go out now.
    let stop_before = (execution.disposition == UserInputDisposition::Queued)
        .then_some(command.params.client_user_message_id.as_str());
    if let Err(error) = flush_ahead(session, stop_before) {
        eprintln!(
            "\x1b[33mwarning:\x1b[0m idle queue flush for {session} failed: {error:#}"
        );
    }
    Ok(Some(execution))
}

fn queue_persistence_error(error: anyhow::Error) -> UserInputExecutionError {
    execution_error("queue", "event_persistence_unavailable", error, true, false)
}

fn enqueue_submit(
    repository: &cutex::management::v2::user_input::UserInputRepository,
    command: &UserInputSubmitCommand,
    active_turn_id: Option<String>,
) -> Result<UserInputSubmitExecution, UserInputExecutionError> {
    let decision = repository
        .enqueue(
            &command.cutex_session_id,
            &command.thread_id,
            &command.management_request_id,
            &command.params,
        )
        .map_err(queue_persistence_error)?;
    // The pause outlives the enqueue (a paused queue is never empty).
    let queue_paused = || {
        repository
            .pause_state(&command.cutex_session_id)
            .map(|pause| pause.is_some())
            .map_err(queue_persistence_error)
    };
    match decision {
        QueueEnqueueDecision::Queued(queue) => Ok(UserInputSubmitExecution {
            disposition: UserInputDisposition::Queued,
            app_server_accepted: false,
            native_request_id: None,
            native_method: None,
            turn_id: active_turn_id,
            queue: Some(queue),
            queue_paused: queue_paused()?,
        }),
        QueueEnqueueDecision::Deduplicated(queue) => Ok(UserInputSubmitExecution {
            disposition: UserInputDisposition::Deduplicated,
            app_server_accepted: false,
            native_request_id: None,
            native_method: None,
            turn_id: active_turn_id,
            queue: Some(queue),
            queue_paused: queue_paused()?,
        }),
        QueueEnqueueDecision::ClientMessageConflict => Err(execution_error_message(
            "queue",
            "idempotency_conflict",
            "clientUserMessageId was already used with different input or origin",
            false,
            false,
        )),
    }
}

/// The Claude adapter refuses `turn/start` while input it queued itself is
/// pending (`data.reason: "threadBusy"`); that is "not idle yet", not a failure.
fn is_native_thread_busy(error: &UserInputExecutionError) -> bool {
    error.code == "native_request_failed"
        && error
            .details
            .pointer("/nativeError/data/reason")
            .and_then(Value::as_str)
            == Some("threadBusy")
}

/// What an earlier send of a clientUserMessageId means for this one, when the
/// native history does not (yet) show the input.
#[derive(Debug, PartialEq)]
enum PriorDelivery {
    /// Never sent, refused, or provably not in the thread any more: send.
    None,
    /// Native accepted it (into this turn, when known): do not send again.
    Deduplicated(Option<String>),
    /// A send is unresolved (in flight, or its outcome is unknown) and may
    /// still land: neither send nor claim success; retry later.
    InProgress(InputDelivery),
}

/// Resolves the recorded delivery of `client_user_message_id` against the
/// native thread just read (`thread`: `None` when it could not be read).
/// Clears a delivery that provably did not end up in this thread, so the
/// caller may send again:
/// - `accepted` into a turn the thread no longer lists (history revert);
/// - recorded for another native thread;
/// - `sending` / `unknown` older than `grace` while the thread is idle and
///   does not show the input (an accepted input is in an idle thread's
///   history).
fn resolve_prior_delivery(
    repository: &cutex::management::v2::user_input::UserInputRepository,
    cutex_session_id: &str,
    client_user_message_id: &str,
    thread_id: &str,
    thread: Option<&Value>,
    now: chrono::DateTime<Utc>,
    grace: Duration,
) -> anyhow::Result<PriorDelivery> {
    let Some(delivery) = repository.delivery(cutex_session_id, client_user_message_id)? else {
        return Ok(PriorDelivery::None);
    };
    // Queued inputs retain unresolved reservations across timeouts and
    // binding changes. Neither an idle snapshot nor a new thread proves
    // that the earlier native request will never execute.
    if matches!(delivery, InputDelivery::Sending { .. } | InputDelivery::Unknown { .. })
        && repository.list(cutex_session_id)?.1.iter()
            .any(|item| item.client_user_message_id == client_user_message_id)
    {
        return Ok(PriorDelivery::InProgress(delivery));
    }
    let clear = |delivery: &InputDelivery| -> anyhow::Result<PriorDelivery> {
        if repository.set_delivery(cutex_session_id, client_user_message_id, Some(delivery), None)? {
            Ok(PriorDelivery::None)
        } else {
            // Changed underneath (another process): do not guess.
            Ok(PriorDelivery::InProgress(delivery.clone()))
        }
    };
    if delivery.thread_id() != thread_id {
        return clear(&delivery);
    }
    match &delivery {
        InputDelivery::Accepted { turn_id, .. } => {
            let still_listed = match (thread, turn_id) {
                (Some(thread), Some(turn_id)) => thread_lists_turn(thread, turn_id),
                // Unverifiable (no thread read / no turn id): trust the receipt.
                _ => true,
            };
            if still_listed {
                Ok(PriorDelivery::Deduplicated(turn_id.clone()))
            } else {
                clear(&delivery)
            }
        }
        InputDelivery::Sending { since, .. } | InputDelivery::Unknown { since, .. } => {
            let Some(thread) = thread else {
                return Ok(PriorDelivery::InProgress(delivery.clone()));
            };
            let settled = chrono::DateTime::parse_from_rfc3339(since)
                .map(|since| {
                    now.signed_duration_since(since)
                        .to_std()
                        .is_ok_and(|age| age >= grace)
                })
                .unwrap_or(true);
            if active_turn_id_from_thread(thread).is_none() && settled {
                eprintln!(
                    "\x1b[33mwarning:\x1b[0m {cutex_session_id}: an earlier send of clientUserMessageId {client_user_message_id} is not in the idle native thread; sending it again"
                );
                clear(&delivery)
            } else {
                Ok(PriorDelivery::InProgress(delivery.clone()))
            }
        }
    }
}

/// `user_input_in_progress` (HTTP 202, retryable, not recorded for requestId
/// replay): an earlier send of this clientUserMessageId is unresolved.
fn delivery_in_progress_error(
    client_user_message_id: &str,
    delivery: Option<&InputDelivery>,
) -> UserInputExecutionError {
    let state = match delivery {
        Some(InputDelivery::Sending { .. }) => "sending",
        Some(InputDelivery::Unknown { .. }) => "unknown",
        Some(InputDelivery::Accepted { .. }) => "accepted",
        None => "unresolved",
    };
    UserInputExecutionError {
        stage: "route".to_string(),
        code: "user_input_in_progress".to_string(),
        message: "an earlier submit of this clientUserMessageId has not resolved yet; nothing was sent, retry later".to_string(),
        retryable: true,
        details: json!({
            "clientUserMessageId": client_user_message_id,
            "delivery": state,
            "retryAfterMs": DELIVERY_RETRY_AFTER_MS,
        }),
        outcome_unknown: false,
    }
}

/// Every native user-input request goes through here (C-010): the delivery
/// is reserved (`sending`) before the request leaves, so no second request
/// for the same clientUserMessageId can start while it is unresolved, and
/// recorded as `accepted` (with the turn) / `unknown` afterwards; a definite
/// refusal clears it.
fn send_recorded(
    repository: &cutex::management::v2::user_input::UserInputRepository,
    send_native: &impl Fn(
        &UserInputSubmitCommand,
        &str,
        Value,
        UserInputDisposition,
    ) -> Result<UserInputSubmitExecution, UserInputExecutionError>,
    command: &UserInputSubmitCommand,
    method: &str,
    params: Value,
    disposition: UserInputDisposition,
) -> Result<UserInputSubmitExecution, UserInputExecutionError> {
    let session = &command.cutex_session_id;
    let client_id = &command.params.client_user_message_id;
    let sending = InputDelivery::Sending {
        thread_id: command.thread_id.clone(),
        since: Utc::now().to_rfc3339(),
    };
    let reserved = repository
        .set_delivery(session, client_id, None, Some(sending.clone()))
        .map_err(|error| {
            execution_error("route", "event_persistence_unavailable", error, true, false)
        })?;
    if !reserved {
        let current = repository.delivery(session, client_id).ok().flatten();
        return Err(delivery_in_progress_error(client_id, current.as_ref()));
    }
    match send_native(command, method, params, disposition) {
        Ok(execution) => {
            let accepted = InputDelivery::Accepted {
                thread_id: command.thread_id.clone(),
                turn_id: execution.turn_id.clone(),
                accepted_at: Utc::now().to_rfc3339(),
            };
            match repository.set_delivery(session, client_id, Some(&sending), Some(accepted)) {
                Ok(true) => Ok(execution),
                // Native accepted it, but a retry could not tell: report the
                // outcome as unknown (the `sending` record keeps blocking a
                // resend until the history shows the input or proves it absent).
                Ok(false) => Err(execution_error_message(
                    "post_submit",
                    "event_persistence_unavailable",
                    "native accepted the input but its delivery record changed underneath",
                    false,
                    true,
                )),
                Err(error) => Err(execution_error(
                    "post_submit",
                    "event_persistence_unavailable",
                    error,
                    false,
                    true,
                )),
            }
        }
        Err(error) => {
            let next = error.outcome_unknown.then(|| InputDelivery::Unknown {
                thread_id: command.thread_id.clone(),
                since: Utc::now().to_rfc3339(),
            });
            // Best effort: if this write fails, `sending` stays and is
            // resolved like `unknown`.
            if let Err(record_error) =
                repository.set_delivery(session, client_id, Some(&sending), next)
            {
                eprintln!(
                    "\x1b[33mwarning:\x1b[0m {session}: could not record the delivery of {client_id}: {record_error:#}"
                );
            }
            Err(error)
        }
    }
}

fn send_user_input_native_request(
    command: &UserInputSubmitCommand,
    method: &str,
    params: Value,
    disposition: UserInputDisposition,
) -> Result<UserInputSubmitExecution, UserInputExecutionError> {
    use cutex::app_server::client::AppServerClientError;

    let native_request_id = format!("cutex-user-input-{}", Uuid::new_v4());
    let handle = session_manager(&command.cutex_session_id)
        .handle(&command.cutex_session_id)
        .map_err(|error| {
            execution_error(
                "native_request",
                "app_server_unavailable",
                error,
                true,
                false,
            )
        })?;
    let send = |native_request_id: &str| handle
        .request_raw_message(json!({
            "id": native_request_id,
            "method": method,
            "params": params,
        }))
        .map_err(|error| {
            let outcome_unknown = matches!(
                error,
                AppServerClientError::Transport(_)
                    | AppServerClientError::Disconnected(_)
                    | AppServerClientError::Timeout { .. }
            );
            execution_error_message(
                "native_request",
                if outcome_unknown {
                    "request_outcome_unknown"
                } else {
                    "app_server_unavailable"
                },
                &error.to_string(),
                !outcome_unknown,
                outcome_unknown,
            )
        });
    let mut native_request_id = native_request_id;
    let mut response = send(&native_request_id)?;
    // The owner's own message to a Claude agent whose adapter closed its
    // input gate (the last turn was interrupted or failed, tool effects may
    // be uncertain) is the owner's acknowledgement: reopen the gate, as
    // `cutex/asks/resume` does, and send it once more. Never for agent
    // deliveries.
    if method == "turn/start"
        && response.get("error").is_some_and(is_claude_gate_refusal)
        && owner_reopens_gate(command)
        && is_claude_session(&command.cutex_session_id)
    {
        match super::asks_claude::resume(&command.cutex_session_id) {
            Ok(()) => {
                eprintln!(
                    "user input: reopened the Claude input gate of {} for the owner's message",
                    command.cutex_session_id
                );
                native_request_id = format!("cutex-user-input-{}", Uuid::new_v4());
                response = send(&native_request_id)?;
            }
            Err(error) => eprintln!(
                "user input: could not reopen the Claude input gate of {}: {error:#}",
                command.cutex_session_id
            ),
        }
    }
    if let Some(error) = response.get("error") {
        return Err(native_user_input_failure(error));
    }
    if method == "turn/start" {
        cutex::management::v2::admission::owner_started(&command.cutex_session_id);
    }
    let turn_id = response
        .pointer("/result/turn/id")
        .or_else(|| response.pointer("/result/turnId"))
        .and_then(Value::as_str)
        .map(str::to_string);
    session_manager(&command.cutex_session_id)
        .note_active_turn(&command.cutex_session_id, turn_id.clone())
        .map_err(|error| execution_error("post_submit", "internal_error", error, false, true))?;
    Ok(UserInputSubmitExecution {
        disposition,
        app_server_accepted: true,
        native_request_id: Some(Value::String(native_request_id)),
        native_method: Some(method.to_string()),
        turn_id,
        queue: None,
        queue_paused: false,
    })
}

/// The Claude adapter refused `turn/start` because its input queue is
/// halted ("queue paused without a terminal failed/interrupted turn") or its
/// interruption gate is closed.
fn is_claude_gate_refusal(error: &Value) -> bool {
    error.get("code").and_then(Value::as_i64) == Some(-32009)
        && error.get("message").and_then(Value::as_str).is_some_and(|message| {
            message.contains("queue paused") || message.contains("interruption gate is closed")
        })
}

/// Owner input (desktop, TUI, CLI, Cutex automation the owner enabled), not
/// an Agent Bus delivery.
fn owner_reopens_gate(command: &UserInputSubmitCommand) -> bool {
    !matches!(
        command.params.origin.kind,
        cutex::management::v2::user_input::UserInputOriginKind::AgentBus
    )
}

fn native_user_input_failure(error: &Value) -> UserInputExecutionError {
    // A JSON-RPC response can also report an uncertain native write. Keep
    // its reservation, just as for a timeout or broken transport.
    let outcome_unknown = error
        .pointer("/data/outcomeUnknown")
        .and_then(Value::as_bool)
        == Some(true);
    UserInputExecutionError {
        stage: "native_request".into(),
        code: if outcome_unknown {
            "request_outcome_unknown"
        } else {
            "native_request_failed"
        }
        .into(),
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("native user-input request failed")
            .into(),
        retryable: false,
        details: json!({"nativeError": error}),
        outcome_unknown,
    }
}

fn execution_error(
    stage: &str,
    code: &str,
    error: impl std::fmt::Display,
    retryable: bool,
    outcome_unknown: bool,
) -> UserInputExecutionError {
    execution_error_message(stage, code, &error.to_string(), retryable, outcome_unknown)
}

fn execution_error_message(
    stage: &str,
    code: &str,
    message: &str,
    retryable: bool,
    outcome_unknown: bool,
) -> UserInputExecutionError {
    UserInputExecutionError {
        stage: stage.to_string(),
        code: code.to_string(),
        message: message.to_string(),
        retryable,
        details: json!({}),
        outcome_unknown,
    }
}

pub(crate) fn handle_runtime_event(
    cutex_session_id: &str,
    event: &AppServerEvent,
) -> anyhow::Result<()> {
    let AppServerEvent::Notification(notification) = event else {
        return Ok(());
    };
    let should_flush = notification.method == "turn/completed"
        || (notification.method == "thread/status/changed"
            && notification
                .params
                .as_ref()
                .and_then(|params| params.pointer("/status/type"))
                .and_then(Value::as_str)
                == Some("idle"));
    if should_flush {
        let Some(status) = session_manager(cutex_session_id).status(cutex_session_id)? else { return Ok(()); };
        let thread_id = notification.params.as_ref().and_then(|params| params.get("threadId")).and_then(Value::as_str);
        if thread_id.is_some_and(|thread| thread != status.thread_id) || status.active_turn_id.is_some() {
            return Ok(());
        }
        // The manager already applied the matching thread/turn event and
        // cleared admission state before observers run.
        flush_queued_if_idle(cutex_session_id)?;
    }
    Ok(())
}

/// Automatic flush (turn completed, thread idle, runtime connected): sends
/// the next queued item when the native thread is idle, unless the queue is
/// paused (`cutex/turn/stop`); then only `bypassPause` items go.
pub(crate) fn flush_queued_if_idle(cutex_session_id: &str) -> anyhow::Result<bool> {
    flush_queued(cutex_session_id, false)
}

/// `ignore_pause`: an explicit `cutex/userInput/queue/flush` sends the front
/// item even while the queue is paused (the pause itself stays).
pub(crate) fn flush_queued(cutex_session_id: &str, ignore_pause: bool) -> anyhow::Result<bool> {
    let session_lock = session_input_lock(cutex_session_id)?;
    let _session_guard = session_lock
        .lock()
        .map_err(|_| anyhow::anyhow!("app-server session input lock was poisoned"))?;
    let repository = user_input_repository()?;
    flush_queued_locked(
        cutex_session_id,
        ignore_pause,
        repository,
        None,
        read_thread,
        send_user_input_native_request,
        append_input_event,
    )
}

/// `flush_queued` with the session input lock already held. `stop_before`:
/// a clientUserMessageId this flush must not send (a submit that just
/// queued it and has not announced it yet).
fn flush_queued_locked(
    cutex_session_id: &str,
    ignore_pause: bool,
    repository: &cutex::management::v2::user_input::UserInputRepository,
    stop_before: Option<&str>,
    read_native_thread: impl Fn(&str, &str) -> anyhow::Result<Value>,
    send_native: impl Fn(
        &UserInputSubmitCommand,
        &str,
        Value,
        UserInputDisposition,
    ) -> Result<UserInputSubmitExecution, UserInputExecutionError>,
    append_event: impl Fn(&str, EventCorrelation, &str, Value) -> anyhow::Result<()>,
) -> anyhow::Result<bool> {
    let mut changed = false;
    loop {
        let Some((management_request_id, thread_id, queue_item)) =
            repository.next_flushable(cutex_session_id, ignore_pause)?
        else {
            return Ok(changed);
        };
        if stop_before == Some(queue_item.client_user_message_id.as_str()) {
            return Ok(changed);
        }
        let thread = read_native_thread(cutex_session_id, &thread_id)?;
        // Visible in native history: native took it (a flush or steer whose
        // response was lost, or a crash after the native write): only the
        // queue item is left to remove, as before 02. No adapter in this
        // release projects an input before its write (Claude has no
        // same-turn steering here); one that does must add a durable receipt
        // check before this.
        let visible = client_message_turn_id(&thread, &queue_item.client_user_message_id).is_some();
        if !visible
            && matches!(repository.delivery(cutex_session_id, &queue_item.client_user_message_id)?,
                Some(InputDelivery::Sending { .. } | InputDelivery::Unknown { .. }))
        {
            // Unresolved and not in history: never resent automatically. The
            // queue list shows `delivery` so the owner can decide (remove).
            return Ok(changed);
        }
        let already_submitted = visible
            || match resolve_prior_delivery(
                repository,
                cutex_session_id,
                &queue_item.client_user_message_id,
                &thread_id,
                Some(&thread),
                Utc::now(),
                UNRESOLVED_DELIVERY_GRACE,
            )? {
                PriorDelivery::None => false,
                // Accepted earlier (e.g. the flush failed after native took
                // it): only the queue item is left to remove (C-010).
                PriorDelivery::Deduplicated(_) => true,
                // Unresolved: a later flush decides.
                PriorDelivery::InProgress(_) => return Ok(changed),
            };
        if already_submitted {
            let removed = repository
                .remove(cutex_session_id, &queue_item.queue_id, queue_item.revision)?
                .context("queued user input disappeared during materialization cleanup")?;
            append_queue_removed_event(
                &append_event,
                cutex_session_id,
                &management_request_id,
                &removed,
                "submitted",
                repository.list(cutex_session_id)?.0,
            )?;
            changed = true;
            continue;
        }
        if active_turn_id_from_thread(&thread).is_some() {
            return Ok(changed);
        }
        // Asks (委托) share this session's admission: no flush while an ask
        // holds the lease, and an ask queued earlier goes first (FIFO). An
        // explicit flush is the owner's decision.
        if !ignore_pause && !repository.owner_may_start(cutex_session_id, Some(&queue_item.queue_id))? {
            return Ok(changed);
        }
        let command = UserInputSubmitCommand {
            management_request_id: management_request_id.clone(),
            cutex_session_id: cutex_session_id.to_string(),
            thread_id: thread_id.clone(),
            params: cutex::management::v2::user_input::UserInputSubmitParams {
                client_user_message_id: queue_item.client_user_message_id.clone(),
                origin: queue_item.origin.clone(),
                strategy: UserInputStrategy::Auto,
                input: queue_item.input.clone(),
                expected_turn_id: None,
            },
        };
        let execution = match send_recorded(
            repository,
            &send_native,
            &command,
            "turn/start",
            json!({
                "threadId": thread_id,
                "input": queue_item.input,
                "clientUserMessageId": queue_item.client_user_message_id,
            }),
            UserInputDisposition::Started,
        ) {
            Ok(execution) => execution,
            // Not idle after all (see `is_native_thread_busy`): the next
            // turn/completed flushes again.
            Err(error) if is_native_thread_busy(&error) => return Ok(changed),
            Err(error) => {
                append_event(
                    cutex_session_id,
                    EventCorrelation {
                        thread_id: Some(command.thread_id.clone()),
                        client_user_message_id: Some(command.params.client_user_message_id.clone()),
                        management_request_id: Some(management_request_id.clone()),
                        queue_id: Some(queue_item.queue_id.clone()),
                        ..Default::default()
                    },
                    "cutex/userInput/failed",
                    json!({
                        "managementRequestId": management_request_id,
                        "clientUserMessageId": command.params.client_user_message_id,
                        "origin": command.params.origin,
                        "input": command.params.input,
                        "stage": error.stage,
                        "error": {
                            "source": "cutex",
                            "code": error.code,
                            "message": error.message,
                            "retryable": error.retryable,
                            "details": error.details,
                        },
                        "failedAt": Utc::now().to_rfc3339(),
                    }),
                )?;
                anyhow::bail!("queued native user input failed: {}", error.message);
            }
        };
        append_event(
            cutex_session_id,
            EventCorrelation {
                thread_id: Some(command.thread_id.clone()),
                turn_id: execution.turn_id.clone(),
                client_user_message_id: Some(command.params.client_user_message_id.clone()),
                management_request_id: Some(management_request_id.clone()),
                queue_id: Some(queue_item.queue_id.clone()),
                native_request_id: execution.native_request_id.clone(),
                ..Default::default()
            },
            "cutex/userInput/submitted",
            json!({
                "managementRequestId": management_request_id,
                "clientUserMessageId": command.params.client_user_message_id,
                "origin": command.params.origin,
                "input": command.params.input,
                "disposition": execution.disposition,
                "nativeRequestId": execution.native_request_id,
                "nativeMethod": execution.native_method,
                "turnId": execution.turn_id,
                "appServerAccepted": true,
                "submittedAt": Utc::now().to_rfc3339(),
            }),
        )?;
        let removed = repository
            .remove(cutex_session_id, &queue_item.queue_id, queue_item.revision)?
            .context("queued user input disappeared after native submission")?;
        append_queue_removed_event(
            &append_event,
            cutex_session_id,
            &command.management_request_id,
            &removed,
            "submitted",
            repository.list(cutex_session_id)?.0,
        )?;
        return Ok(true);
    }
}

/// `cutex/turn/stop` backend: interrupt the session's active native turn and
/// wait until it has stopped. The management layer records the queue pause
/// *before* calling this, so the interrupted turn's `turn/completed` / idle
/// events (which run `flush_queued_if_idle`) already see the pause. The
/// session input lock is held throughout, so no submit or flush can start a
/// turn between the interrupt and its completion. Returns the interrupted
/// turn id, or `None` when no turn was running.
pub(crate) fn interrupt_active_turn_v2(
    cutex_session_id: &str,
    thread_id: &str,
) -> Result<Option<String>, UserInputExecutionError> {
    let session_lock = session_input_lock(cutex_session_id)
        .map_err(|error| execution_error("route", "internal_error", error, true, false))?;
    let _session_guard = session_lock.lock().map_err(|_| {
        execution_error_message(
            "route",
            "internal_error",
            "app-server session input lock was poisoned",
            true,
            false,
        )
    })?;
    // Newest turn only (thread/turns/list), not the full thread snapshot.
    let active_turn_id = super::active_turn::active_turn_id(cutex_session_id, thread_id)
        .map_err(|error| {
            execution_error("native_request", "app_server_unavailable", error, true, false)
        })?;
    let Some(active_turn_id) = active_turn_id else {
        return Ok(None);
    };
    session_manager(cutex_session_id)
        .commands(cutex_session_id)
        .and_then(|commands| {
            commands
                .turn_interrupt(&TurnInterruptParams {
                    thread_id: thread_id.to_string(),
                    turn_id: active_turn_id.clone(),
                })
                .map(|_| ())
                .map_err(Into::into)
        })
        .map_err(|error| execution_error("interrupt", "app_server_unavailable", error, true, true))?;
    wait_for_turn_to_stop(cutex_session_id, &active_turn_id)
        .map_err(|error| execution_error("interrupt", "interrupt_timeout", error, true, true))?;
    Ok(Some(active_turn_id))
}

fn append_queue_removed_event(
    append_event: &impl Fn(&str, EventCorrelation, &str, Value) -> anyhow::Result<()>,
    cutex_session_id: &str,
    management_request_id: &str,
    removed: &cutex::management::v2::user_input::UserInputQueueItem,
    reason: &str,
    queue_revision: u64,
) -> anyhow::Result<()> {
    append_event(
        cutex_session_id,
        EventCorrelation {
            client_user_message_id: Some(removed.client_user_message_id.clone()),
            management_request_id: Some(management_request_id.to_string()),
            queue_id: Some(removed.queue_id.clone()),
            ..Default::default()
        },
        "cutex/userInput/queueRemoved",
        json!({
            "queueId": removed.queue_id,
            "clientUserMessageId": removed.client_user_message_id,
            "revision": removed.revision,
            "queueRevision": queue_revision,
            "reason": reason,
            "removedAt": Utc::now().to_rfc3339(),
        }),
    )
}

fn append_input_event(
    cutex_session_id: &str,
    correlation: EventCorrelation,
    method: &str,
    params: Value,
) -> anyhow::Result<()> {
    management_v2_repository()?.append(PendingEvent {
        cutex_session_id: cutex_session_id.to_string(),
        host_id: current_host_name(),
        source: EventSource::Cutex,
        schema: None,
        correlation,
        native: None,
        cutex: Some(CutexMessage {
            method: method.to_string(),
            params,
        }),
    })?;
    Ok(())
}

fn read_thread(cutex_session_id: &str, thread_id: &str) -> anyhow::Result<Value> {
    let started = Instant::now();
    let result: anyhow::Result<Value> = (|| {
        Ok(session_manager(cutex_session_id)
            .commands(cutex_session_id)?
            .thread_read(&ThreadReadParams {
                thread_id: thread_id.to_string(),
                include_turns: true,
            })?)
    })();
    if started.elapsed() >= Duration::from_secs(1) {
        let turns = result.as_ref().ok()
            .and_then(|value| value.pointer("/thread/turns"))
            .and_then(Value::as_array).map_or(0, Vec::len);
        // No input, response body, or error text; do not serialize the large
        // response again merely to measure its size.
        eprintln!(
            "cutex-input-history session={cutex_session_id} elapsed_ms={} success={} turns={turns}",
            started.elapsed().as_millis(), result.is_ok(),
        );
    }
    result
}

fn active_turn_id_from_thread(response: &Value) -> Option<String> {
    response
        .pointer("/thread/turns")
        .and_then(Value::as_array)
        .and_then(|turns| {
            turns
                .iter()
                .rev()
                .find(|turn| turn.get("status").and_then(Value::as_str) == Some("inProgress"))
        })
        .and_then(|turn| turn.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn thread_lists_turn(response: &Value, turn_id: &str) -> bool {
    response
        .pointer("/thread/turns")
        .and_then(Value::as_array)
        .is_some_and(|turns| {
            turns
                .iter()
                .any(|turn| turn.get("id").and_then(Value::as_str) == Some(turn_id))
        })
}

fn client_message_turn_id(response: &Value, client_message_id: &str) -> Option<String> {
    response
        .pointer("/thread/turns")
        .and_then(Value::as_array)?
        .iter()
        .find(|turn| {
            turn.get("items")
                .and_then(Value::as_array)
                .is_some_and(|items| {
                    items.iter().any(|item| {
                        item.get("type").and_then(Value::as_str) == Some("userMessage")
                            && item.get("clientId").and_then(Value::as_str)
                                == Some(client_message_id)
                    })
                })
        })
        .and_then(|turn| turn.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn wait_for_turn_to_stop(cutex_session_id: &str, turn_id: &str) -> anyhow::Result<()> {
    let manager = session_manager(cutex_session_id);
    let deadline = Instant::now() + INTERRUPT_WAIT_TIMEOUT;
    loop {
        let active_turn_id = manager
            .status(cutex_session_id)?
            .and_then(|status| status.active_turn_id);
        if active_turn_id.as_deref() != Some(turn_id) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for interrupted turn {turn_id} to stop");
        }
        std::thread::sleep(INTERRUPT_POLL_INTERVAL);
    }
}

/// Claude sessions are served by their own adapter runtime manager; every
/// other app-server session by the Codex runtime manager.
pub(crate) fn is_claude_session(cutex_session_id: &str) -> bool {
    cutex::session::store::with_cutex_session_store(|store| {
        store
            .sessions
            .get(cutex_session_id)
            .is_some_and(|record| record.claude_session.is_some())
    })
    .unwrap_or(false)
}

pub(crate) fn session_manager(
    cutex_session_id: &str,
) -> &'static cutex::app_server::manager::AppServerRuntimeManager {
    if is_claude_session(cutex_session_id) {
        super::claude_runtime::manager()
    } else {
        app_server_runtime::runtime_manager()
    }
}

/// All leases block starts; only Ask leases block same-turn owner input.
/// An unbound or stale Ask turn id stays conservative (asks §5.3).
fn admission_lease_blocks_owner_input(cutex_session_id: &str, active_turn_id: Option<&str>) -> bool {
    cutex::asks::store::AskStore::open_default()
        .ok()
        .and_then(|store| store.peek().ok().flatten())
        .and_then(|state| state.leases.get(cutex_session_id).cloned())
        .is_some_and(|lease| {
            active_turn_id.is_none()
                || matches!(lease.holder, cutex::asks::model::LeaseHolder::Ask { .. })
        })
}

/// Whether an ask holds this session's admission lease (no store: no).
fn ask_lease_held(cutex_session_id: &str) -> bool {
    cutex::asks::store::AskStore::open_default()
        .ok()
        .and_then(|store| store.peek().ok().flatten())
        .is_some_and(|state| state.leases.contains_key(cutex_session_id))
}

pub(crate) fn session_input_lock(cutex_session_id: &str) -> anyhow::Result<std::sync::Arc<Mutex<()>>> {
    cutex::management::v2::user_input::session_input_lock(cutex_session_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cutex::management::v2::user_input::UserInputOrigin;
    use cutex::management::v2::user_input::UserInputOriginKind;
    use cutex::management::v2::user_input::UserInputRepository;

    #[test]
    fn claude_gate_refusals_are_recognized_for_owner_input_only() {
        assert!(is_claude_gate_refusal(&json!({"code": -32009,
            "message": "queue paused without a terminal failed/interrupted turn; inspect the runtime"})));
        assert!(is_claude_gate_refusal(&json!({"code": -32009, "message": "interruption gate is closed; resume before retry"})));
        assert!(!is_claude_gate_refusal(&json!({"code": -32009, "message": "thread busy; use claude/input/submit",
            "data": {"reason": "threadBusy"}})));
        assert!(!is_claude_gate_refusal(&json!({"code": -32011, "message": "queue paused"})));
        let mut command = queue_command("cutex.s", "c");
        assert!(owner_reopens_gate(&command));
        command.params.origin.kind = UserInputOriginKind::AgentBus;
        assert!(!owner_reopens_gate(&command));
    }

    #[test]
    fn native_thread_history_correlates_user_client_message_id() {
        let thread = serde_json::json!({
            "thread": {
                "turns": [
                    {
                        "id": "turn-1",
                        "status": "completed",
                        "items": [
                            {
                                "type": "userMessage",
                                "id": "item-1",
                                "clientId": "message-1",
                                "content": [{"type": "text", "text": "hello"}]
                            }
                        ]
                    },
                    {
                        "id": "turn-2",
                        "status": "inProgress",
                        "items": []
                    }
                ]
            }
        });

        assert_eq!(
            client_message_turn_id(&thread, "message-1").as_deref(),
            Some("turn-1")
        );
        assert_eq!(
            active_turn_id_from_thread(&thread).as_deref(),
            Some("turn-2")
        );
        assert!(client_message_turn_id(&thread, "missing").is_none());
    }

    #[test]
    fn explicit_queue_is_durable_while_app_server_is_offline() {
        let root = std::env::temp_dir().join(format!(
            "cutex-v2-offline-user-input-queue-{}",
            uuid::Uuid::new_v4()
        ));
        let repository = UserInputRepository::open(&root).expect("open user-input repository");
        let command = UserInputSubmitCommand {
            management_request_id: "management-offline-queue-1".to_string(),
            cutex_session_id: "cutex.offline-queue".to_string(),
            thread_id: "thread-offline-queue".to_string(),
            params: cutex::management::v2::user_input::UserInputSubmitParams {
                client_user_message_id: "client-offline-queue-1".to_string(),
                origin: UserInputOrigin {
                    kind: UserInputOriginKind::Android,
                    client_id: "integration-phone".to_string(),
                },
                strategy: UserInputStrategy::Queue,
                input: vec![json!({
                    "type": "text",
                    "text": "queue while offline",
                    "text_elements": []
                })],
                expected_turn_id: None,
            },
        };

        let execution = submit_v2_with_repository(
            command,
            &repository,
            |_, _| None,
            |_, _| anyhow::bail!("app-server is offline"),
            |_, _, _, _| panic!("an explicit queue never reaches the native runtime"),
            no_flush,
        )
        .expect("explicit queue must not require app-server state");

        assert_eq!(execution.disposition, UserInputDisposition::Queued);
        assert!(!execution.app_server_accepted);
        assert_eq!(execution.turn_id, None);
        assert!(execution.queue.is_some());
        let (revision, items) = repository
            .list("cutex.offline-queue")
            .expect("read durable queue");
        assert_eq!(revision, 1);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].client_user_message_id, "client-offline-queue-1");
        std::fs::remove_dir_all(root).expect("remove user-input repository");
    }

    #[test]
    fn busy_claude_adapter_keeps_the_input_in_the_management_queue() {
        let root = std::env::temp_dir().join(format!(
            "cutex-v2-busy-user-input-queue-{}",
            uuid::Uuid::new_v4()
        ));
        let repository = UserInputRepository::open(&root).expect("open user-input repository");
        let command = |client: &str| UserInputSubmitCommand {
            management_request_id: format!("management-{client}"),
            cutex_session_id: "cutex.busy-claude".to_string(),
            thread_id: "thread-busy".to_string(),
            params: cutex::management::v2::user_input::UserInputSubmitParams {
                client_user_message_id: client.to_string(),
                origin: UserInputOrigin {
                    kind: UserInputOriginKind::Other,
                    client_id: "desktop".to_string(),
                },
                strategy: UserInputStrategy::Auto,
                input: vec![json!({"type": "text", "text": "hello", "text_elements": []})],
                expected_turn_id: None,
            },
        };
        let idle = |_: &str, _: &str| Ok(json!({"thread": {"id": "thread-busy", "turns": []}}));
        let native_error = |reason: &str| UserInputExecutionError {
            stage: "native_request".to_string(),
            code: "native_request_failed".to_string(),
            message: "thread busy".to_string(),
            retryable: false,
            details: json!({"nativeError": {"code": -32009, "message": "thread busy", "data": {"reason": reason}}}),
            outcome_unknown: false,
        };
        let queued = submit_v2_with_repository(command("busy-1"), &repository, |_, _| None, idle, |_, method, _, _| {
            assert_eq!(method, "turn/start");
            Err(native_error("threadBusy"))
        }, no_flush)
        .expect("a busy Claude adapter queues instead of failing");
        assert_eq!(queued.disposition, UserInputDisposition::Queued);
        assert_eq!(repository.list("cutex.busy-claude").unwrap().1.len(), 1);
        // A new idle auto input follows existing admission instead of passing it.
        let behind = submit_v2_with_repository(command("busy-2"), &repository, |_, _| None, idle, never_sent, no_flush).unwrap();
        assert_eq!(behind.disposition, UserInputDisposition::Queued);
        for item in repository.list("cutex.busy-claude").unwrap().1 {
            repository.remove("cutex.busy-claude", &item.queue_id, item.revision).unwrap();
        }
        // Any other native refusal still fails the submit.
        let failed = submit_v2_with_repository(command("busy-3"), &repository, |_, _| None, idle, |_, _, _, _| {
            Err(native_error("somethingElse"))
        }, no_flush)
        .expect_err("other native errors are not queued");
        assert_eq!(failed.code, "native_request_failed");
        assert_eq!(repository.list("cutex.busy-claude").unwrap().1.len(), 0);
        std::fs::remove_dir_all(root).expect("remove user-input repository");
    }

    #[test]
    fn new_client_message_routes_from_tracked_turn_without_reading_history() {
        let root = std::env::temp_dir().join(format!(
            "cutex-v2-fast-submit-{}",
            uuid::Uuid::new_v4()
        ));
        let repository = UserInputRepository::open(&root).expect("open user-input repository");
        let command = |client: &str| UserInputSubmitCommand {
            management_request_id: format!("management-{client}"),
            cutex_session_id: "cutex.fast-submit".to_string(),
            thread_id: "thread-fast".to_string(),
            params: cutex::management::v2::user_input::UserInputSubmitParams {
                client_user_message_id: client.to_string(),
                origin: UserInputOrigin {
                    kind: UserInputOriginKind::Other,
                    client_id: "desktop".to_string(),
                },
                strategy: UserInputStrategy::Auto,
                input: vec![json!({"type": "text", "text": "hello", "text_elements": []})],
                expected_turn_id: None,
            },
        };
        let ok = |method: &str, disposition| UserInputSubmitExecution {
            disposition,
            app_server_accepted: true,
            native_request_id: None,
            native_method: Some(method.to_string()),
            turn_id: Some("turn-x".to_string()),
            queue: None,
            queue_paused: false,
        };
        let no_history = |_: &str, _: &str| -> anyhow::Result<Value> {
            panic!("a new clientUserMessageId must not read the native history")
        };
        // Idle runtime: turn/start without a thread read.
        let started = submit_v2_with_repository(
            command("fast-1"),
            &repository,
            |_, _| Some(None),
            no_history,
            |_, method, _, disposition| Ok(ok(method, disposition)),
            no_flush,
        )
        .expect("fast start");
        assert_eq!(started.disposition, UserInputDisposition::Started);
        // Running turn: steer with the tracked turn id.
        let steered = submit_v2_with_repository(
            command("fast-2"),
            &repository,
            |_, _| Some(Some("turn-live".to_string())),
            no_history,
            |_, method, params, disposition| {
                assert_eq!(method, "turn/steer");
                assert_eq!(params["expectedTurnId"], "turn-live");
                Ok(ok(method, disposition))
            },
            no_flush,
        )
        .expect("fast steer");
        assert_eq!(steered.disposition, UserInputDisposition::Steered);
        // Stale tracked turn: the refused steer refreshes active-turn state.
        let reads = std::cell::Cell::new(0);
        let fallback = submit_v2_with_repository(
            command("fast-3"),
            &repository,
            |_, _| Some(Some("turn-gone".to_string())),
            |_, _| {
                reads.set(reads.get() + 1);
                Ok(json!({"thread": {"id": "thread-fast", "turns": []}}))
            },
            |_, method, _, disposition| {
                if method == "turn/steer" {
                    return Err(UserInputExecutionError {
                        stage: "native_request".to_string(),
                        code: "native_request_failed".to_string(),
                        message: "no active turn".to_string(),
                        retryable: false,
                        details: json!({}),
                        outcome_unknown: false,
                    });
                }
                Ok(ok(method, disposition))
            },
            no_flush,
        )
        .expect("fallback start");
        assert_eq!(fallback.disposition, UserInputDisposition::Started);
        assert_eq!(reads.get(), 1);
        // A repeated clientUserMessageId always consults the history (dedupe).
        let deduplicated = submit_v2_with_repository(
            command("fast-1"),
            &repository,
            |_, _| Some(None),
            |_, _| {
                Ok(json!({"thread": {"id": "thread-fast", "turns": [{
                    "id": "turn-1",
                    "status": "completed",
                    "items": [{"type": "userMessage", "id": "i", "clientId": "fast-1", "content": []}]
                }]}}))
            },
            |_, _, _, _| panic!("a deduplicated submit sends nothing"),
            no_flush,
        )
        .expect("dedupe");
        assert_eq!(deduplicated.disposition, UserInputDisposition::Deduplicated);
        assert_eq!(deduplicated.turn_id.as_deref(), Some("turn-1"));
        std::fs::remove_dir_all(root).expect("remove user-input repository");
    }

    #[test]
    fn missing_tracking_routes_new_input_with_only_a_turn_summary() {
        for active in [None, Some("turn-live".to_string())] {
            let (root, repository) = queue_repository("summary-new");
            let reads = std::cell::Cell::new(0);
            let result = super::submit_v2_with_repository(
                retry_command("cutex.summary-new", "request-1"), &repository,
                |_, _| None,
                |_, _| { reads.set(reads.get() + 1); Ok(active.clone()) },
                no_history,
                |command, method, params, disposition| {
                    assert_eq!(method, if active.is_some() { "turn/steer" } else { "turn/start" });
                    assert_eq!(params.get("expectedTurnId").and_then(Value::as_str), active.as_deref());
                    accepted("turn-result")(command, method, params, disposition)
                }, no_flush,
            ).unwrap();
            assert!(result.app_server_accepted);
            assert_eq!(reads.get(), 1);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn tracked_turn_refusal_refreshes_summary_but_unknown_outcome_never_retries() {
        for unknown in [false, true] {
            let (root, repository) = queue_repository("summary-refusal");
            let reads = std::cell::Cell::new(0);
            let sends = std::cell::Cell::new(0);
            let result = super::submit_v2_with_repository(
                retry_command("cutex.summary-refusal", "request-1"), &repository,
                |_, _| Some(Some("turn-stale".into())),
                |_, _| { reads.set(reads.get() + 1); Ok(None) },
                no_history,
                |command, method, params, disposition| {
                    sends.set(sends.get() + 1);
                    if sends.get() == 1 {
                        assert_eq!(method, "turn/steer");
                        assert_eq!(params["expectedTurnId"], "turn-stale");
                        return Err(UserInputExecutionError {
                            code: "native_request_failed".into(),
                            outcome_unknown: unknown,
                            ..outcome_unknown_error()
                        });
                    }
                    assert_eq!(method, "turn/start");
                    accepted("turn-fresh")(command, method, params, disposition)
                }, no_flush,
            );
            if unknown {
                assert!(result.unwrap_err().outcome_unknown);
                assert_eq!((reads.get(), sends.get()), (0, 1));
                // A repeated ID must still check history and respect the
                // durable unknown reservation, even if tracking says idle.
                let retry = super::submit_v2_with_repository(
                    retry_command("cutex.summary-refusal", "request-2"), &repository,
                    |_, _| Some(None),
                    |_, _| panic!("C-010 requires history for an existing identity"),
                    thread_with(json!([])), never_sent, no_flush,
                ).unwrap_err();
                assert_eq!(retry.code, "user_input_in_progress");
            } else {
                assert_eq!(result.unwrap().disposition, UserInputDisposition::Started);
                assert_eq!((reads.get(), sends.get()), (1, 2));
            }
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn fresh_summary_preserves_expected_turn_conflicts_and_offline_queue() {
        for queue in [false, true] {
            let (root, repository) = queue_repository("summary-guards");
            let mut command = retry_command("cutex.summary-guards", "request-1");
            if queue {
                command.params.strategy = UserInputStrategy::Queue;
            } else {
                command.params.expected_turn_id = Some("turn-expected".into());
            }
            let result = super::submit_v2_with_repository(
                command, &repository,
                |_, _| None,
                |_, _| if queue { anyhow::bail!("offline") } else { Ok(None) },
                no_history, never_sent, no_flush,
            );
            if queue {
                assert_eq!(result.unwrap().disposition, UserInputDisposition::Queued);
            } else {
                assert_eq!(result.unwrap_err().code, "turn_conflict");
            }
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    fn submit_v2_with_repository(
        command: UserInputSubmitCommand,
        repository: &UserInputRepository,
        tracked_active_turn: impl Fn(&str, &str) -> Option<Option<String>>,
        read_native_thread: impl Fn(&str, &str) -> anyhow::Result<Value>,
        send_native_raw: impl Fn(&UserInputSubmitCommand, &str, Value, UserInputDisposition)
            -> Result<UserInputSubmitExecution, UserInputExecutionError>,
        flush_ahead: impl Fn(&str, Option<&str>) -> anyhow::Result<bool>,
    ) -> Result<UserInputSubmitExecution, UserInputExecutionError> {
        super::submit_v2_with_repository(
            command, repository, tracked_active_turn,
            |session, thread| read_native_thread(session, thread)
                .map(|snapshot| active_turn_id_from_thread(&snapshot)),
            &read_native_thread, send_native_raw, flush_ahead,
        )
    }

    fn no_flush(_: &str, _: Option<&str>) -> anyhow::Result<bool> {
        Ok(false)
    }

    fn test_runs_in_private_home(name: &str) -> bool {
        const CHILD: &str = "CUTEX_OWNER_INPUT_TEST_CHILD";
        if std::env::var(CHILD).ok().as_deref() == Some(name) { return false; }
        let parent = std::env::var_os("CUTEX_TEST_PRIVATE_HOME").map(std::path::PathBuf::from)
            .expect("owner input tests require the private test home");
        let root = parent.join(format!("owner-input-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".cutex-test-private-home"), b"owner input isolated test\n").unwrap();
        let test = format!("cli_app::app_server_user_input::tests::{name}");
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &test, "--nocapture"])
            .env("HOME", &root).env("CUTEX_TEST_PRIVATE_HOME", &root).env(CHILD, name)
            .status().unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(status.success(), "isolated owner input test failed: {name}");
        true
    }

    fn queue_repository(name: &str) -> (std::path::PathBuf, UserInputRepository) {
        let root = std::env::temp_dir().join(format!("cutex-v2-{name}-{}", uuid::Uuid::new_v4()));
        let repository = UserInputRepository::open(&root).expect("open user-input repository");
        (root, repository)
    }

    fn queue_command(session: &str, client: &str) -> UserInputSubmitCommand {
        UserInputSubmitCommand {
            management_request_id: format!("management-{client}"),
            cutex_session_id: session.to_string(),
            thread_id: "thread-q".to_string(),
            params: cutex::management::v2::user_input::UserInputSubmitParams {
                client_user_message_id: client.to_string(),
                origin: UserInputOrigin {
                    kind: UserInputOriginKind::Other,
                    client_id: "desktop".to_string(),
                },
                strategy: UserInputStrategy::Queue,
                input: vec![json!({"type": "text", "text": client, "text_elements": []})],
                expected_turn_id: None,
            },
        }
    }

    fn idle_thread(_: &str, _: &str) -> anyhow::Result<Value> {
        Ok(json!({"thread": {"id": "thread-q", "turns": []}}))
    }

    fn no_history(_: &str, _: &str) -> anyhow::Result<Value> {
        panic!("a new clientUserMessageId on a tracked thread reads no history")
    }

    fn never_sent(
        _: &UserInputSubmitCommand,
        method: &str,
        _: Value,
        _: UserInputDisposition,
    ) -> Result<UserInputSubmitExecution, UserInputExecutionError> {
        panic!("nothing may reach the native runtime ({method})")
    }

    /// Records each native request as `method clientUserMessageId`.
    fn recording_sender(
        sent: &std::cell::RefCell<Vec<String>>,
    ) -> impl Fn(
        &UserInputSubmitCommand,
        &str,
        Value,
        UserInputDisposition,
    ) -> Result<UserInputSubmitExecution, UserInputExecutionError>
           + '_ {
        move |command, method, params, disposition| {
            assert_eq!(params["clientUserMessageId"], json!(command.params.client_user_message_id));
            sent.borrow_mut()
                .push(format!("{method} {}", command.params.client_user_message_id));
            Ok(UserInputSubmitExecution {
                disposition,
                app_server_accepted: true,
                native_request_id: None,
                native_method: Some(method.to_string()),
                turn_id: Some("turn-new".to_string()),
                queue: None,
                queue_paused: false,
            })
        }
    }

    fn recording_events(
        events: &std::cell::RefCell<Vec<String>>,
    ) -> impl Fn(&str, EventCorrelation, &str, Value) -> anyhow::Result<()> + '_ {
        move |_, _, method, _| {
            events.borrow_mut().push(method.to_string());
            Ok(())
        }
    }

    fn clients(repository: &UserInputRepository, session: &str) -> Vec<String> {
        repository
            .list(session)
            .expect("list queue")
            .1
            .into_iter()
            .map(|item| item.client_user_message_id)
            .collect()
    }

    #[test]
    fn queue_on_an_idle_unpaused_agent_starts_at_once() {
        let (root, repository) = queue_repository("queue-idle-start");
        let session = "cutex.queue-idle-start";
        let sent = std::cell::RefCell::new(Vec::new());
        // Runtime tracks the thread as idle.
        let started = submit_v2_with_repository(
            queue_command(session, "idle-1"),
            &repository,
            |_, _| Some(None),
            no_history,
            recording_sender(&sent),
            no_flush,
        )
        .expect("queue on idle");
        assert_eq!(started.disposition, UserInputDisposition::Started);
        assert!(started.queue.is_none());
        // Not tracked, but the thread read shows no running turn.
        let read = submit_v2_with_repository(
            queue_command(session, "idle-2"),
            &repository,
            |_, _| None,
            idle_thread,
            recording_sender(&sent),
            no_flush,
        )
        .expect("queue on idle after a thread read");
        assert_eq!(read.disposition, UserInputDisposition::Started);
        assert_eq!(*sent.borrow(), ["turn/start idle-1", "turn/start idle-2"]);
        assert!(clients(&repository, session).is_empty());
        std::fs::remove_dir_all(root).expect("remove user-input repository");
    }

    #[test]
    fn queue_steer_routes_original_identity_and_records_delivery() {
        let (root, repository) = queue_repository("queue-steer-routes");
        for active in [Some("turn-running".to_string()), None] {
            let client = if active.is_some() {
                "busy"
            } else {
                "idle-claude"
            };
            let command = queue_command("queue-steer-routes", client);
            repository
                .enqueue(
                    &command.cutex_session_id,
                    &command.thread_id,
                    &command.management_request_id,
                    &command.params,
                )
                .unwrap();
            let execution = steer_queued_with_repository(
                &command,
                true,
                &repository,
                active.clone(),
                false,
                || {
                    assert!(active.is_some(), "idle start never checks steer capability");
                    true
                },
                |sent, method, params, disposition| {
                    assert_eq!(sent.params.origin, command.params.origin);
                    assert_eq!(params["input"], json!(command.params.input));
                    assert_eq!(params["clientUserMessageId"], client);
                    if let Some(active) = &active {
                        assert_eq!(method, "turn/steer");
                        assert_eq!(params["expectedTurnId"], *active);
                        assert_eq!(disposition, UserInputDisposition::Steered);
                    } else {
                        assert_eq!(method, "turn/start");
                        assert!(params.get("expectedTurnId").is_none());
                        assert_eq!(disposition, UserInputDisposition::Started);
                    }
                    accepted("turn-running")(sent, method, params, disposition)
                },
            )
            .unwrap();
            assert!(execution.app_server_accepted);
            assert!(matches!(
                repository
                    .delivery(&command.cutex_session_id, client)
                    .unwrap(),
                Some(InputDelivery::Accepted { .. })
            ));
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn queue_steer_refuses_no_capability_delegation_idle_fail_and_stale_turn() {
        let (root, repository) = queue_repository("queue-steer-refusals");
        let mut command = queue_command("queue-steer-refusals", "queued");
        repository
            .enqueue(
                &command.cutex_session_id,
                &command.thread_id,
                &command.management_request_id,
                &command.params,
            )
            .unwrap();
        let before = repository.list(&command.cutex_session_id).unwrap();
        for (active, lease, start, reason) in [
            (Some("turn".to_string()), false, true, "runtimeNoSteer"),
            (Some("turn".to_string()), true, true, "delegationTurn"),
            (None, true, true, "delegationTurn"),
            (None, false, false, "noActiveTurn"),
        ] {
            let error = steer_queued_with_repository(
                &command,
                start,
                &repository,
                active,
                lease,
                || false,
                never_sent,
            )
            .unwrap_err();
            assert_eq!(error.code, "steer_unavailable");
            assert_eq!(error.details["reason"], reason);
            assert_eq!(repository.list(&command.cutex_session_id).unwrap(), before);
        }
        command.params.expected_turn_id = Some("old".into());
        let error = steer_queued_with_repository(
            &command,
            true,
            &repository,
            Some("new".into()),
            false,
            || true,
            never_sent,
        )
        .unwrap_err();
        assert_eq!(error.code, "turn_conflict");
        assert_eq!(error.details["activeTurnId"], "new");
        assert!(repository
            .delivery(&command.cutex_session_id, "queued")
            .unwrap()
            .is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn owner_input_routes_by_lease_holder_and_active_turn() {
        if test_runs_in_private_home("owner_input_routes_by_lease_holder_and_active_turn") { return; }
        use cutex::asks::model::{AdmissionLease, LeaseHolder};

        let store = cutex::asks::store::AskStore::open_default().unwrap();
        let (root, repository) = queue_repository("steer-lease");
        for holder in [
            LeaseHolder::Ask {
                ask_id: "ask".into(),
                attempt: 1,
            },
            LeaseHolder::Event {
                event_id: "event".into(),
            },
            LeaseHolder::Owner,
        ] {
            for bound_turn in [None, Some("active"), Some("old")] {
                for active in [None, Some("active")] {
                    let session = format!("steer-lease-{}", uuid::Uuid::new_v4());
                    store
                        .with_state(|state| {
                            state.leases.insert(
                                session.clone(),
                                AdmissionLease {
                                    cutex_session_id: session.clone(),
                                    holder: holder.clone(),
                                    native_message_id: "native".into(),
                                    runtime_generation: 1,
                                    turn_id: bound_turn.map(str::to_string),
                                    since: Utc::now().to_rfc3339(),
                                },
                            );
                        })
                        .unwrap();
                    let blocked = active.is_none() || matches!(holder, LeaseHolder::Ask { .. });
                    let command = queue_command(&session, "queued");
                    repository
                        .enqueue(
                            &session,
                            &command.thread_id,
                            &command.management_request_id,
                            &command.params,
                        )
                        .unwrap();
                    let before = repository.list(&session).unwrap();
                    let sent = std::cell::RefCell::new(Vec::new());
                    let result = steer_queued_with_repository(
                        &command,
                        true,
                        &repository,
                        active.map(str::to_string),
                        admission_lease_blocks_owner_input(&session, active),
                        || true,
                        recording_sender(&sent),
                    );
                    if blocked {
                        let error = result.unwrap_err();
                        assert_eq!(error.code, "steer_unavailable");
                        assert_eq!(error.details["reason"], "delegationTurn");
                        assert!(sent.borrow().is_empty());
                        assert_eq!(repository.list(&session).unwrap(), before);
                        assert!(repository.delivery(&session, "queued").unwrap().is_none());
                    } else {
                        assert_eq!(result.unwrap().disposition, UserInputDisposition::Steered);
                        assert_eq!(*sent.borrow(), ["turn/steer queued"]);
                    }
                    sent.borrow_mut().clear();
                    let mut auto = queue_command(&session, "auto");
                    auto.params.strategy = UserInputStrategy::Auto;
                    let result = route_submit(
                        &auto,
                        &repository,
                        active.map(str::to_string),
                        true,
                        &recording_sender(&sent),
                        &no_flush,
                    )
                    .unwrap();
                    if blocked {
                        assert_eq!(result.disposition, UserInputDisposition::Queued);
                        assert!(clients(&repository, &session).contains(&"auto".to_string()));
                        assert!(sent.borrow().is_empty());
                    } else {
                        assert_eq!(result.disposition, UserInputDisposition::Steered);
                        assert_eq!(*sent.borrow(), ["turn/steer auto"]);
                    }
                    // Queue/flush scheduling still treats every holder as occupied.
                    assert!(ask_lease_held(&session));
                    store
                        .with_state(|state| {
                            state.leases.remove(&session);
                        })
                        .unwrap();
                }
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn steer_failures_map_to_the_queue_contract() {
        let native = |code: i64, message: &str, data: Value| {
            native_user_input_failure(&json!({"code": code, "message": message, "data": data}))
        };
        let mapped = |method: &str, error| steer_failure(method, error);
        // The turn moved on: retry.
        for message in ["no active turn to steer", "expected active turn id `a` but found `b`"] {
            let error = mapped("turn/steer", native(-32600, message, Value::Null));
            assert_eq!((error.code.as_str(), error.retryable), ("turn_conflict", true), "{message}");
        }
        // A runtime without steer.
        let error = mapped("turn/steer", native(-32601, "Claude cannot add input", json!({"unsupported": true})));
        assert_eq!((error.code.as_str(), error.details["reason"].as_str()), ("steer_unavailable", Some("runtimeNoSteer")));
        assert_eq!(error.details["nativeError"]["code"], -32601);
        // Other refusals (review / compact turn, draining, guarded phase).
        let error = mapped("turn/steer", native(-32600, "cannot steer a review turn", json!({"codexErrorInfo": {}})));
        assert_eq!((error.code.as_str(), error.details["reason"].as_str()), ("steer_unavailable", Some("runtimeRefused")));
        assert_eq!(error.details["nativeError"]["message"], "cannot steer a review turn");
        // A refused idle start: a turn started meanwhile.
        let error = mapped("turn/start", native(-32600, "turn already active", Value::Null));
        assert_eq!(error.code, "turn_conflict");
        // The runtime went away mid-request.
        let error = mapped(
            "turn/steer",
            execution_error_message("native_request", "app_server_unavailable", "closed", true, false),
        );
        assert_eq!((error.code.as_str(), error.retryable), ("session_offline", true));
        // Unknown outcome stays unknown and is never retried.
        let error = mapped("turn/steer", native(-32603, "x", json!({"outcomeUnknown": true})));
        assert_eq!((error.code.as_str(), error.retryable), ("outcome_unknown", false));
    }

    #[test]
    fn queue_steer_known_failure_can_retry_but_unknown_never_auto_resends() {
        let (root, repository) = queue_repository("queue-steer-failures");
        let command = queue_command("queue-steer-failures", "queued");
        repository
            .enqueue(
                &command.cutex_session_id,
                &command.thread_id,
                &command.management_request_id,
                &command.params,
            )
            .unwrap();
        let before = repository.list(&command.cutex_session_id).unwrap();
        let error = steer_queued_with_repository(
            &command,
            true,
            &repository,
            Some("turn".into()),
            false,
            || true,
            |_, _, _, _| {
                Err(execution_error_message(
                    "native_request",
                    "native_request_failed",
                    "no active turn to steer",
                    false,
                    false,
                ))
            },
        )
        .unwrap_err();
        assert_eq!(error.code, "turn_conflict");
        assert!(error.retryable);
        assert!(repository
            .delivery(&command.cutex_session_id, "queued")
            .unwrap()
            .is_none());
        assert_eq!(repository.list(&command.cutex_session_id).unwrap(), before);
        let error = steer_queued_with_repository(
            &command,
            true,
            &repository,
            Some("turn".into()),
            false,
            || true,
            |_, _, _, _| Err(outcome_unknown_error()),
        )
        .unwrap_err();
        assert_eq!(error.code, "outcome_unknown");
        assert!(error.outcome_unknown);
        assert!(!error.retryable);
        let delivery = repository
            .delivery(&command.cutex_session_id, "queued")
            .unwrap()
            .unwrap();
        let old = InputDelivery::Unknown {
            thread_id: command.thread_id.clone(),
            since: (Utc::now() - chrono::Duration::days(1)).to_rfc3339(),
        };
        repository
            .set_delivery(
                &command.cutex_session_id,
                "queued",
                Some(&delivery),
                Some(old.clone()),
            )
            .unwrap();
        for ignore_pause in [false, true] {
            assert!(!flush_queued_locked(
                &command.cutex_session_id,
                ignore_pause,
                &repository,
                None,
                idle_thread,
                never_sent,
                |_, _, _, _| panic!("unresolved input emits no success")
            )
            .unwrap());
        }
        assert_eq!(
            repository
                .delivery(&command.cutex_session_id, "queued")
                .unwrap(),
            Some(old)
        );
        // Integration review: the unresolved state is visible in the list
        // (the owner decides); nothing else about the item changed.
        let mut unknown = before.clone();
        unknown.1[0].delivery = Some("unknown".into());
        assert_eq!(repository.list(&command.cutex_session_id).unwrap(), unknown);
        assert!(matches!(
            resolve_prior_delivery(
                &repository,
                &command.cutex_session_id,
                "queued",
                "new-binding",
                Some(&idle_thread("", "").unwrap()),
                Utc::now(),
                Duration::ZERO
            )
            .unwrap(),
            PriorDelivery::InProgress(_)
        ));
        // Once native history shows the input (the lost response did reach
        // native), flush removes the item as submitted and sends nothing;
        // it no longer holds the queue.
        let removed = std::cell::RefCell::new(Vec::new());
        assert!(flush_queued_locked(
            &command.cutex_session_id,
            false,
            &repository,
            None,
            |_, _| Ok(json!({"thread": {"id": "thread-q", "turns": [
                {"id": "turn-steered", "status": "completed",
                 "items": [{"type": "userMessage", "clientId": "queued"}]}
            ]}})),
            never_sent,
            |_, _, method, params| { removed.borrow_mut().push((method.to_string(), params)); Ok(()) }
        )
        .unwrap());
        assert!(repository.list(&command.cutex_session_id).unwrap().1.is_empty());
        assert_eq!(removed.borrow().len(), 1);
        assert_eq!(removed.borrow()[0].0, "cutex/userInput/queueRemoved");
        assert_eq!(removed.borrow()[0].1["reason"], "submitted");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn queue_move_changes_the_next_native_flush_and_removal_revision() {
        let (root, repository) = queue_repository("queue-move-flush");
        let session = "queue-move-flush";
        for client in ["a", "b", "c"] {
            let command = queue_command(session, client);
            repository
                .enqueue(
                    session,
                    &command.thread_id,
                    &command.management_request_id,
                    &command.params,
                )
                .unwrap();
        }
        let (_, items) = repository.list(session).unwrap();
        repository
            .move_admission_item(
                session,
                &items[2].queue_id,
                repository.admission(session).unwrap().order_revision,
                Some(&cutex::management::v2::admission::AdmissionRef { kind: "owner".into(), id: items[0].queue_id.clone() }),
                true,
            )
            .unwrap();
        let sent = std::cell::RefCell::new(Vec::new());
        assert!(flush_queued_locked(
            session,
            false,
            &repository,
            None,
            idle_thread,
            |command, method, params, disposition| {
                let mut execution = recording_sender(&sent)(command, method, params, disposition)?;
                execution.native_request_id = Some(json!("fixture-native-request"));
                Ok(execution)
            },
            |_, _, method, params| {
                cutex::management::v2::contract_validation::validate_cutex_event_message(
                    &json!({"method":method,"params":params}),
                )
                .unwrap();
                if method == "cutex/userInput/queueRemoved" {
                    assert_eq!(params["queueRevision"], 5);
                }
                Ok(())
            }
        )
        .unwrap());
        assert_eq!(*sent.borrow(), ["turn/start c"]);
        assert_eq!(clients(&repository, session), ["a", "b"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn admission_ask(repository: &UserInputRepository, session: &str) -> String {
        use cutex::asks::model::{AskBody, AskOptions, Requester};
        repository.ask_store().with_state(|state| {
            state.create_ask(cutex::asks::store::NewAsk {
                ask_id: None, action_id: "waiting-ask".into(), requester: Requester::Owner,
                target_cutex_session_id: session.into(), target_name: None, project_id: None,
                body: AskBody {
                    text: "waiting".into(), text_artifact: None, text_bytes: 7,
                    text_sha256: cutex::asks::sha256_hex(b"waiting"), attachments: Vec::new(),
                },
                title: None, options: AskOptions::default(), follow_up_of: None, parent_ask: None, gate: None,
            }, Utc::now()).unwrap().0.ask_id
        }).unwrap()
    }

    #[test]
    fn idle_cancelled_head_ask_triggers_changed_hook_and_owner_dispatch() {
        use cutex::asks::model::{CloseOutcome, ExecutionState};
        let (root, repository) = queue_repository("cancel-head-flush");
        let session = format!("cutex.cancel-head-{}", uuid::Uuid::new_v4());
        let ask = admission_ask(&repository, &session);
        let command = queue_command(&session, "waiting-owner");
        repository.enqueue(&session, &command.thread_id, &command.management_request_id, &command.params).unwrap();
        let sent = std::cell::RefCell::new(Vec::new());
        let events = std::cell::RefCell::new(Vec::new());
        assert!(!flush_queued_locked(&session, false, &repository, None, idle_thread,
            recording_sender(&sent), recording_events(&events)).unwrap());
        cutex::management::v2::admission::take_change(&session);
        repository.ask_store().with_state(|state| {
            state.update_ask(&ask, Utc::now(), |ask| {
                ask.execution = ExecutionState::Cancelled;
                ask.closed = Some(CloseOutcome::Cancelled);
            });
        }).unwrap();
        assert!(cutex::management::v2::admission::take_change(&session));
        assert!(flush_queued_locked(&session, false, &repository, None, idle_thread,
            recording_sender(&sent), recording_events(&events)).unwrap());
        assert_eq!(*sent.borrow(), ["turn/start waiting-owner"]);
        assert!(clients(&repository, &session).is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn idle_move_owner_to_head_triggers_changed_hook_and_dispatch() {
        let (root, repository) = queue_repository("move-head-flush");
        let session = format!("cutex.move-head-{}", uuid::Uuid::new_v4());
        let ask = admission_ask(&repository, &session);
        let command = queue_command(&session, "moved-owner");
        repository.enqueue(&session, &command.thread_id, &command.management_request_id, &command.params).unwrap();
        let sent = std::cell::RefCell::new(Vec::new());
        let events = std::cell::RefCell::new(Vec::new());
        assert!(!flush_queued_locked(&session, false, &repository, None, idle_thread,
            recording_sender(&sent), recording_events(&events)).unwrap());
        let owner = repository.list(&session).unwrap().1.remove(0);
        let admission = repository.admission(&session).unwrap();
        cutex::management::v2::admission::take_change(&session);
        repository.move_admission_item(&session, &owner.queue_id, admission.order_revision,
            Some(&cutex::management::v2::admission::AdmissionRef { kind: "ask".into(), id: ask }), true).unwrap();
        assert!(cutex::management::v2::admission::take_change(&session));
        assert!(flush_queued_locked(&session, false, &repository, None, idle_thread,
            recording_sender(&sent), recording_events(&events)).unwrap());
        assert_eq!(*sent.borrow(), ["turn/start moved-owner"]);
        assert!(clients(&repository, &session).is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn queue_on_an_idle_agent_with_a_paused_queue_is_held() {
        let (root, repository) = queue_repository("queue-idle-paused");
        let session = "cutex.queue-idle-paused";
        let held = queue_command(session, "held-1");
        repository
            .enqueue(session, &held.thread_id, &held.management_request_id, &held.params)
            .expect("enqueue");
        assert!(matches!(
            repository.pause(session, "interrupted", Some("turn-stopped".to_string())).unwrap(),
            cutex::management::v2::user_input::QueuePauseDecision::Paused { .. }
        ));
        let queued = submit_v2_with_repository(
            queue_command(session, "held-2"),
            &repository,
            |_, _| Some(None),
            no_history,
            never_sent,
            |_: &str, _: Option<&str>| -> anyhow::Result<bool> {
                panic!("a paused queue is not flushed")
            },
        )
        .expect("queue into a paused queue");
        assert_eq!(queued.disposition, UserInputDisposition::Queued);
        assert!(queued.queue_paused);
        assert_eq!(clients(&repository, session), ["held-1", "held-2"]);
        assert!(repository.pause_state(session).unwrap().is_some());
        std::fs::remove_dir_all(root).expect("remove user-input repository");
    }

    #[test]
    fn auto_on_idle_after_interrupt_skips_paused_owner_queue() {
        let (root, repository) = queue_repository("auto-idle-paused");
        let session = format!("cutex.auto-idle-paused-{}", uuid::Uuid::new_v4());
        let held = queue_command(&session, "held");
        repository.enqueue(&session, &held.thread_id, &held.management_request_id, &held.params).unwrap();
        repository.pause(&session, "interrupted", Some("turn-stopped".into())).unwrap();
        let sent = std::cell::RefCell::new(Vec::new());
        let mut command = queue_command(&session, "fresh");
        command.params.strategy = UserInputStrategy::Auto;
        let started = submit_v2_with_repository(command, &repository, |_, _| Some(None), no_history,
            recording_sender(&sent), |_, _| panic!("paused messages must not flush")).unwrap();
        assert_eq!(started.disposition, UserInputDisposition::Started);
        assert_eq!(*sent.borrow(), ["turn/start fresh"]);
        assert_eq!(clients(&repository, &session), ["held"]);
        assert!(repository.pause_state(&session).unwrap().is_some());

        let ask = admission_ask(&repository, &session);
        let mut command = queue_command(&session, "after-ask");
        command.params.strategy = UserInputStrategy::Auto;
        let queued = submit_v2_with_repository(command, &repository, |_, _| Some(None), no_history,
            never_sent, no_flush).unwrap();
        assert_eq!(queued.disposition, UserInputDisposition::Queued);
        assert!(repository.admission(&session).unwrap().items.iter().any(|item| item.id == ask));
        assert_eq!(clients(&repository, &session), ["held", "after-ask"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn queue_while_busy_waits_for_the_turn_end_flush() {
        let (root, repository) = queue_repository("queue-busy");
        let session = "cutex.queue-busy";
        let queued = submit_v2_with_repository(
            queue_command(session, "busy-1"),
            &repository,
            |_, _| Some(Some("turn-live".to_string())),
            no_history,
            never_sent,
            no_flush,
        )
        .expect("queue while busy");
        assert_eq!(queued.disposition, UserInputDisposition::Queued);
        assert!(!queued.queue_paused);
        assert_eq!(queued.turn_id.as_deref(), Some("turn-live"));
        assert_eq!(clients(&repository, session), ["busy-1"]);
        // turn/completed: the idle flush sends it.
        let sent = std::cell::RefCell::new(Vec::new());
        let events = std::cell::RefCell::new(Vec::new());
        let flushed = flush_queued_locked(
            session,
            false,
            &repository,
            None,
            idle_thread,
            recording_sender(&sent),
            recording_events(&events),
        )
        .expect("flush on turn end");
        assert!(flushed);
        assert_eq!(*sent.borrow(), ["turn/start busy-1"]);
        assert_eq!(
            *events.borrow(),
            ["cutex/userInput/submitted", "cutex/userInput/queueRemoved"]
        );
        assert!(clients(&repository, session).is_empty());
        std::fs::remove_dir_all(root).expect("remove user-input repository");
    }

    /// The race: the turn ends and its idle flush runs on an empty queue; the
    /// client, which still saw the turn running, then submits `queue`. Before,
    /// the item waited for the next turn end / reconnect; now it goes out.
    #[test]
    fn queue_right_after_the_idle_flush_goes_out() {
        let (root, repository) = queue_repository("queue-race");
        let session = "cutex.queue-race";
        let events = std::cell::RefCell::new(Vec::new());
        let flushed = flush_queued_locked(
            session,
            false,
            &repository,
            None,
            idle_thread,
            never_sent,
            recording_events(&events),
        )
        .expect("idle flush on an empty queue");
        assert!(!flushed);
        let sent = std::cell::RefCell::new(Vec::new());
        let execution = submit_v2_with_repository(
            queue_command(session, "late-1"),
            &repository,
            |_, _| Some(None),
            no_history,
            recording_sender(&sent),
            no_flush,
        )
        .expect("late queue submit");
        assert_eq!(execution.disposition, UserInputDisposition::Started);
        assert_eq!(execution.turn_id.as_deref(), Some("turn-new"));
        assert_eq!(*sent.borrow(), ["turn/start late-1"]);
        assert!(clients(&repository, session).is_empty());
        std::fs::remove_dir_all(root).expect("remove user-input repository");
    }

    #[test]
    fn queue_on_an_idle_agent_sends_older_waiting_items_first() {
        let (root, repository) = queue_repository("queue-idle-older");
        let session = "cutex.queue-idle-older";
        let older = queue_command(session, "older-1");
        repository
            .enqueue(session, &older.thread_id, &older.management_request_id, &older.params)
            .expect("enqueue");
        let sent = std::cell::RefCell::new(Vec::new());
        let events = std::cell::RefCell::new(Vec::new());
        let execution = submit_v2_with_repository(
            queue_command(session, "newer-1"),
            &repository,
            |_, _| Some(None),
            no_history,
            never_sent,
            |session: &str, stop_before: Option<&str>| {
                assert_eq!(stop_before, Some("newer-1"));
                flush_queued_locked(
                    session,
                    false,
                    &repository,
                    stop_before,
                    idle_thread,
                    recording_sender(&sent),
                    recording_events(&events),
                )
            },
        )
        .expect("queue behind an older item");
        assert_eq!(execution.disposition, UserInputDisposition::Queued);
        assert!(!execution.queue_paused);
        assert_eq!(*sent.borrow(), ["turn/start older-1"]);
        assert_eq!(clients(&repository, session), ["newer-1"]);
        // The flush never sends the item the submit has not announced yet.
        let none = flush_queued_locked(
            session,
            false,
            &repository,
            Some("newer-1"),
            idle_thread,
            never_sent,
            recording_events(&events),
        )
        .expect("stop before the new item");
        assert!(!none);
        std::fs::remove_dir_all(root).expect("remove user-input repository");
    }

    /// A fake native thread whose accepted input stays invisible: a started
    /// turn is listed `inProgress` with no items (the userMessage shows up
    /// late, as native r50 does right after `turn/start` / `turn/steer`).
    struct LateVisibleNative {
        turns: Mutex<Vec<String>>,
        sends: Mutex<Vec<String>>,
    }

    impl LateVisibleNative {
        fn new() -> Self {
            Self { turns: Mutex::new(Vec::new()), sends: Mutex::new(Vec::new()) }
        }

        fn read(&self, thread_id: &str) -> anyhow::Result<Value> {
            let turns = self.turns.lock().unwrap();
            Ok(json!({"thread": {"id": thread_id, "turns": turns.iter().map(|id| json!({
                "id": id, "status": "inProgress", "items": []
            })).collect::<Vec<_>>()}}))
        }

        fn send(
            &self,
            command: &UserInputSubmitCommand,
            method: &str,
            disposition: UserInputDisposition,
        ) -> Result<UserInputSubmitExecution, UserInputExecutionError> {
            self.sends
                .lock()
                .unwrap()
                .push(format!("{method} {}", command.params.client_user_message_id));
            let mut turns = self.turns.lock().unwrap();
            if method == "turn/start" {
                let next = format!("turn-{}", turns.len() + 1);
                turns.push(next);
            }
            Ok(UserInputSubmitExecution {
                disposition,
                app_server_accepted: true,
                native_request_id: None,
                native_method: Some(method.to_string()),
                turn_id: turns.last().cloned(),
                queue: None,
                queue_paused: false,
            })
        }

        fn active(&self) -> Option<Option<String>> {
            Some(self.turns.lock().unwrap().last().cloned())
        }
    }

    fn retry_command(session: &str, request: &str) -> UserInputSubmitCommand {
        UserInputSubmitCommand {
            management_request_id: request.to_string(),
            cutex_session_id: session.to_string(),
            thread_id: "thread-c010".to_string(),
            params: cutex::management::v2::user_input::UserInputSubmitParams {
                client_user_message_id: "same-client-message".to_string(),
                origin: UserInputOrigin {
                    kind: UserInputOriginKind::Other,
                    client_id: "desktop".to_string(),
                },
                strategy: UserInputStrategy::Auto,
                input: vec![json!({"type": "text", "text": "hello", "text_elements": []})],
                expected_turn_id: None,
            },
        }
    }

    /// C-010 (Kutex): concurrent retries of one clientUserMessageId under
    /// different management requestIds. The session input lock serializes
    /// them, but the first one's userMessage is not yet visible in
    /// `thread/read` when the next runs, so history-based dedupe missed it
    /// and the retries steered the same input again.
    #[test]
    fn concurrent_retries_of_one_client_message_reach_native_once() {
        let (root, repository) = queue_repository("c010-concurrent");
        let session = "cutex.c010";
        let native = LateVisibleNative::new();
        let session_lock = Mutex::new(());
        let barrier = std::sync::Barrier::new(3);
        let results: Vec<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..3)
                .map(|attempt| {
                    let (native, repository, session_lock, barrier) =
                        (&native, &repository, &session_lock, &barrier);
                    scope.spawn(move || {
                        barrier.wait();
                        let _guard = session_lock.lock().unwrap();
                        submit_v2_with_repository(
                            retry_command(session, &format!("request-{attempt}")),
                            repository,
                            |_, _| native.active(),
                            |_, thread| native.read(thread),
                            |command, method, _, disposition| native.send(command, method, disposition),
                            no_flush,
                        )
                    })
                })
                .collect();
            handles.into_iter().map(|handle| handle.join().unwrap()).collect()
        });
        assert_eq!(*native.sends.lock().unwrap(), ["turn/start same-client-message"]);
        let mut dispositions: Vec<_> = results
            .into_iter()
            .map(|result| result.expect("every retry succeeds").disposition)
            .collect();
        dispositions.sort_by_key(|disposition| format!("{disposition:?}"));
        assert_eq!(
            dispositions,
            [
                UserInputDisposition::Deduplicated,
                UserInputDisposition::Deduplicated,
                UserInputDisposition::Started
            ]
        );
        std::fs::remove_dir_all(root).expect("remove user-input repository");
    }

    fn accepted(turn: &str) -> impl Fn(
        &UserInputSubmitCommand,
        &str,
        Value,
        UserInputDisposition,
    ) -> Result<UserInputSubmitExecution, UserInputExecutionError>
           + '_ {
        move |_, method, _, disposition| {
            Ok(UserInputSubmitExecution {
                disposition,
                app_server_accepted: true,
                native_request_id: None,
                native_method: Some(method.to_string()),
                turn_id: Some(turn.to_string()),
                queue: None,
                queue_paused: false,
            })
        }
    }

    fn thread_with(turns: Value) -> impl Fn(&str, &str) -> anyhow::Result<Value> {
        move |_, thread| Ok(json!({"thread": {"id": thread, "turns": turns.clone()}}))
    }

    fn outcome_unknown_error() -> UserInputExecutionError {
        UserInputExecutionError {
            stage: "native_request".to_string(),
            code: "request_outcome_unknown".to_string(),
            message: "transport closed".to_string(),
            retryable: false,
            details: json!({}),
            outcome_unknown: true,
        }
    }

    /// The accepted delivery is durable: a retry after a management restart,
    /// still before the userMessage is visible, sends nothing.
    #[test]
    fn accepted_delivery_dedupes_late_visible_retries_across_reopen() {
        let (root, repository) = queue_repository("c010-reopen");
        let session = "cutex.c010-reopen";
        submit_v2_with_repository(
            retry_command(session, "request-1"),
            &repository,
            |_, _| Some(None),
            no_history,
            accepted("turn-a"),
            no_flush,
        )
        .expect("first submit starts");
        drop(repository);
        let repository = UserInputRepository::open(&root).expect("reopen");
        for attempt in 2..5 {
            let retry = submit_v2_with_repository(
                retry_command(session, &format!("request-{attempt}")),
                &repository,
                |_, _| Some(Some("turn-a".to_string())),
                thread_with(json!([{"id": "turn-a", "status": "inProgress", "items": []}])),
                never_sent,
                no_flush,
            )
            .expect("retry is deduplicated");
            assert_eq!(retry.disposition, UserInputDisposition::Deduplicated);
            assert_eq!(retry.turn_id.as_deref(), Some("turn-a"));
        }
        std::fs::remove_dir_all(root).expect("remove user-input repository");
    }

    /// Same id with other input or origin is still a conflict, never a dedupe.
    #[test]
    fn accepted_delivery_keeps_payload_and_origin_conflicts() {
        let (root, repository) = queue_repository("c010-conflict");
        let session = "cutex.c010-conflict";
        submit_v2_with_repository(
            retry_command(session, "request-1"),
            &repository,
            |_, _| Some(None),
            no_history,
            accepted("turn-a"),
            no_flush,
        )
        .expect("first submit starts");
        for change_origin in [false, true] {
            let mut command = retry_command(session, "request-2");
            if change_origin {
                command.params.origin.client_id = "another-client".to_string();
            } else {
                command.params.input = vec![json!({"type": "text", "text": "other", "text_elements": []})];
            }
            let error = submit_v2_with_repository(
                command,
                &repository,
                |_, _| panic!("conflict before routing"),
                |_, _| panic!("conflict before the history read"),
                never_sent,
                no_flush,
            )
            .expect_err("conflict");
            assert_eq!(error.code, "idempotency_conflict");
        }
        std::fs::remove_dir_all(root).expect("remove user-input repository");
    }

    /// A receipt only proves delivery into the thread it names and while that
    /// turn is still listed: after a history revert (turn gone) or on another
    /// native thread the input is sent again; a forked session has its own ids.
    #[test]
    fn accepted_delivery_does_not_survive_revert_other_thread_or_fork() {
        for case in ["revert", "thread", "fork"] {
            let (root, repository) = queue_repository(&format!("c010-scope-{case}"));
            submit_v2_with_repository(
                retry_command("cutex.c010-scope", "request-1"),
                &repository,
                |_, _| Some(None),
                no_history,
                accepted("turn-a"),
                no_flush,
            )
            .expect("first submit starts");
            let mut command = retry_command(
                if case == "fork" { "cutex.c010-fork" } else { "cutex.c010-scope" },
                "request-2",
            );
            if case == "thread" {
                command.thread_id = "thread-other".to_string();
            }
            let turns = if case == "revert" {
                json!([])
            } else {
                json!([{"id": "turn-a", "status": "completed", "items": []}])
            };
            let sends = std::cell::Cell::new(0);
            let result = submit_v2_with_repository(
                command,
                &repository,
                |_, _| None,
                thread_with(turns),
                |command, method, params, disposition| {
                    sends.set(sends.get() + 1);
                    accepted("turn-b")(command, method, params, disposition)
                },
                no_flush,
            )
            .expect("resubmit");
            assert_eq!(sends.get(), 1, "{case}: sent again");
            assert_eq!(result.disposition, UserInputDisposition::Started);
            std::fs::remove_dir_all(root).expect("remove user-input repository");
        }
    }

    /// Outcome unknown (transport failure after the request left): a retry
    /// neither sends nor claims success while the input may still land
    /// (`user_input_in_progress`); the history settles it: the input shows up
    /// (deduplicated), or the thread is idle without it past the grace period
    /// (sent again).
    #[test]
    fn unknown_delivery_blocks_resend_until_the_history_settles_it() {
        let (root, repository) = queue_repository("c010-unknown");
        let session = "cutex.c010-unknown";
        let client = "same-client-message";
        let error = submit_v2_with_repository(
            retry_command(session, "request-1"),
            &repository,
            |_, _| Some(None),
            no_history,
            |_, _, _, _| Err(outcome_unknown_error()),
            no_flush,
        )
        .expect_err("outcome unknown");
        assert!(error.outcome_unknown);
        let unknown = repository.delivery(session, client).unwrap().expect("recorded");
        assert!(matches!(unknown, InputDelivery::Unknown { .. }));
        // A turn runs (maybe ours, history not caught up yet), or the thread
        // is idle but the unknown send is recent: wait.
        for turns in [
            json!([{"id": "turn-x", "status": "inProgress", "items": []}]),
            json!([]),
        ] {
            let pending = submit_v2_with_repository(
                retry_command(session, "request-2"),
                &repository,
                |_, _| Some(None),
                thread_with(turns),
                never_sent,
                no_flush,
            )
            .expect_err("in progress");
            assert_eq!(pending.code, "user_input_in_progress");
            assert!(pending.retryable);
            assert!(!pending.outcome_unknown);
            assert_eq!(pending.details["delivery"], "unknown");
        }
        // It landed after all: deduplicated, nothing sent.
        let landed = submit_v2_with_repository(
            retry_command(session, "request-3"),
            &repository,
            |_, _| Some(None),
            thread_with(json!([{"id": "turn-x", "status": "completed", "items": [
                {"type": "userMessage", "id": "i", "clientId": client, "content": []}
            ]}])),
            never_sent,
            no_flush,
        )
        .expect("landed");
        assert_eq!(landed.disposition, UserInputDisposition::Deduplicated);
        assert_eq!(landed.turn_id.as_deref(), Some("turn-x"));
        // It did not land: idle thread without it, past the grace period.
        let old = InputDelivery::Unknown {
            thread_id: "thread-c010".to_string(),
            since: (Utc::now() - chrono::Duration::hours(1)).to_rfc3339(),
        };
        assert!(repository
            .set_delivery(session, client, Some(&unknown), Some(old))
            .unwrap());
        let sends = std::cell::Cell::new(0);
        let resent = submit_v2_with_repository(
            retry_command(session, "request-4"),
            &repository,
            |_, _| Some(None),
            thread_with(json!([])),
            |command, method, params, disposition| {
                sends.set(sends.get() + 1);
                accepted("turn-y")(command, method, params, disposition)
            },
            no_flush,
        )
        .expect("resent");
        assert_eq!(sends.get(), 1);
        assert_eq!(resent.disposition, UserInputDisposition::Started);
        assert!(matches!(
            repository.delivery(session, client).unwrap(),
            Some(InputDelivery::Accepted { turn_id: Some(turn), .. }) if turn == "turn-y"
        ));
        std::fs::remove_dir_all(root).expect("remove user-input repository");
    }

    /// The reservation: while a send is recorded in flight (another process,
    /// or a crash mid-send), a submit never sends a second native request; a
    /// definite native refusal clears the reservation so a retry may send.
    #[test]
    fn in_flight_reservation_admits_one_sender_and_refusal_releases_it() {
        let (root, repository) = queue_repository("c010-reserve");
        let session = "cutex.c010-reserve";
        let client = "same-client-message";
        let command = retry_command(session, "request-1");
        assert_eq!(
            repository.register_identity(session, &command.params).unwrap(),
            ClientIdentityDecision::New
        );
        let sending = InputDelivery::Sending {
            thread_id: "thread-c010".to_string(),
            since: Utc::now().to_rfc3339(),
        };
        assert!(repository.set_delivery(session, client, None, Some(sending.clone())).unwrap());
        // A second reservation loses the compare-and-set.
        assert!(!repository.set_delivery(session, client, None, Some(sending.clone())).unwrap());
        let busy = send_recorded(
            &repository,
            &never_sent,
            &command,
            "turn/start",
            json!({}),
            UserInputDisposition::Started,
        )
        .expect_err("reserved by another sender");
        assert_eq!(busy.code, "user_input_in_progress");
        assert_eq!(busy.details["delivery"], "sending");
        let retry = submit_v2_with_repository(
            retry_command(session, "request-2"),
            &repository,
            |_, _| Some(None),
            thread_with(json!([])),
            never_sent,
            no_flush,
        )
        .expect_err("in flight");
        assert_eq!(retry.code, "user_input_in_progress");
        // A definite refusal releases the reservation.
        assert!(repository.set_delivery(session, client, Some(&sending), None).unwrap());
        let refused = send_recorded(
            &repository,
            &|_: &UserInputSubmitCommand, _: &str, _: Value, _: UserInputDisposition| {
                Err(UserInputExecutionError {
                    stage: "native_request".to_string(),
                    code: "native_request_failed".to_string(),
                    message: "refused".to_string(),
                    retryable: false,
                    details: json!({}),
                    outcome_unknown: false,
                })
            },
            &command,
            "turn/start",
            json!({}),
            UserInputDisposition::Started,
        )
        .expect_err("refused");
        assert_eq!(refused.code, "native_request_failed");
        assert_eq!(repository.delivery(session, client).unwrap(), None);
        std::fs::remove_dir_all(root).expect("remove user-input repository");
    }

    /// Native accepted it but the acceptance could not be recorded: the
    /// outcome is reported unknown and not blindly retryable.
    #[test]
    fn unrecorded_acceptance_is_outcome_unknown() {
        let (root, repository) = queue_repository("c010-record-failure");
        let session = "cutex.c010-record-failure";
        let error = submit_v2_with_repository(
            retry_command(session, "request-1"),
            &repository,
            |_, _| Some(None),
            no_history,
            |command, method, params, disposition| {
                // Lose the identity after native accepted, before the record.
                repository.clear_session(session).unwrap();
                accepted("turn-a")(command, method, params, disposition)
            },
            no_flush,
        )
        .expect_err("unrecorded");
        assert!(error.outcome_unknown);
        assert!(!error.retryable);
        assert_eq!(error.stage, "post_submit");
        std::fs::remove_dir_all(root).expect("remove user-input repository");
    }

    /// A queue flush whose native send was accepted but whose event append
    /// failed leaves the item queued; the next flush removes it without a
    /// second native send, even before the userMessage is visible.
    #[test]
    fn accepted_queue_flush_is_not_resent_after_an_event_failure() {
        let (root, repository) = queue_repository("c010-flush");
        let session = "cutex.c010-flush";
        let command = queue_command(session, "queued-1");
        repository
            .enqueue(session, &command.thread_id, &command.management_request_id, &command.params)
            .unwrap();
        let sent = std::cell::RefCell::new(Vec::new());
        assert!(flush_queued_locked(
            session,
            false,
            &repository,
            None,
            idle_thread,
            recording_sender(&sent),
            |_, _, _, _| anyhow::bail!("injected event failure"),
        )
        .is_err());
        assert_eq!(*sent.borrow(), ["turn/start queued-1"]);
        assert_eq!(clients(&repository, session), ["queued-1"]);
        // Accepted queue items cannot be edited any more.
        let item = repository.list(session).unwrap().1.remove(0);
        let edit = repository
            .update(
                session,
                &item.queue_id,
                item.revision,
                vec![json!({"type": "text", "text": "edited", "text_elements": []})],
            )
            .expect_err("edit after acceptance");
        assert!(edit
            .downcast_ref::<cutex::management::v2::user_input::QueueInputAlreadySubmitted>()
            .is_some());
        drop(repository);
        let repository = UserInputRepository::open(&root).unwrap();
        let events = std::cell::RefCell::new(Vec::new());
        assert!(flush_queued_locked(
            session,
            false,
            &repository,
            None,
            thread_with(json!([{"id": "turn-new", "status": "inProgress", "items": []}])),
            never_sent,
            recording_events(&events),
        )
        .unwrap());
        assert!(clients(&repository, session).is_empty());
        assert_eq!(*events.borrow(), ["cutex/userInput/queueRemoved"]);
        std::fs::remove_dir_all(root).expect("remove user-input repository");
    }
}
