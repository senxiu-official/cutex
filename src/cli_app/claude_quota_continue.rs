//! Management glue of the Claude quota auto-continue
//! (`cutex::management::v2::quota_continue`, capability
//! `claude-quota-auto-continue`): observes Claude turn ends, finds when the
//! exhausted limit resets, and sends the due continuations.
use std::time::Duration;

use chrono::Utc;
use serde_json::{json, Value};

use cutex::app_server::client::AppServerEvent;
use cutex::management::v2::quota_continue::{self as qc, StateFile};

/// Longest sleep of the worker between due checks.
const MAX_SLEEP: Duration = Duration::from_secs(15);

/// A Claude runtime event of `session` (observer of `claude_runtime`).
pub(super) fn on_runtime_event(session: &str, thread_id: &str, generation: u64, event: &AppServerEvent) {
    let AppServerEvent::Notification(notification) = event else {
        return;
    };
    if notification.method != "turn/completed" {
        return;
    }
    let Some(params) = notification.params.as_ref() else {
        return;
    };
    if params.get("threadId").and_then(Value::as_str) != Some(thread_id) {
        return;
    }
    let Some(turn) = params.get("turn") else {
        return;
    };
    if qc::quota_failure(turn) {
        let session = session.to_string();
        let turn_id = turn.get("id").and_then(Value::as_str).map(str::to_string);
        // Hold the agent now (estimated), before the asks observer sees this
        // turn end: a delegation it puts back in the queue must not be
        // redelivered before the reset lookup below has run.
        let held_turn = turn_id.clone();
        qc::update_default(&session, "scheduled", |state| state.hold(&session, held_turn, generation, Utc::now()));
        // Reads the adapter (a request from the observer thread could wait on
        // itself) and possibly the account's usage endpoint.
        let _ = std::thread::Builder::new().name("cutex-quota-reset".into()).spawn(move || {
            schedule_after_failure(&session, generation, turn_id);
        });
    } else if turn.get("status").and_then(Value::as_str) == Some("completed") {
        // The agent got a turn through after all: it is not stuck any more.
        qc::completed_default(session);
    }
}

fn adapter_reset(session: &str, generation: u64, now: i64) -> Option<i64> {
    let handle = super::claude_runtime::manager().handle_for_generation(session, generation).ok()?;
    let limits = handle.request("account/rateLimits/read", json!({})).ok()?;
    qc::exhausted_reset_from_rate_limits(&limits, now)
}

fn account_reset(account: &str, now: i64) -> Option<i64> {
    use cutex::management::v2::profile_quota_claude as quota;
    let snapshot = quota::refresh(account).ok().flatten().or_else(|| {
        let dir = cutex::management::v2::profile_quota_native::store_dir().ok()?;
        quota::load_at(&dir, account)
    })?;
    qc::exhausted_reset_from_windows(&snapshot.windows, now)
}

fn schedule_after_failure(session: &str, generation: u64, turn_id: Option<String>) {
    let now = Utc::now();
    let account = super::asks_runtime::record(session).and_then(|record| {
        cutex::runtime::claude_accounts::session_account_id(&cutex::runtime::claude_accounts::load_cached(), &record)
    });
    let reset = adapter_reset(session, generation, now.timestamp())
        .or_else(|| account.as_deref().and_then(|account| account_reset(account, now.timestamp())));
    match reset {
        Some(reset) => {
            eprintln!("quota auto-continue: {session} hit its Claude usage limit; resets at {reset}");
            qc::update_default(session, "scheduled", |state| state.schedule(session, reset, account, turn_id, generation, now));
        }
        None => {
            eprintln!("quota auto-continue: {session} hit its Claude usage limit; reset time unknown, estimated probe");
            qc::update_default(session, "scheduled", |state| state.schedule_estimated(session, account, turn_id, generation, now));
        }
    }
}

