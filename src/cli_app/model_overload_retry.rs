//! Management-only overload worker. Observer hooks never acquire a session
//! lane or issue RPC: the RPC reader may be calling the observer synchronously.
use chrono::Utc;
use cutex::app_server::{
    client::{AppServerEvent, AppServerHandle},
    external_input::{Delivery, DeliveryState, Envelope, Message, Response, Source, SourceKind},
};
use cutex::asks::{model::LeaseHolder, store::AskStore};
use cutex::management::v2::model_overload_retry::{self as retry, Phase, Plan, Store};
use serde_json::{json, Value};

pub(super) fn on_runtime_event(
    session: &str,
    thread: &str,
    generation: u64,
    event: &AppServerEvent,
) {
    let AppServerEvent::Notification(note) = event else {
        return;
    };
    if !matches!(
        note.method.as_str(),
        "turn/started" | "turn/completed" | "item/completed"
    ) {
        return;
    }
    let Some(params) = &note.params else {
        return;
    };
    if params["threadId"].as_str() != Some(thread) {
        return;
    }
    let Some(record) = super::asks_runtime::record(session) else {
        return;
    };
    if record.runtime_generation != generation || record.app_server_thread_id() != Some(thread) {
        return;
    }
    let result = (|| -> anyhow::Result<bool> {
        let store = Store::open_default()?;
        if store.is_disabled() {
            return Ok(false);
        }
        if note.method == "item/completed" {
            capture_early_message(&store, session, thread, generation, params)?;
            return Ok(false);
        }
        let asks = AskStore::open_default()?;
        observe(
            &store,
            &asks,
            session,
            thread,
            generation,
            &note.method,
            &params["turn"],
            Utc::now(),
        )
    })();
    match result {
        Ok(true) => publish_current(session),
        Err(error) => eprintln!("model overload retry observer {session}: {error:#}"),
        _ => {}
    }
}

fn capture_early_message(
    store: &Store,
    session: &str,
    thread: &str,
    generation: u64,
    params: &Value,
) -> anyhow::Result<()> {
    if !cutex::asks::answer::is_final_agent_message(&params["item"]) {
        return Ok(());
    }
    store.update(|s| {
        let Some(p) = s.sessions.get_mut(session).and_then(|e| e.plan.as_mut()) else {
            return;
        };
        if matches!(p.phase, Phase::Firing | Phase::Uncertain | Phase::Binding)
            && p.thread_id == thread
            && p.runtime_generation == generation
            && params["turnId"].as_str().is_some_and(|id| id != p.turn_id)
        {
            p.early_message = Some(params.clone());
        }
    })
}

// No lane/RPC on the reader callback. A later observed turn invalidates a
// waiting timer atomically. A claimed send retains only receipt-bound events.
fn observe(
    store: &Store,
    asks: &AskStore,
    session: &str,
    thread: &str,
    generation: u64,
    method: &str,
    turn: &Value,
    now: chrono::DateTime<Utc>,
) -> anyhow::Result<bool> {
    let lease = asks.peek()?.and_then(|s| s.leases.get(session).cloned());
    let (changed, cancelled) = store.update(|s| {
        if turn["id"].as_str().is_some_and(|id| {
            s.sessions
                .get(session)
                .is_some_and(|e| e.seen.iter().any(|v| v == id))
        }) {
            return (false, None);
        }
        let superseded = s
            .sessions
            .get(session)
            .and_then(|e| e.plan.as_ref())
            .is_some_and(|p| {
                matches!(p.phase, Phase::Pending | Phase::Running)
                    && (p.runtime_generation != generation
                        || p.thread_id != thread
                        || turn["id"].as_str().is_some_and(|id| id != p.turn_id))
            });
        if superseded {
            return (true, s.cancel(session, "newerTurn", now));
        }
        if method == "turn/completed" {
            (
                s.terminal(session, thread, generation, turn, lease, now),
                None,
            )
        } else {
            (false, None)
        }
    })?;
    if cancelled.is_some() {
        retry::recover_settlement(store, asks, session)?;
    }
    Ok(changed)
}

