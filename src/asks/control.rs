//! Owner and requester control of asks (asks design §5.10).
//!
//! - **Owner stop** (`cutex/turn/stop`), owner decision 2026-10-12
//!   ("停止 = everything waits", replaces the r208 rule that a stop never
//!   pauses delegation): ends the session's `delegation_wait` calls and
//!   records a **stop hold**, an [`AdmissionPause`] with origin `ownerStop`
//!   and `stopHold: true` (an existing explicit pause just gains
//!   `stopHold`). While it exists no ask is admitted, no ANSWER/QUESTION
//!   event is delivered and no ride-along delivery (bus wake, tell, alarm)
//!   starts a turn (`crate::management::v2::admission::delivery_may_start`).
//!   The native external-input gate the interrupt closed stays closed, so
//!   items native already accepted wait too; every release reopens it.
//!   Released by `cutex/userInput/queue/resume` ([`release_stop_hold`]),
//!   `cutex/asks/resume`, or the owner's next message that starts or steers
//!   a turn (a message the owner only queues releases nothing). Durable in
//!   the ask store, so it survives a daemon restart.
//! - **Owner delegation pause** (`cutex/asks/pause`): the session admits no
//!   asks (queued ones show `paused(owner)`) and receives no ANSWER/QUESTION
//!   events; auto-wake never undoes it. Ride-along bus deliveries continue.
//! - Both are cleared by `cutex/asks/resume`, or by the owner's next message
//!   to that session (`cutex/userInput/submit` started / steered / queued;
//!   `queued` does not clear a stop hold). Every change publishes
//!   `cutex/asks/pauseChanged` on the session stream.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use super::model::*;
use super::store::{AskState, AskStore, QueuedCommand, RuntimeCommand};
use super::{new_id, now_rfc3339};

/// Session event for every change of the delegation pause.
pub const PAUSE_CHANGED_EVENT: &str = "cutex/asks/pauseChanged";
/// [`AdmissionPause::origin`] of an explicit owner pause.
pub const ORIGIN_OWNER_PAUSE: &str = "ownerPause";
/// [`AdmissionPause::origin`] of the pause an owner turn stop creates.
pub const ORIGIN_OWNER_STOP: &str = "ownerStop";
/// `held` of a ride-along the owner stop holds (admission `nextStep`).
pub const HELD_OWNER_STOP: &str = "ownerStop";
/// [`AdmissionPause::origin`] of the pause a closed native input gate
/// creates (Claude: the adapter halted its queue after an interrupted or
/// failed turn; Codex: the N2 external-input gate an interrupt closed).
pub const ORIGIN_RUNTIME_GATE: &str = "runtimeGate";
/// `heldByRuntime.native` of asks waiting on a closed gate.
pub const NATIVE_GATE_CLOSED: &str = "gate_closed";

pub fn queue_command(state: &mut AskState, command: RuntimeCommand, now: DateTime<Utc>) {
    if state.commands.iter().any(|queued| queued.command == command) {
        return;
    }
    state.commands.push(QueuedCommand { command_id: new_id("cmd"), command, created_at: now_rfc3339(now) });
}

/// `asksPaused` as clients see it: `{since, by, origin, stopHold}` or `null`.
pub fn pause_json(pause: Option<&AdmissionPause>) -> Value {
    match pause {
        Some(pause) => json!({
            "since": pause.since,
            "by": pause.by,
            "origin": pause.origin.as_deref().unwrap_or("legacy"),
            "stopHold": pause.stop_hold,
        }),
        None => Value::Null,
    }
}

fn pause_changed_notice(state: &mut AskState, session: &str, reason: &str) {
    let paused = pause_json(state.pauses.get(session));
    state.notice(PAUSE_CHANGED_EVENT, vec![session.to_string()], json!({"asksPaused": paused, "reason": reason}));
}

/// Whether an owner stop holds the ride-alongs of `session`.
pub fn stop_held(state: &AskState, session: &str) -> bool {
    state.pauses.get(session).is_some_and(|pause| pause.stop_hold)
}

/// Record an owner pause on `session`'s admission and hold its queued asks.
/// Returns whether the pause was created now.
pub fn owner_pause(state: &mut AskState, session: &str, now: DateTime<Utc>) -> bool {
    pause_with(state, session, ORIGIN_OWNER_PAUSE, false, None, now)
}

