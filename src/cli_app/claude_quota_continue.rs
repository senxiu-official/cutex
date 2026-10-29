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
    // Another agent on this account may already know when it resets (the
    // usage reading lags the first failures: host-a 2026-10-21).
    let shared = reset.is_none().then(|| account.as_deref().and_then(|account| known_account_reset(account, now.timestamp()))).flatten();
    match reset.or(shared) {
        Some(reset) => {
            let source = if shared.is_some() { " (as another agent on its account)" } else { "" };
            eprintln!("quota auto-continue: {session} hit its Claude usage limit; resets at {reset}{source}");
            let mut adopted = Vec::new();
            qc::update_default(session, "scheduled", |state| {
                if let Some(account) = account.as_deref() {
                    adopted = state.adopt_account_reset(session, account, reset, now);
                }
                state.schedule(session, reset, account.clone(), turn_id, generation, now) || !adopted.is_empty()
            });
            if !adopted.is_empty() {
                if let Ok(state) = qc::Store::open_default().and_then(|store| store.snapshot()) {
                    for other in adopted {
                        eprintln!("quota auto-continue: {other}: estimated probe moved to the account's reset at {reset}");
                        qc::publish(&state, &other, "scheduled");
                    }
                }
            }
        }
        None => {
            eprintln!("quota auto-continue: {session} hit its Claude usage limit; reset time unknown, estimated probe");
            qc::update_default(session, "scheduled", |state| state.schedule_estimated(session, account, turn_id, generation, now));
        }
    }
}

/// A known (not estimated) pending reset of another agent on `account`.
fn known_account_reset(account: &str, now: i64) -> Option<i64> {
    qc::Store::open_default().and_then(|store| store.snapshot()).ok()?.known_account_reset(account, now)
}

/// Why a due continuation must not go out now (recorded as `skipped`).
fn skip_reason(session: &str, pending: &qc::Pending) -> Option<&'static str> {
    let Some(record) = super::asks_runtime::record(session) else {
        return Some("archived");
    };
    if cutex::agent_management::agent_execution_blocked(&record) {
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
    // Whole-agent/project pause and lifecycle contention are checked together
    // with the final take_due CAS. They must never consume this pending item.
    (queue_paused || asks_paused).then_some("paused")
}

fn claim_due(store:&qc::Store,asks:&cutex::asks::store::AskStore,session:&str,expected:&qc::Pending,now:chrono::DateTime<Utc>)->anyhow::Result<Option<qc::Pending>> {
    cutex::asks::pause_admission::unless_paused(asks,session,|_|
        store.with_state(|state| {
            if state.sessions.get(session).and_then(|s|s.pending.as_ref())!=Some(expected) {return None;}
            state.take_due(session,now)
        }))?.transpose().map(Option::flatten)
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
    // An estimated probe whose account meanwhile reports the exhausted
    // window's reset (another agent on it learned it): wait for that.
    if let Some((account, reset)) = estimated_account_reset(&store, session, now) {
        if let Ok(Some(state)) = store.with_state(|state| state.reschedule_estimated(session, &account, reset, now).then(|| state.clone())) {
            eprintln!("quota auto-continue: {session}: estimated probe moved to the account's reset at {reset}");
            qc::publish(&state, session, "scheduled");
        }
        return;
    }
    let Some(expected)=store.snapshot().ok().and_then(|s|s.sessions.get(session).and_then(|e|e.pending.clone())) else {return;};
    // Runtime/queue observations happen before authority/AskStore, and this
    // exact pending value is checked again under the final reservation.
    let skip=skip_reason(session,&expected);
    let asks=match cutex::asks::store::AskStore::open_default(){Ok(store)=>store,Err(_)=>return};
    let pending = match claim_due(&store,&asks,session,&expected,now) {
        Ok(Some(pending)) => pending,
        Ok(None) => return,
        Err(error) => {
            eprintln!("quota auto-continue: {session}: {error:#}");
            return;
        }
    };
    let (result, reason): (&str, String) = match skip {
        Some(reason) => ("skipped", reason.to_string()),
        // Delegations that failed on the limit were put back in the queue
        // and are redelivered now that the hold ended: that is the work.
        // The failed turn closed the native gate; reopen it, or the
        // redelivery waits for the owner (host-a 2026-10-21).
        None if asks_requeued(session) => {
            cutex::asks::control::quota_reset_reopen_default(session);
            ("skipped", "asksRedelivered".to_string())
        }
        None => match qc::send(super::management_context::management_request_context(), session, pending.reset_at) {
            Ok(disposition) => ("fired", disposition),
            Err(error) => ("failed", format!("{error:#}")),
        },
    };
    eprintln!("quota auto-continue: {session}: {result} ({reason})");
    record(&store, session, result, &reason, pending.reset_at);
}