fn publish_current(session: &str) {
    let Ok(s) = Store::open_default().and_then(|s| s.snapshot()) else {
        return;
    };
    let Some(e) = s.sessions.get(session) else {
        return;
    };
    let reason = if e.plan.as_ref().is_some_and(|p| p.phase == Phase::Pending) {
        "scheduled"
    } else if e.last.as_ref().is_some_and(|v| v["reason"] == "turnEnded") {
        "completed"
    } else {
        e.last
            .as_ref()
            .and_then(|v| v["result"].as_str())
            .unwrap_or("fired")
    };
    retry::publish(session, reason);
}

fn envelope(session: &str, plan: &Plan) -> Envelope {
    let mut envelope = Envelope {
        version: 1,
        owner_id: session.into(),
        thread_id: plan.thread_id.clone(),
        runtime_generation: plan.runtime_generation,
        message: Message {
            id: plan.message_id.clone(),
            source: Source {
                kind: SourceKind::Service,
                id: "cutex-model-overload-retry".into(),
            },
            event_type: "model_overload_continue".into(),
            delivery: Delivery::AfterTurn,
            text: retry::CONTINUE_TEXT.into(),
        },
        view: None,
        semantic_sha256: String::new(),
    };
    envelope.semantic_sha256 = envelope.digest();
    envelope
}

fn validate(raw: Value, envelope: &Envelope) -> anyhow::Result<Response> {
    let response: Response = serde_json::from_value(raw)?;
    response.validate(
        &cutex::launch::stock::ExternalInputBinding {
            version: 1,
            owner_id: envelope.owner_id.clone(),
            thread_id: envelope.thread_id.clone(),
            runtime_generation: envelope.runtime_generation,
            canonical_byte_limit: Default::default(),
        },
        &[envelope.key()],
    )?;
    Ok(response)
}

fn status(handle: &AppServerHandle, envelope: &Envelope) -> anyhow::Result<Option<String>> {
    let response = validate(handle.request("thread/externalInput/status", json!({"version":1,
        "ownerId":envelope.owner_id,"threadId":envelope.thread_id,"runtimeGeneration":envelope.runtime_generation,
        "messages":[envelope.key()]}))?,envelope)?;
    Ok(response
        .statuses
        .first()
        .filter(|s| s.delivery_state == DeliveryState::ContextPersisted)
        .and_then(|s| s.receipt.as_ref())
        .map(|r| r.turn_id.clone()))
}

fn bound(
    store: &Store,
    asks: &AskStore,
    session: &str,
    plan: &Plan,
    turn: &str,
) -> anyhow::Result<()> {
    bound_with_hook(store, asks, session, plan, turn, || Ok(()))
}

fn bound_with_hook(
    store: &Store,
    asks: &AskStore,
    session: &str,
    plan: &Plan,
    turn: &str,
    after_rebind: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    retry::begin_binding(store, session, plan, turn)?;
    retry::rebind(asks, session, plan, turn)?;
    after_rebind()?;
    let (early, message) = retry::finish_binding(store, session, plan, turn)?;
    retry::publish(session, "fired");
    if let Some(message) = message.filter(|v| v["turnId"].as_str() == Some(turn)) {
        cutex::asks::observe::observe(asks, session, "item/completed", &message, Utc::now())?;
    }
    // A very fast terminal may have arrived while the RPC reader delivered the
    // acceptance response. Reprocess only the receipt-bound terminal, once.
    if let Some(terminal) = early.filter(|t| t["id"].as_str() == Some(turn)) {
        let lease = asks.peek()?.and_then(|s| s.leases.get(session).cloned());
        store.update(|s| {
            s.terminal(
                session,
                &plan.thread_id,
                plan.runtime_generation,
                &terminal,
                lease,
                Utc::now(),
            )
        })?;
        publish_current(session);
        cutex::asks::observe::observe(
            asks,
            session,
            "turn/completed",
            &json!({"threadId":plan.thread_id,"turn":terminal}),
            Utc::now(),
        )?;
    }
    Ok(())
}