/// The hold of a queued ask under `pause`: a closed runtime gate reads as
/// `heldByRuntime(gate_closed)`, every other pause as `paused(owner)`.
pub fn pause_hold(pause: &AdmissionPause) -> Hold {
    if pause.origin.as_deref() == Some(ORIGIN_RUNTIME_GATE) {
        Hold::HeldByRuntime { native: Some(NATIVE_GATE_CLOSED.into()) }
    } else {
        Hold::Paused { by: pause.by }
    }
}

/// Whether `session`'s admission waits on a closed native gate (origin
/// `runtimeGate`).
pub fn runtime_gate_closed(state: &AskState, session: &str) -> bool {
    state.pauses.get(session).is_some_and(|pause| pause.origin.as_deref() == Some(ORIGIN_RUNTIME_GATE))
}

/// The runtime (generation `generation`) refused a delivery because its
/// input gate is closed (the last turn was interrupted or failed; tool
/// effects may be uncertain). Record it as a pause the owner sees and
/// releases (`cutex/asks/resume`, `cutex/userInput/queue/resume` or the
/// owner's next message): until then no ask or ask result is retried, and
/// the owner's own input is not queued behind them. An existing pause stays
/// as it is (every owner release reopens the gate anyway). Returns whether
/// the pause was created now.
pub fn runtime_gate_pause(state: &mut AskState, session: &str, generation: u64, now: DateTime<Utc>) -> bool {
    if state.pauses.contains_key(session) {
        return false;
    }
    pause_with(state, session, ORIGIN_RUNTIME_GATE, false, Some(generation), now)
}

/// A `runtimeGate` pause of `session` ends without an owner action: the
/// runtime restarted (`generation` differs) or the gate demonstrably opened
/// (a turn started on a runtime that only starts turns with an open gate).
/// Returns whether a pause ended.
pub fn runtime_gate_opened(state: &mut AskState, session: &str, generation: Option<u64>, reason: &str, now: DateTime<Utc>) -> bool {
    let Some(pause) = state.pauses.get(session).filter(|pause| pause.origin.as_deref() == Some(ORIGIN_RUNTIME_GATE)) else {
        return false;
    };
    if generation.is_some_and(|generation| pause.runtime_generation == Some(generation)) {
        return false;
    }
    state.pauses.remove(session);
    clear_pause_holds(state, session, now);
    pause_changed_notice(state, session, reason);
    true
}

fn clear_pause_holds(state: &mut AskState, session: &str, now: DateTime<Utc>) {
    let gate_hold = Some(Hold::HeldByRuntime { native: Some(NATIVE_GATE_CLOSED.into()) });
    let held: Vec<String> = state
        .waiting_for_target(session)
        .iter()
        .filter(|ask| matches!(ask.hold, Some(Hold::Paused { by: PausedBy::Owner })) || ask.hold == gate_hold)
        .map(|ask| ask.ask_id.clone())
        .collect();
    for ask_id in held {
        state.update_ask(&ask_id, now, |ask| ask.hold = None);
    }
}

fn pause_with(state: &mut AskState, session: &str, origin: &str, stop: bool, generation: Option<u64>, now: DateTime<Utc>) -> bool {
    let created = !state.pauses.contains_key(session);
    let pause = state.pauses.entry(session.to_string()).or_insert_with(|| AdmissionPause {
        cutex_session_id: session.to_string(),
        by: PausedBy::Owner,
        since: now_rfc3339(now),
        origin: Some(origin.to_string()),
        stop_hold: false,
        runtime_generation: generation,
    });
    let stop_added = stop && !pause.stop_hold;
    pause.stop_hold |= stop;
    let paused_hold = pause_hold(pause);
    let queued: Vec<String> = state
        .waiting_for_target(session)
        .iter()
        .filter(|ask| ask.execution == ExecutionState::Queued)
        .map(|ask| ask.ask_id.clone())
        .collect();
    for ask_id in queued {
        let hold = Some(paused_hold.clone());
        let changed = state.update_ask(&ask_id, now, |ask| {
            let changed = ask.hold != hold;
            ask.hold = hold.clone();
            changed
        });
        if changed == Some(true) {
            if let Some(ask) = state.asks.get(&ask_id).cloned() {
                super::delivery::record_state_delivery(state, &ask, "paused", now);
            }
        }
    }
    if created || stop_added {
        let reason = if stop {
            ORIGIN_OWNER_STOP
        } else if origin == ORIGIN_RUNTIME_GATE {
            ORIGIN_RUNTIME_GATE
        } else {
            "paused"
        };
        pause_changed_notice(state, session, reason);
    }
    created
}