/// Why a due continuation must not go out now (recorded as `skipped`).
fn skip_reason(session: &str, pending: &qc::Pending) -> Option<&'static str> {
    let Some(record) = super::asks_runtime::record(session) else {
        return Some("archived");
    };
    if record.is_retired() {
        return Some("archived");
    }
    match super::claude_runtime::manager().status(session) {
        Ok(Some(status)) if status.connected => {
            if status.runtime_generation != pending.runtime_generation {
                return Some("runtimeRestarted");
            }
        }
        _ => return Some("offline"),
    }
    let queue_paused = cutex::management::v2::user_input::user_input_repository()
        .and_then(|queue| queue.pause_state(session))
        .map(|pause| pause.is_some())
        .unwrap_or(true);
    // An owner stop or delegation pause holds it too; a closed runtime gate
    // does not (the continuation is owner input and reopens it).
    let asks_paused = cutex::asks::control::owner_paused_default(session);
    (queue_paused || asks_paused).then_some("paused")
}

fn fire(session: &str) {
    let now = Utc::now();
    let store = match qc::Store::open_default() {
        Ok(store) => store,
        Err(error) => {
            eprintln!("quota auto-continue: store unavailable: {error:#}");
            return;
        }
    };
    let pending = match store.with_state(|state| state.take_due(session, now)) {
        Ok(Some(pending)) => pending,
        Ok(None) => return,
        Err(error) => {
            eprintln!("quota auto-continue: {session}: {error:#}");
            return;
        }
    };
    let (result, reason): (&str, String) = match skip_reason(session, &pending) {
        Some(reason) => ("skipped", reason.to_string()),
        // Delegations that failed on the limit were put back in the queue
        // and are redelivered now that the hold ended: that is the work.
        None if asks_requeued(session) => ("skipped", "asksRedelivered".to_string()),
        None => match qc::send(super::management_context::management_request_context(), session, pending.reset_at) {
            Ok(disposition) => ("fired", disposition),
            Err(error) => ("failed", format!("{error:#}")),
        },
    };
    eprintln!("quota auto-continue: {session}: {result} ({reason})");
    record(&store, session, result, &reason, pending.reset_at);
}

/// Open delegations of `session` that went back to the queue after a quota
/// failure and wait to be redelivered.
fn asks_requeued(session: &str) -> bool {
    cutex::asks::store::AskStore::open_default()
        .and_then(|store| store.peek())
        .ok()
        .flatten()
        .is_some_and(|state| {
            state.asks.values().any(|ask| {
                ask.remote.is_none()
                    && ask.target_cutex_session_id == session
                    && ask.quota_requeues > 0
                    && (ask.execution == cutex::asks::model::ExecutionState::Queued || ask.resume_pending())
            })
        })
}

fn record(store: &qc::Store, session: &str, result: &str, reason: &str, reset_at: i64) {
    let reason: String = reason.chars().take(300).collect();
    match store.with_state(|state: &mut StateFile| {
        state.record(session, result, Some(&reason), Some(reset_at), Utc::now());
        state.clone()
    }) {
        Ok(state) => qc::publish(&state, session, result),
        Err(error) => eprintln!("quota auto-continue: {session}: {error:#}"),
    }
}

/// The worker: sends due continuations (management process only).
pub(super) fn spawn_worker() -> anyhow::Result<()> {
    std::thread::Builder::new().name("cutex-quota-continue".into()).spawn(|| loop {
        let state = qc::Store::open_default().and_then(|store| store.snapshot()).unwrap_or_default();
        let now = Utc::now();
        for session in state.due(now) {
            fire(&session);
        }
        let sleep = state
            .next_at()
            .and_then(|at| (at - Utc::now()).to_std().ok())
            .map(|until| until.min(MAX_SLEEP))
            .unwrap_or(MAX_SLEEP)
            .max(Duration::from_millis(200));
        std::thread::sleep(sleep);
    })?;
    Ok(())
}