fn eligible(session: &str, plan: &Plan) -> anyhow::Result<AppServerHandle> {
    let record = super::asks_runtime::record(session).ok_or_else(|| anyhow::anyhow!("deleted"))?;
    anyhow::ensure!(!record.is_retired(), "retired");
    anyhow::ensure!(
        record.runtime_generation == plan.runtime_generation
            && record.app_server_thread_id() == Some(plan.thread_id.as_str()),
        "runtimeRestarted"
    );
    let manager = super::app_server_user_input::session_manager(session);
    let state = manager
        .status(session)?
        .ok_or_else(|| anyhow::anyhow!("offline"))?;
    anyhow::ensure!(
        state.connected && state.runtime_generation == plan.runtime_generation,
        "offline"
    );
    anyhow::ensure!(state.active_turn_id.is_none(), "activeTurn");
    let queue = cutex::management::v2::user_input::user_input_repository()?;
    anyhow::ensure!(
        queue.pause_state(session)?.is_none() && queue.list(session)?.1.is_empty(),
        "ownerQueueOrPaused"
    );
    let asks = AskStore::open_default()?;
    let compatible = cutex::asks::pause_admission::with_state(&asks, |state, _| {
        !state.pauses.contains_key(session)
            && !cutex::asks::agent_pause::is_paused(state, session)
            && !cutex::asks::control::stop_held(state, session)
            && plan.lease.as_ref().is_none_or(|l| match &l.holder {
                LeaseHolder::Ask { ask_id, attempt } => state.asks.get(ask_id).is_some_and(|a| {
                    a.is_open()
                        && a.attempts
                            .iter()
                            .any(|r| r.n == *attempt && !r.stop_requested && !r.state.is_terminal())
                }),
                _ => true,
            })
            && match (&plan.lease, state.leases.get(session)) {
                (None, None) => true,
                (Some(a), Some(b)) => {
                    a.holder == b.holder
                        && b.runtime_generation == plan.runtime_generation
                        && b.turn_id == Some(plan.turn_id.clone())
                }
                _ => false,
            }
    })?;
    anyhow::ensure!(compatible, "pausedOrLeaseChanged");
    let handle = manager.handle_for_generation(session, plan.runtime_generation)?;
    let history = handle.request_with_timeout(
        "thread/read",
        json!({"threadId":plan.thread_id,"includeTurns":true}),
        std::time::Duration::from_secs(5),
    )?;
    anyhow::ensure!(
        latest_failed_turn(&history, plan),
        "newerTurnOrHistoryUnavailable"
    );
    Ok(handle)
}

fn latest_failed_turn(history: &Value, plan: &Plan) -> bool {
    history.pointer("/thread/id").and_then(Value::as_str) == Some(plan.thread_id.as_str())
        && history
            .pointer("/thread/turns")
            .and_then(Value::as_array)
            .and_then(|t| t.last())
            .is_some_and(|t| {
                t["id"].as_str() == Some(plan.turn_id.as_str()) && retry::overloaded(t)
            })
}

fn fire(
    store: &Store,
    asks: &AskStore,
    session: &str,
    pending: &Plan,
    now: chrono::DateTime<Utc>,
    eligibility: impl FnOnce(&Plan) -> anyhow::Result<AppServerHandle>,
) -> anyhow::Result<()> {
    if store.is_disabled() {
        return Ok(());
    }
    let lane = cutex::management::v2::session_coordination::session_lane(session)?;
    let _reservation = match lane.try_lock() {
        Ok(reservation) => reservation,
        Err(cutex::management::v2::session_coordination::ReservationError::WouldBlock) => {
            return Ok(())
        }
        Err(error) => return Err(error.into()),
    };
    if !store.automatic_enabled() {
        return Ok(());
    }
    let Some(plan) = cutex::asks::pause_admission::unless_paused(asks,session,|_|
        store.update(|s| s.claim(session, &pending.plan_id, now)))?.transpose()?.flatten() else {
        return Ok(());
    };
    let handle = match eligibility(&plan) {
        Ok(handle) => handle,
        Err(reason) if reason.downcast_ref::<cutex::asks::pause_admission::Deferred>().is_some()=>{
            postpone_unsent(store,session,&plan)?;
            return Ok(());
        }
        Err(reason) => {
            retry::cancel_with(store, asks, session, &reason.to_string())?;
            retry::publish(session, retry::cancellation_outcome(&reason.to_string()));
            return Ok(());
        }
    };
    // Eligibility may have taken time. Recheck before reopening the native
    // gate, not just before submitting the new retry. No RPC under AskStore.
    match cutex::asks::pause_admission::gate(asks,session) {
        Ok(cutex::asks::pause_admission::Gate::Open)=>{},
        Ok(_)=>{postpone_unsent(store,session,&plan)?;return Ok(());},
        Err(error)=>{postpone_unsent(store,session,&plan)?;return Err(error);},
    }
    // Failure halted the native gate. Resume only this same live occurrence;
    // never runtime/online, never an owner-input auto/queue/steer fallback.
    if let Err(error) = handle.request(
        "thread/externalInput/resume",
        json!({"version":1,"ownerId":session,
        "threadId":plan.thread_id,"runtimeGeneration":plan.runtime_generation}),
    ) {
        retry::cancel_with(store, asks, session, "resumeUnavailable")?;
        retry::publish(session, "failed");
        return Err(error.into());
    }
    send(store, asks, session, &plan, &handle)
}