/// Clear an owner pause; queue the N2 gate resume. Returns whether a pause
/// existed. `reason`: `resumed` (explicit) or `ownerMessage`.
pub fn owner_resume(state: &mut AskState, session: &str, reason: &str, now: DateTime<Utc>) -> bool {
    let existed = state.pauses.remove(session).is_some();
    // Reopen the native gate even without a cutex pause: an interrupt from
    // another path may have closed it.
    queue_command(state, RuntimeCommand::ResumeGate { cutex_session_id: session.to_string() }, now);
    clear_pause_holds(state, session, now);
    if existed {
        pause_changed_notice(state, session, reason);
    }
    existed
}

/// Owner stop of a turn (`cutex/turn/stop`): end the session's waits and
/// hold everything that could start a turn (owner decision 2026-10-12). The
/// native gate the interrupt closes is left closed; a release reopens it.
pub fn owner_stop(state: &mut AskState, session: &str, now: DateTime<Utc>) {
    super::waits::cancel_waits(state, session);
    // A stop supersedes a closed-gate pause: it is the owner's own hold now.
    if runtime_gate_closed(state, session) {
        if let Some(pause) = state.pauses.get_mut(session) {
            pause.origin = Some(ORIGIN_OWNER_STOP.into());
            pause.runtime_generation = None;
        }
    }
    pause_with(state, session, ORIGIN_OWNER_STOP, true, None, now);
}

/// Release a stop hold (`cutex/userInput/queue/resume`): an `ownerStop`
/// pause ends; an explicit owner pause the owner then stopped stays, only
/// its stop hold ends. Reopens the native gate. Returns whether a hold
/// existed.
pub fn release_stop_hold(state: &mut AskState, session: &str, reason: &str, now: DateTime<Utc>) -> bool {
    // The owner resuming the queue also acknowledges a closed runtime gate.
    if runtime_gate_closed(state, session) {
        return owner_resume(state, session, reason, now);
    }
    let Some(pause) = state.pauses.get_mut(session).filter(|pause| pause.stop_hold) else {
        return false;
    };
    if pause.origin.as_deref() == Some(ORIGIN_OWNER_STOP) {
        return owner_resume(state, session, reason, now);
    }
    pause.stop_hold = false;
    queue_command(state, RuntimeCommand::ResumeGate { cutex_session_id: session.to_string() }, now);
    pause_changed_notice(state, session, reason);
    true
}

/// The owner's message to `session` landed with `disposition`
/// (`started` / `steered` / `queued`). A delegation pause ends; a stop hold
/// ends only with a message that started or steered a turn.
pub fn owner_message(state: &mut AskState, session: &str, disposition: &str, now: DateTime<Utc>) -> bool {
    if disposition == "queued" && stop_held(state, session) {
        return false;
    }
    owner_resume(state, session, "ownerMessage", now)
}

/// Best-effort hook for `cutex/turn/stop`: a store failure is logged, never
/// fails the stop.
pub fn owner_stop_default(session: &str) {
    let Ok(store) = AskStore::open_default() else {
        return;
    };
    match store.with_state(|state| owner_stop(state, session, Utc::now())) {
        Ok(()) => crate::management::v2::asks::publish_pending_notices(&store),
        Err(error) => eprintln!("asks: could not record the owner stop for {session}: {error:#}"),
    }
}

/// Best-effort hook for `cutex/userInput/queue/resume`: release the stop
/// hold of `session`. Cheap when there is none; never creates the store.
pub fn release_stop_hold_default(session: &str) -> bool {
    let Ok(store) = AskStore::open_default() else {
        return false;
    };
    let held = store.inspect(|state| stop_held(state, session) || runtime_gate_closed(state, session)).unwrap_or(false);
    if !held {
        return false;
    }
    match store.with_state(|state| release_stop_hold(state, session, "resumed", Utc::now())) {
        Ok(released) => {
            if released {
                crate::management::v2::asks::publish_pending_notices(&store);
            }
            released
        }
        Err(error) => {
            eprintln!("asks: could not release the stop hold of {session}: {error:#}");
            false
        }
    }
}

