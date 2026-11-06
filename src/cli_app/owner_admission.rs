//! r271 owner admission worker (desktop design 050 §5, `owner-admission-held-v1`).
//!
//! One thread per Management process, never a bridge worker; lifecycle never
//! joins it. Lock protocol: it waits for the host authority fence holding no
//! session lane, then only TRIES the session lane (debug-enforced by
//! `session_coordination::set_thread_lane_try_only`). With both held it re-runs
//! the ordinary owner route; the fence is dropped right after the re-check and
//! the C-010 reservation, before any native call (`send_user_input_native_request`).
//! Held inputs past `owner_held::HELD_BUDGET` fail visibly (`admission_timeout`).
use std::cell::Cell;
use std::time::Duration;

use cutex::management::v2::owner_held::{self, HeldOwnerInput};
use cutex::management::v2::user_input::{
    UserInputDisposition, UserInputExecutionError, UserInputSubmitCommand, UserInputSubmitExecution,
};
use serde_json::{json, Value};

use super::app_server_user_input;

thread_local! {
    static ON_WORKER: Cell<bool> = const { Cell::new(false) };
}

pub(crate) fn on_worker_thread() -> bool {
    ON_WORKER.with(Cell::get)
}

pub(crate) fn held_execution() -> UserInputSubmitExecution {
    UserInputSubmitExecution {
        disposition: UserInputDisposition::Held,
        app_server_accepted: false,
        native_request_id: None,
        native_method: None,
        turn_id: None,
        queue: None,
        queue_paused: false,
        join_refusal: None,
        queued_reason: None,
    }
}

/// The submit params of `command` in their wire form (re-parsed on admission).
fn wire_params(command: &UserInputSubmitCommand) -> Value {
    let params = &command.params;
    let mut wire = json!({
        "clientUserMessageId": params.client_user_message_id,
        "origin": params.origin,
        "strategy": params.strategy,
        "input": params.input,
    });
    if let Some(turn) = &params.expected_turn_id {
        wire["expectedTurnId"] = json!(turn);
    }
    if let Some(join) = &params.join_delegation {
        wire["joinDelegation"] = json!(join);
    }
    wire
}

/// Record `command` as held (idempotent by identity) and wake the worker.
/// Called under the session lane by the HTTP submit only.
pub(crate) fn hold(
    command: &UserInputSubmitCommand,
) -> Result<UserInputSubmitExecution, UserInputExecutionError> {
    owner_held::hold(HeldOwnerInput::new(
        &command.cutex_session_id,
        &command.params.client_user_message_id,
        &command.management_request_id,
        &command.thread_id,
        wire_params(command),
        owner_held::held_claude_backend(),
        chrono::Utc::now(),
    ))
    .map_err(|error| UserInputExecutionError {
        stage: "route".into(),
        code: "event_persistence_unavailable".into(),
        message: format!("owner held input could not be recorded: {error:#}"),
        retryable: true,
        details: json!({}),
        outcome_unknown: false,
    })?;
    Ok(held_execution())
}