fn send(
    store: &Store,
    asks: &AskStore,
    session: &str,
    plan: &Plan,
    handle: &AppServerHandle,
) -> anyhow::Result<()> {
    anyhow::ensure!(store.automatic_enabled(), "automatic retry disabled");
    let frozen = envelope(session, plan);
    let mut params = serde_json::to_value(&frozen)?;
    params["admission"] = json!("exclusive_turn");
    // Write-ahead: a crash anywhere after this point is an unknown send, never
    // a pending timer. Query the same key; never submit it a second time.
    let reserved=cutex::asks::pause_admission::unless_paused(asks,session,|_| store.update(|s| {
            let p = s.sessions.get_mut(session)?.plan.as_mut()?;
            if p.message_id != plan.message_id || p.phase != Phase::Firing {
                return None;
            }
            p.phase = Phase::Uncertain;
            p.receipt_check_at = Some((Utc::now() + chrono::Duration::seconds(2)).to_rfc3339());
            Some(())
        }));
    let reserved=match reserved {
        Ok(Some(result))=>result?,
        Ok(None)=>{postpone_unsent(store,session,plan)?;return Ok(());},
        Err(error)=>{postpone_unsent(store,session,plan)?;return Err(error);},
    };
    reserved.ok_or_else(||anyhow::anyhow!("retry superseded before send"))?;
    let result = (|| -> anyhow::Result<String> {
        anyhow::ensure!(
            !store.is_disabled(),
            "automatic retry disabled before native submit"
        );
        let mut raw = handle.request("thread/externalInput/submit", params)?;
        let top_turn = raw.as_object_mut().and_then(|r| r.remove("turnId"));
        let response = validate(raw, &frozen)?;
        let status = response
            .statuses
            .first()
            .ok_or_else(|| anyhow::anyhow!("missing status"))?;
        // Native N1 returns a durable exclusive reservation before A4 context
        // persistence. A validated pending status plus its turnId is a known
        // acceptance; Unknown/Conflict plus a naked turnId never is.
        let turn = match status.delivery_state {
            DeliveryState::ContextPersisted => status.receipt.as_ref().map(|r| r.turn_id.clone()),
            DeliveryState::Pending => top_turn
                .as_ref()
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && id.len() <= 256)
                .map(str::to_owned),
            _ => None,
        }
        .ok_or_else(|| anyhow::anyhow!("exclusive acceptance missing valid turn reservation"))?;
        anyhow::ensure!(
            top_turn
                .as_ref()
                .is_none_or(|id| id.as_str() == Some(turn.as_str())),
            "receipt turn mismatch"
        );
        Ok(turn)
    })();
    match result {
        Ok(turn) => bound(store, asks, session, plan, &turn),
        Err(error) => {
            if let Some(reason) = cutex::app_server::external_input::busy_refusal_reason(&error) {
                store.update(|s| {
                    if let Some(p) = s.sessions.get_mut(session).and_then(|e| e.plan.as_mut()) {
                        if p.message_id == plan.message_id {
                            p.phase = Phase::Firing;
                        }
                    }
                })?;
                retry::cancel_with(store, asks, session, &format!("nativeBusy:{reason}"))?;
                retry::publish(session, "skipped");
                return Ok(());
            }
            store.update(|s| {
                if let Some(e)=s.sessions.get_mut(session) {
                    if e.plan.as_ref().is_some_and(|p|p.message_id==plan.message_id) {
                        e.last=Some(json!({"at":Utc::now().to_rfc3339(),"result":"failed","reason":"outcomeUnknown","attempt":plan.attempt}));
                    }
                }
            })?;
            retry::publish(session, "failed");
            Err(error)
        }
    }
}