/// A turn started on `session`'s Claude runtime: the adapter starts turns
/// only with an open input gate, so a `runtimeGate` pause ends (reason
/// `turnStarted`). Cheap when there is none; never creates the store.
pub fn runtime_gate_turn_started_default(session: &str) -> bool {
    let Ok(store) = AskStore::open_default() else {
        return false;
    };
    if !store.inspect(|state| runtime_gate_closed(state, session)).unwrap_or(false) {
        return false;
    }
    match store.with_state(|state| runtime_gate_opened(state, session, None, "turnStarted", Utc::now())) {
        Ok(ended) => {
            if ended {
                crate::management::v2::asks::publish_pending_notices(&store);
            }
            ended
        }
        Err(error) => {
            eprintln!("asks: could not end the runtime gate pause of {session}: {error:#}");
            false
        }
    }
}

/// `cutex/asks/pause`: `{asksPaused, changed}`.
pub fn owner_pause_in_store(store: &AskStore, session: &str, now: DateTime<Utc>) -> anyhow::Result<Value> {
    store.with_state(|state| {
        super::waits::cancel_waits(state, session);
        let changed = owner_pause(state, session, now);
        json!({"asksPaused": pause_json(state.pauses.get(session)), "changed": changed})
    })
}

/// `cutex/asks/resume`: `{asksPaused: null, changed}`.
pub fn owner_resume_in_store(store: &AskStore, session: &str, now: DateTime<Utc>) -> anyhow::Result<Value> {
    store.with_state(|state| {
        let changed = owner_resume(state, session, "resumed", now);
        json!({"asksPaused": Value::Null, "changed": changed})
    })
}

/// The owner submitted a message to `session` (`disposition`: started,
/// steered or queued): a delegation pause ends ([`owner_message`]). Cheap
/// when there is none; never creates the store. Returns whether a pause was
/// cleared.
pub fn owner_message_default(session: &str, disposition: &str) -> bool {
    let Ok(store) = AskStore::open_default() else {
        return false;
    };
    match owner_message_in_store(&store, session, disposition) {
        Ok(cleared) => {
            if cleared {
                crate::management::v2::asks::publish_pending_notices(&store);
            }
            cleared
        }
        Err(error) => {
            eprintln!("asks: could not clear the delegation pause of {session}: {error:#}");
            false
        }
    }
}

fn owner_message_in_store(store: &AskStore, session: &str, disposition: &str) -> anyhow::Result<bool> {
    // Most submissions have no pause to release. Inspect only the relevant
    // pause, without cloning the full ask history under the shared lock.
    // A queued message also cannot release a stop hold, so avoid an exclusive
    // transaction (and its full-state work) for that known no-op.
    let needs_resume = store
        .inspect(|state| state.pauses.contains_key(session) && !(disposition == "queued" && stop_held(state, session)))?;
    if !needs_resume {
        return Ok(false);
    }
    // Recheck the current stop hold inside the mutation: another process may
    // have stopped the session since the read above.
    store.with_state(|state| owner_message(state, session, disposition, Utc::now()))
}

/// Whether the owner holds `session` (a delegation pause or stop hold; a
/// closed runtime gate is not the owner's hold). Never creates the store.
pub fn owner_paused_default(session: &str) -> bool {
    AskStore::open_default()
        .ok()
        .and_then(|store| store.inspect(|state| {
            state.pauses.get(session).is_some_and(|pause| pause.origin.as_deref() != Some(ORIGIN_RUNTIME_GATE))
        }).ok())
        .unwrap_or(false)
}