pub(crate) fn spawn() -> anyhow::Result<()> {
    // A claim of a previous process re-runs; C-010 decides whether it was sent.
    owner_held::unclaim_all()?;
    std::thread::Builder::new()
        .name("cutex-owner-admission".into())
        .spawn(|| {
            ON_WORKER.with(|flag| flag.set(true));
            cutex::management::v2::session_coordination::set_thread_lane_try_only(true);
            let mut backoff = Duration::from_millis(50);
            loop {
                owner_held::wait_for_work(Duration::from_secs(1));
                if !owner_held::any_waiting().unwrap_or(false) {
                    continue;
                }
                match admit_one(&app_server_user_input::submit_v2) {
                    Ok(true) => backoff = Duration::from_millis(50),
                    Ok(false) => {
                        std::thread::sleep(backoff);
                        backoff = (backoff * 2).min(Duration::from_millis(200));
                    }
                    Err(error) => {
                        eprintln!("owner admission: {error:#}");
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
            }
        })?;
    // r271 (051 §2 decision A): dead launch claims a lane holder could not
    // clear (busy fence) are cleared here, lane-free, once the fence is free.
    // Rules (051 §2): queued only on hostBusy; lane-free; under the fence only
    // claim / roster record changes (a wedged runtime is stopped after the
    // fence is released, holding only its lane); retries at least 1 s apart
    // with jitter; fence holds go through the step-0 lock accounting.
    std::thread::Builder::new()
        .name("cutex-launch-claim-sweep".into())
        .spawn(|| loop {
            wait_for_sweeps(Duration::from_secs(1));
            sweep_launch_claims_once();
            if sweep_queue()
                .0
                .lock()
                .is_ok_and(|pending| !pending.is_empty())
            {
                let jitter = (uuid::Uuid::new_v4().as_u128() % 500) as u64;
                std::thread::sleep(Duration::from_millis(1_000 + jitter));
            }
        })?;
    // Its own thread: the worker may wait for the fence longer than the budget.
    std::thread::Builder::new()
        .name("cutex-owner-held-expiry".into())
        .spawn(|| loop {
            std::thread::sleep(Duration::from_secs(1));
            expire_held();
        })?;
    Ok(())
}

/// One fence hold: the first waiting session whose lane is free gets its head
/// input admitted. `Ok(false)`: no lane was free (fence released, back off).
pub(crate) type Submit<'a> =
    &'a dyn Fn(UserInputSubmitCommand) -> Result<UserInputSubmitExecution, UserInputExecutionError>;

pub(crate) fn admit_one(submit: Submit<'_>) -> anyhow::Result<bool> {
    // Blocking FIFO wait for the fence; this thread holds no lane here.
    cutex::asks::pause_admission::acquire_worker_fence()?;
    let result = (|| -> anyhow::Result<Option<(String, Option<String>)>> {
        for session in owner_held::sessions_in_order()? {
            let lane = cutex::management::v2::session_coordination::session_lane(&session)?;
            let Ok(_lane) = lane.try_lock() else { continue };
            let Some(held) = owner_held::claim_head(&session)? else {
                continue;
            };
            return Ok(Some((session, admit(held, submit)?)));
        }
        Ok(None)
    })();
    cutex::asks::pause_admission::release_worker_fence();
    let Some((session, landed)) = result? else {
        return Ok(false);
    };
    // The owner's held message landed: as for an HTTP submit, it ends the
    // session's delegation pause, and (started / steered) the stop hold or
    // closed-gate pause an interrupt-and-send left. Until then that hold kept
    // deliveries from going ahead of it. Outside the lane and the fence.
    if let Some(disposition) = landed {
        cutex::asks::control::owner_message_default(&session, &disposition);
    }
    Ok(true)
}

/// Re-run the ordinary route for `held` (lane + fence held), publish its
/// outcome as the HTTP submit would have, and drop the held record.
fn admit(held: HeldOwnerInput, submit: Submit<'_>) -> anyhow::Result<Option<String>> {
    let execution =
        match cutex::management::v2::user_input::parse_user_input_submit_params(&held.params) {
            Ok(params) => {
                let command = UserInputSubmitCommand {
                    management_request_id: held.management_request_id.clone(),
                    cutex_session_id: held.cutex_session_id.clone(),
                    thread_id: held.thread_id.clone(),
                    params: params.clone(),
                };
                let execution = submit(command);
                Some((params, execution))
            }
            Err(message) => {
                eprintln!(
                    "owner admission: {}: unreadable held input: {message}",
                    held.cutex_session_id
                );
                None
            }
        };
    // Never publish under the fence (the route already released it before
    // native IO; release again for the queue/failure paths).
    cutex::asks::pause_admission::release_worker_fence();
    let mut landed = None;
    if let Some((params, execution)) = execution {
        let events = cutex::management::v2::repository::management_v2_repository()?;
        match cutex::management::v2::server::publish_submit_outcome(
            events,
            &held.cutex_session_id,
            &held.thread_id,
            &held.management_request_id,
            &params,
            held.claude_backend,
            execution,
        ) {
            Ok(result) if cutex::management::v2::server::owner_message_landed(&result) => {
                landed = result
                    .get("disposition")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            }
            Ok(_) => {}
            Err(error) => eprintln!(
                "owner admission: {}: {}: {}",
                held.cutex_session_id, error.code, error.message
            ),
        }
    }
    owner_held::finish(&held.cutex_session_id, &held.client_user_message_id)?;
    Ok(landed)
}

/// Lane-free follow-ups of work a lane holder could not do with the host
/// fence busy (051 §2 decision A): clear a dead launch claim, or drop the
/// roster row of a deleted session.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Sweep {
    LaunchClaim(String),
    ForgetDeleted(String),
}

fn sweep_queue() -> &'static (
    std::sync::Mutex<std::collections::BTreeSet<Sweep>>,
    std::sync::Condvar,
) {
    static SWEEPS: std::sync::OnceLock<(
        std::sync::Mutex<std::collections::BTreeSet<Sweep>>,
        std::sync::Condvar,
    )> = std::sync::OnceLock::new();
    SWEEPS.get_or_init(Default::default)
}

fn queue_sweep(sweep: Sweep) {
    let (pending, wake) = sweep_queue();
    if let Ok(mut pending) = pending.lock() {
        pending.insert(sweep);
        wake.notify_all();
    }
}

/// Session delete removed the record but met a busy fence for its roster row.
pub(crate) fn sweep_forget_deleted_session(key: &str) {
    queue_sweep(Sweep::ForgetDeleted(key.to_string()));
}

/// A lane holder met a busy host fence while checking `key`'s launch claim.
/// The sweep thread re-runs the check on the lane-free (blocking) path, so a
/// dead claim is cleared as soon as the fence is free.
pub(crate) fn sweep_launch_claim(key: &str) {
    queue_sweep(Sweep::LaunchClaim(key.to_string()));
}