/// The stored usage snapshot of the account of `session`'s due estimated
/// probe still shows an exhausted window resetting later (no network read).
fn estimated_account_reset(store: &qc::Store, session: &str, now: chrono::DateTime<Utc>) -> Option<(String, i64)> {
    let state = store.snapshot().ok()?;
    let pending = state.sessions.get(session)?.pending.as_ref().filter(|pending| pending.estimated)?;
    let account = pending.account_id.clone()?;
    let dir = cutex::management::v2::profile_quota_native::store_dir().ok()?;
    let snapshot = cutex::management::v2::profile_quota_claude::load_at(&dir, &account)?;
    let reset = qc::exhausted_reset_from_windows(&snapshot.windows, now.timestamp())?;
    (reset > now.timestamp()).then_some((account, reset))
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

#[cfg(test)]
mod pause_tests {
    use super::*;
    #[test]
    fn due_quota_claim_defers_without_consuming_pending_reset_or_probe_budget() {
        let root=std::env::temp_dir().join(format!("quota-pause-{}",uuid::Uuid::new_v4()));
        let store=qc::Store::at(root.join("quota.json"));
        let asks=cutex::asks::store::AskStore::at(root.join("asks"));
        let now=Utc::now();
        let pending=qc::Pending{at:now.to_rfc3339(),reset_at:now.timestamp(),account_id:None,turn_id:Some("failed-turn".into()),runtime_generation:7,scheduled_at:now.to_rfc3339(),estimated:true};
        store.with_state(|s|{s.sessions.entry("cutex.quota".into()).or_default().pending=Some(pending.clone());}).unwrap();
        let before=store.snapshot().unwrap();
        let held=cutex::asks::pause_admission::try_with_authority(|_|{
            assert!(claim_due(&store,&asks,"cutex.quota",&pending,now)?.is_none());Ok(())
        }).unwrap();
        assert!(matches!(held,cutex::asks::pause_admission::Admission::Ready(())));
        assert_eq!(store.snapshot().unwrap(),before);
        let mut replacement=pending.clone();replacement.runtime_generation=8;
        store.with_state(|s|s.sessions.get_mut("cutex.quota").unwrap().pending=Some(replacement.clone())).unwrap();
        assert!(claim_due(&store,&asks,"cutex.quota",&pending,now).unwrap().is_none());
        assert_eq!(store.snapshot().unwrap().sessions["cutex.quota"].pending,Some(replacement));
        store.with_state(|s|s.sessions.get_mut("cutex.quota").unwrap().pending=Some(pending.clone())).unwrap();
        assert_eq!(claim_due(&store,&asks,"cutex.quota",&pending,now).unwrap(),Some(pending.clone()));
        assert!(claim_due(&store,&asks,"cutex.quota",&pending,now).unwrap().is_none());
        let entry=&store.snapshot().unwrap().sessions["cutex.quota"];
        assert_eq!(entry.unknown_probes,1);assert_eq!(entry.fired_reset_at,Some(pending.reset_at));
        let _=std::fs::remove_dir_all(root);
    }
}