/// `asksPaused` of a session for queue/list and bootstrap (never creates
/// the store; `null` when there is none).
pub fn pause_state_default(session: &str) -> Value {
    AskStore::open_default()
        .ok()
        .and_then(|store| store.inspect(|state| pause_json(state.pauses.get(session))).ok())
        .unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::store::tests::{new_ask, temp_store, TARGET};

    #[test]
    fn owner_message_probe_leaves_absent_store_absent() {
        let (store, dir) = temp_store("owner-message-absent");
        assert!(!owner_message_in_store(&store, TARGET, "started").unwrap());
        assert!(!dir.exists(), "ordinary owner input must not create ask state or a lock");
    }

    #[test]
    fn queued_owner_message_preserves_stop_hold_without_mutating_store() {
        let (store, dir) = temp_store("owner-message-held");
        store.with_state(|state| owner_stop(state, TARGET, Utc::now())).unwrap();
        let before = std::fs::read(dir.join("asks-v1.json")).unwrap();
        assert!(!owner_message_in_store(&store, TARGET, "queued").unwrap());
        assert_eq!(std::fs::read(dir.join("asks-v1.json")).unwrap(), before);
        let state = store.snapshot().unwrap();
        assert!(stop_held(&state, TARGET));
        assert!(state.commands.is_empty());
        // A started message still releases the hold and queues the native gate.
        assert!(owner_message_in_store(&store, TARGET, "started").unwrap());
        let state = store.snapshot().unwrap();
        assert!(!stop_held(&state, TARGET));
        assert!(state.commands.iter().any(|queued| queued.command ==
            RuntimeCommand::ResumeGate { cutex_session_id: TARGET.into() }));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// An explicit owner pause holds queued asks (also ones created later);
    /// resume clears it and reopens the native gate; both publish
    /// `cutex/asks/pauseChanged` once.
    #[test]
    fn owner_pause_holds_asks_until_resumed() {
        let mut state = AskState::default();
        let now = Utc::now();
        assert!(owner_pause(&mut state, TARGET, now));
        assert!(!owner_pause(&mut state, TARGET, now));
        assert_eq!(state.pauses[TARGET].origin.as_deref(), Some(ORIGIN_OWNER_PAUSE));
        let (ask, _) = state.create_ask(new_ask("a", "x"), now).unwrap();
        owner_pause(&mut state, TARGET, now);
        assert_eq!(state.asks[&ask.ask_id].hold, Some(Hold::Paused { by: PausedBy::Owner }));
        assert!(owner_resume(&mut state, TARGET, "resumed", now));
        assert_eq!(state.asks[&ask.ask_id].hold, None);
        assert_eq!(
            state.commands[0].command,
            RuntimeCommand::ResumeGate { cutex_session_id: TARGET.into() }
        );
        assert!(!owner_resume(&mut state, TARGET, "resumed", now));
        assert_eq!(state.commands.len(), 1, "deduplicated");
        let changes: Vec<&Value> = state
            .notices
            .iter()
            .filter(|notice| notice.method == PAUSE_CHANGED_EVENT)
            .map(|notice| &notice.params)
            .collect();
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0]["asksPaused"]["origin"], json!(ORIGIN_OWNER_PAUSE));
        assert_eq!(changes[0]["reason"], json!("paused"));
        assert_eq!(changes[1]["asksPaused"], Value::Null);
    }

    /// Owner decision 2026-10-12 ("停止 = everything waits"): a stop holds
    /// asks and every ride-along, leaves the native gate closed, survives
    /// a message the owner only queues, and is released by queue resume
    /// (which reopens the gate) or by an owner message that starts a turn.
    #[test]
    fn owner_stop_holds_everything_until_resume_or_owner_message() {
        let mut state = AskState::default();
        let now = Utc::now();
        let (ask, _) = state.create_ask(new_ask("a", "x"), now).unwrap();
        owner_stop(&mut state, TARGET, now);
        let pause = &state.pauses[TARGET];
        assert_eq!(pause.origin.as_deref(), Some(ORIGIN_OWNER_STOP));
        assert!(pause.stop_hold && stop_held(&state, TARGET));
        assert_eq!(state.asks[&ask.ask_id].hold, Some(Hold::Paused { by: PausedBy::Owner }));
        assert!(state.commands.is_empty(), "the gate the interrupt closed stays closed");
        assert_eq!(pause_json(state.pauses.get(TARGET))["stopHold"], json!(true));
        // A second stop changes nothing; a queued owner message releases nothing.
        owner_stop(&mut state, TARGET, now);
        assert!(!owner_message(&mut state, TARGET, "queued", now));
        assert!(stop_held(&state, TARGET));
        // Queue resume releases it and reopens the gate.
        assert!(release_stop_hold(&mut state, TARGET, "resumed", now));
        assert!(state.pauses.is_empty());
        assert_eq!(state.asks[&ask.ask_id].hold, None);
        assert_eq!(state.commands[0].command, RuntimeCommand::ResumeGate { cutex_session_id: TARGET.into() });
        assert!(!release_stop_hold(&mut state, TARGET, "resumed", now));
        // The owner's next message that starts (or steers) a turn releases it too.
        for disposition in ["started", "steered"] {
            owner_stop(&mut state, TARGET, now);
            assert!(owner_message(&mut state, TARGET, disposition, now));
            assert!(!stop_held(&state, TARGET));
        }
        let reasons: Vec<&Value> = state.notices.iter()
            .filter(|notice| notice.method == PAUSE_CHANGED_EVENT)
            .map(|notice| &notice.params["reason"]).collect();
        assert_eq!(reasons, [&json!("ownerStop"), &json!("resumed"), &json!("ownerStop"),
            &json!("ownerMessage"), &json!("ownerStop"), &json!("ownerMessage")]);
        for notice in state.notices.iter().filter(|notice| notice.method == PAUSE_CHANGED_EVENT) {
            crate::management::v2::contract_validation::validate_cutex_event_message(
                &json!({"method": notice.method, "params": notice.params}),
            ).expect("pauseChanged matches the schema");
        }
    }

    /// A closed runtime gate pause: created once (never over an owner
    /// pause), released by queue resume (gate reopened), superseded by a
    /// stop, ended by an observed turn start without a gate resume.
    #[test]
    fn runtime_gate_pause_lifecycle() {
        let mut state = AskState::default();
        let now = Utc::now();
        let (ask, _) = state.create_ask(new_ask("a", "x"), now).unwrap();
        assert!(runtime_gate_pause(&mut state, TARGET, 4, now));
        assert!(!runtime_gate_pause(&mut state, TARGET, 4, now));
        assert!(runtime_gate_closed(&state, TARGET));
        assert_eq!(state.asks[&ask.ask_id].hold, Some(Hold::HeldByRuntime { native: Some(NATIVE_GATE_CLOSED.into()) }));
        assert_eq!(pause_json(state.pauses.get(TARGET))["origin"], json!(ORIGIN_RUNTIME_GATE));
        assert_eq!(pause_json(state.pauses.get(TARGET))["stopHold"], json!(false));
        // The same generation keeps it; queue resume releases it and reopens the gate.
        assert!(!runtime_gate_opened(&mut state, TARGET, Some(4), "runtimeRestart", now));
        assert!(release_stop_hold(&mut state, TARGET, "resumed", now));
        assert!(state.pauses.is_empty());
        assert_eq!(state.asks[&ask.ask_id].hold, None);
        assert_eq!(state.commands.len(), 1);
        // A stop takes it over (owner hold, gate stays closed).
        runtime_gate_pause(&mut state, TARGET, 4, now);
        owner_stop(&mut state, TARGET, now);
        assert!(!runtime_gate_closed(&state, TARGET) && stop_held(&state, TARGET));
        assert_eq!(state.asks[&ask.ask_id].hold, Some(Hold::Paused { by: PausedBy::Owner }));
        assert!(!runtime_gate_opened(&mut state, TARGET, None, "turnStarted", now), "never ends an owner hold");
        owner_resume(&mut state, TARGET, "resumed", now);
        // An owner pause is never replaced by a gate pause.
        owner_pause(&mut state, TARGET, now);
        assert!(!runtime_gate_pause(&mut state, TARGET, 4, now));
        owner_resume(&mut state, TARGET, "resumed", now);
        // A turn start ends it without a gate command.
        state.commands.clear();
        runtime_gate_pause(&mut state, TARGET, 4, now);
        assert!(runtime_gate_opened(&mut state, TARGET, None, "turnStarted", now));
        assert!(state.pauses.is_empty() && state.commands.is_empty());
        assert_eq!(state.asks[&ask.ask_id].hold, None);
        for notice in state.notices.iter().filter(|notice| notice.method == PAUSE_CHANGED_EVENT) {
            crate::management::v2::contract_validation::validate_cutex_event_message(
                &json!({"method": notice.method, "params": notice.params}),
            ).expect("pauseChanged matches the schema");
        }
    }

    /// A stop on an explicit delegation pause adds the stop hold; queue
    /// resume ends only the stop hold, the explicit pause stays.
    #[test]
    fn owner_stop_on_an_explicit_pause_keeps_it_after_queue_resume() {
        let mut state = AskState::default();
        let now = Utc::now();
        owner_pause(&mut state, TARGET, now);
        assert!(!stop_held(&state, TARGET));
        owner_stop(&mut state, TARGET, now);
        assert_eq!(state.pauses[TARGET].origin.as_deref(), Some(ORIGIN_OWNER_PAUSE));
        assert!(stop_held(&state, TARGET));
        assert!(release_stop_hold(&mut state, TARGET, "resumed", now));
        assert!(!stop_held(&state, TARGET));
        assert!(state.pauses.contains_key(TARGET));
        assert_eq!(state.commands.len(), 1);
        // A legacy pause (no origin) reads as `legacy`.
        state.pauses.insert(
            TARGET.into(),
            AdmissionPause { cutex_session_id: TARGET.into(), by: PausedBy::Owner, since: "t".into(), origin: None, stop_hold: false, runtime_generation: None },
        );
        assert_eq!(pause_json(state.pauses.get(TARGET))["origin"], json!("legacy"));
    }
}