fn wait_for_sweeps(timeout: Duration) {
    let (pending, wake) = sweep_queue();
    if let Ok(guard) = pending.lock() {
        if guard.is_empty() {
            let _ = wake.wait_timeout(guard, timeout);
        }
    }
}

/// Check every pending claim once on this (lane-free) thread. A cleared or
/// live claim leaves the set; only a still-busy host keeps it pending.
pub(crate) fn sweep_launch_claims_once() {
    let sweeps: Vec<Sweep> = match sweep_queue().0.lock() {
        Ok(pending) => pending.iter().cloned().collect(),
        Err(_) => return,
    };
    for sweep in sweeps {
        debug_assert!(!cutex::management::v2::session_coordination::thread_holds_lane());
        let busy = match &sweep {
            Sweep::LaunchClaim(key) => {
                match cutex::agent_management::with_wedged_stop_outside_fence(|| {
                    super::management_context::clear_dead_launch_claim_once(key)
                }) {
                    Err(error) if cutex::asks::pause_admission::is_deferred(&error) => true,
                    Err(error) => {
                        eprintln!("launch claim sweep: {key}: {error:#}");
                        false
                    }
                    Ok(()) => false,
                }
            }
            Sweep::ForgetDeleted(key) => {
                let result = cutex::role_revision::CutexSessionId::new(key.clone())
                    .map_err(|_| anyhow::anyhow!("invalid durable Agent ID"))
                    .and_then(|id| {
                        cutex::agent_management::AgentManagementProvider::open_default()
                            .map_err(|error| anyhow::anyhow!("{error:#}"))?
                            .forget_deleted_session(&id)
                            .map_err(anyhow::Error::from)
                    });
                match result {
                    Err(error)
                        if error
                            .downcast_ref::<cutex::agent_management::AgentManagementError>()
                            .is_some_and(|error| {
                                matches!(
                                    error,
                                    cutex::agent_management::AgentManagementError::HostBusy(_)
                                )
                            }) =>
                    {
                        true
                    }
                    Err(error) => {
                        eprintln!(
                            "session delete sweep: roster cleanup failed for {key}: {error:#}"
                        );
                        false
                    }
                    Ok(_) => false,
                }
            }
        };
        if !busy {
            if let Ok(mut pending) = sweep_queue().0.lock() {
                pending.remove(&sweep);
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn launch_claim_sweep_pending(key: &str) -> bool {
    sweep_queue()
        .0
        .lock()
        .is_ok_and(|pending| pending.contains(&Sweep::LaunchClaim(key.to_string())))
}

/// Every second: held inputs past their budget (never one the worker has
/// claimed) fail visibly. Nothing was sent; the owner decides (no automatic
/// queue or mode change).
pub(crate) fn expire_held() {
    let expired = match owner_held::take_expired(chrono::Utc::now()) {
        Ok(expired) => expired,
        Err(error) => {
            eprintln!("owner admission: expiry: {error:#}");
            return;
        }
    };
    for held in expired {
        let result = cutex::management::v2::repository::management_v2_repository().and_then(|events| {
            events.append(cutex::management::v2::model::PendingEvent {
                cutex_session_id: held.cutex_session_id.clone(),
                host_id: cutex::platform::host::current_host_name(),
                source: cutex::management::v2::model::EventSource::Cutex,
                schema: None,
                correlation: cutex::management::v2::model::EventCorrelation {
                    thread_id: Some(held.thread_id.clone()),
                    client_user_message_id: Some(held.client_user_message_id.clone()),
                    management_request_id: Some(held.management_request_id.clone()),
                    ..Default::default()
                },
                native: None,
                cutex: Some(cutex::management::v2::model::CutexMessage {
                    method: "cutex/userInput/failed".into(),
                    params: json!({
                        "managementRequestId": held.management_request_id,
                        "clientUserMessageId": held.client_user_message_id,
                        "origin": held.params["origin"],
                        "input": held.params["input"],
                        "stage": "admission",
                        "error": {
                            "source": "cutex",
                            "code": owner_held::ADMISSION_TIMEOUT_CODE,
                            "message": "the host stayed busy; the message was not sent",
                            "retryable": true,
                            "details": { "reason": owner_held::HELD_BY_HOST_BUSY, "heldSince": held.held_since },
                        },
                        "failedAt": chrono::Utc::now().to_rfc3339(),
                    }),
                }),
            })?;
            Ok(())
        });
        if let Err(error) = result {
            eprintln!(
                "owner admission: {}: timeout event: {error:#}",
                held.cutex_session_id
            );
        }
        // 054 §2.9: held past its budget → failed{admission_timeout}.
        cutex::management::v2::user_input::ledger::held_expired(&held.cutex_session_id, &held.client_user_message_id);
    }
}