/// Firing is a local claim, not a native send. Only this proven-unsent phase
/// may return to Pending; Uncertain/Binding can never be rearmed here.
fn postpone_unsent(store:&Store,session:&str,plan:&Plan)->anyhow::Result<()> {
    store.update(|state|{
        if let Some(current)=state.sessions.get_mut(session).and_then(|e|e.plan.as_mut())
            .filter(|p|p.message_id==plan.message_id&&p.phase==Phase::Firing) {
            current.phase=Phase::Pending;
            current.at=(Utc::now()+chrono::Duration::seconds(2)).to_rfc3339();
        }
    })
}

// Bounded read-only reconciliation. No status, error, or timeout authorizes
// another submission. Check times persist, so repeated ticks do not hot-loop.
fn reconcile(
    store: &Store,
    asks: &AskStore,
    session: &str,
    plan: &Plan,
    now: chrono::DateTime<Utc>,
    handle: impl FnOnce() -> anyhow::Result<AppServerHandle>,
) -> anyhow::Result<()> {
    let lane = cutex::management::v2::session_coordination::session_lane(session)?;
    let _guard = lane.lock()?;
    let claimed = store.update(|s| {
        let p = s.sessions.get_mut(session)?.plan.as_mut()?;
        if p.message_id != plan.message_id || p.phase != Phase::Uncertain {
            return None;
        }
        if p.receipt_check_at
            .as_deref()
            .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
            .is_some_and(|at| at.with_timezone(&Utc) > now)
        {
            return None;
        }
        p.receipt_checks += 1;
        p.receipt_check_at = Some(
            (now + chrono::Duration::seconds((2i64.pow(p.receipt_checks.min(6))).min(60)))
                .to_rfc3339(),
        );
        Some(p.clone())
    })?;
    let Some(p) = claimed else {
        return Ok(());
    };
    let result = handle().and_then(|h| status(&h, &envelope(session, &p)));
    match result {
        Ok(Some(turn)) => bound(store, asks, session, &p, &turn),
        _ if p.receipt_checks >= 8 => {
            retry::cancel_with(store, asks, session, "receiptUnresolved")?;
            retry::publish(session, "failed");
            Ok(())
        }
        Err(e) => Err(e),
        _ => Ok(()),
    }
}

pub(super) fn spawn_worker() -> anyhow::Result<()> {
    let store = Store::open_default()?;
    let asks = AskStore::open_default()?;
    // Never restart a timer or replay a claimed continuation after management
    // restart. Receipt-bound turns remain in normal Ask reconciliation.
    let initial = match store.snapshot() {
        Ok(state) => state,
        Err(error) => {
            eprintln!("model overload retry disabled: unreadable journal: {error:#}");
            return Ok(());
        }
    };
    for session in initial.sessions.keys() {
        if let Some(p) = initial.sessions[session]
            .plan
            .as_ref()
            .filter(|p| p.phase == Phase::Binding)
        {
            if let Some(turn) = &p.binding_turn_id {
                bound(&store, &asks, session, p, turn)?;
            }
        }
        retry::cancel_with(&store, &asks, session, "managementRestarted")?;
        retry::publish(session, "cancelled");
    }
    std::thread::Builder::new()
        .name("cutex-model-overload".into())
        .spawn(move || loop {
            if let Ok(state) = store.snapshot() {
                for (session, entry) in state.sessions {
                    if entry.settlement.is_some() {
                        if let Err(error) = retry::recover_settlement(&store, &asks, &session) {
                            eprintln!("retry settlement: {error:#}");
                        }
                        continue;
                    }
                    let Some(plan) = entry.plan else {
                        continue;
                    };
                    let result = match plan.phase {
                        Phase::Pending => fire(&store, &asks, &session, &plan, Utc::now(), |p| {
                            eligible(&session, p)
                        }),
                        Phase::Uncertain => {
                            reconcile(&store, &asks, &session, &plan, Utc::now(), || {
                                let manager =
                                    super::app_server_user_input::session_manager(&session);
                                Ok(manager
                                    .handle_for_generation(&session, plan.runtime_generation)?)
                            })
                        }
                        Phase::Binding => match plan.binding_turn_id.as_deref() {
                            Some(turn) => bound(&store, &asks, &session, &plan, turn),
                            None => Err(anyhow::anyhow!("binding turn missing")),
                        },
                        Phase::Firing | Phase::Running => Ok(()),
                    };
                    if let Err(error) = result {
                        eprintln!("model overload retry {session}: {error:#}");
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        })?;
    Ok(())
}

#[cfg(test)]
#[path = "model_overload_retry_tests.rs"]
mod tests;
