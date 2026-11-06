//! Agent pause at the next tool boundary (r276, docs/agent-pause.md).
//!
//! The pause itself stays the durable admission gate in AskState
//! (`asks::agent_pause`). This module tells the live runtime about it: a
//! runtime that offers a tool gate (claude-r37 `claude/toolGate/set`;
//! cute-codex `toolGateVersion` 1, bound `thread/toolGate/set`) holds the
//! agent's next tool call until resume,
//! inside the running turn. Level-triggered on the scheduler tick: every
//! online session's wanted state (paused or not) is pushed once per runtime
//! generation, so pause, resume, a reconnect and a daemon restart all
//! converge without per-route plumbing. Held calls come back as
//! `claude/toolGate/changed` / `thread/toolGate/changed` and are kept on the
//! pause (`held`). DSH/Kimi have no gate: they pause at the end of the turn.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::Utc;
use cutex::asks::store::{AskStore, HeldToolCall};
use serde_json::Value;
#[cfg(test)]
use serde_json::json;

pub(crate) const CLAUDE_SET: &str = "claude/toolGate/set";
pub(crate) const CLAUDE_CHANGED: &str = "claude/toolGate/changed";
pub(crate) const CODEX_CHANGED: &str = "thread/toolGate/changed";
const RETRY: Duration = Duration::from_secs(10);

#[derive(Default)]
struct Pushed {
    /// session -> (runtime generation, held) the runtime last confirmed.
    confirmed: HashMap<String, (Option<u64>, bool)>,
    /// session -> when a failed push may be tried again.
    retry_at: HashMap<String, Instant>,
}

fn pushed() -> &'static Mutex<Pushed> {
    static PUSHED: OnceLock<Mutex<Pushed>> = OnceLock::new();
    PUSHED.get_or_init(Default::default)
}

/// What one push did: the runtime has a gate and was set, or it has none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Push {
    Set,
    Unsupported,
}

/// One scheduler tick. Reads only the cached presence rows; a runtime is
/// contacted only when its wanted state or generation changed.
pub(crate) fn reconcile(store: &AskStore, runtime: &super::asks_runtime::ManagementAskRuntime) -> anyhow::Result<()> {
    reconcile_with(store, &mut |session, held| push(runtime, session, held), Instant::now())
}

pub(crate) fn reconcile_with(
    store: &AskStore,
    push: &mut dyn FnMut(&str, bool) -> anyhow::Result<Push>,
    now: Instant,
) -> anyhow::Result<()> {
    let rows: Vec<(String, Option<u64>, bool)> = store.inspect(|state| {
        state.presence.iter()
            .filter(|(_, presence)| presence.online)
            .map(|(session, presence)| (session.clone(), presence.runtime_generation, presence.agent_pause.is_some()))
            .collect()
    })?;
    let mut pushed = pushed().lock().map_err(|_| anyhow::anyhow!("tool gate state poisoned"))?;
    // Offline or gone: forget, the next runtime occurrence is pushed afresh.
    pushed.confirmed.retain(|session, _| rows.iter().any(|row| &row.0 == session));
    for (session, generation, want) in rows {
        let previous = pushed.confirmed.get(&session).copied();
        if previous == Some((generation, want)) {
            continue;
        }
        if pushed.retry_at.get(&session).is_some_and(|at| *at > now) {
            continue;
        }
        match push(&session, want) {
            Ok(outcome) => {
                pushed.retry_at.remove(&session);
                pushed.confirmed.insert(session.clone(), (generation, want));
                let new_occurrence = previous.is_none_or(|(old, _)| old != generation);
                if want || new_occurrence {
                    store.with_state(|state| {
                        // A new runtime occurrence holds nothing yet.
                        if new_occurrence {
                            cutex::asks::agent_pause::note_held(state, &session, Vec::new(), Utc::now());
                        }
                        cutex::asks::agent_pause::note_gate(state, &session, outcome == Push::Set);
                    })?;
                }
            }
            Err(error) => {
                eprintln!("agent pause: tool gate of {session} not set: {error:#}");
                pushed.retry_at.insert(session, now + RETRY);
            }
        }
    }
    Ok(())
}

fn push(runtime: &super::asks_runtime::ManagementAskRuntime, session: &str, held: bool) -> anyhow::Result<Push> {
    let Some(record) = super::asks_runtime::record(session) else {
        return Ok(Push::Unsupported);
    };
    if record.claude_session.is_some() {
        return super::asks_claude::set_tool_gate(session, held);
    }
    if record.dsh_session.is_some() {
        // DSH / Kimi: no tool gate; the pause takes effect at the end of the turn.
        return Ok(Push::Unsupported);
    }
    runtime.set_native_tool_gate(session, held)
}

/// The held calls of a `claude/toolGate/changed` / `thread/toolGate/changed` for this session's
/// bound thread. A Codex sub-agent thread's call (its own `threadId`) keeps that id as `agentId`.
pub(crate) fn held_from(thread_id: &str, params: &Value) -> Option<Vec<HeldToolCall>> {
    if params.get("threadId").and_then(Value::as_str) != Some(thread_id) {
        return None;
    }
    let rows = params.get("waiting")?.as_array()?;
    Some(rows.iter().filter_map(|row| {
        let text = |key: &str| row.get(key).and_then(Value::as_str).filter(|v| !v.is_empty() && v.len() <= 256).map(str::to_string);
        let since = row.get("sinceMs").and_then(Value::as_i64)
            .and_then(chrono::DateTime::from_timestamp_millis)
            .unwrap_or_else(Utc::now);
        Some(HeldToolCall {
            call_id: text("callId")?,
            tool: text("tool")?,
            turn_id: text("turnId"),
            agent_id: text("agentId").or_else(|| text("threadId").filter(|id| id != thread_id)),
            since: since.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        })
    }).take(cutex::asks::agent_pause::MAX_HELD).collect())
}

/// Runtime event: keep the held calls on the pause.
pub(crate) fn note_changed(session: &str, thread_id: &str, params: Option<&Value>) {
    let Some(held) = params.and_then(|params| held_from(thread_id, params)) else {
        return;
    };
    let result = AskStore::open_default().and_then(|store| store.with_state(|state| {
        cutex::asks::agent_pause::note_held(state, session, held, Utc::now())
    }));
    if let Err(error) = result {
        eprintln!("agent pause: held tool calls of {session} not recorded: {error:#}");
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use cutex::asks::store::{AgentPausePhase, Presence};

    const S: &str = "cutex.gate";

    fn store(name: &str) -> AskStore {
        AskStore::at(std::env::temp_dir().join(format!("cutex-{name}-{}", uuid::Uuid::new_v4())))
    }

    fn online(store: &AskStore, generation: u64, busy: bool) {
        store.with_state(|state| {
            let presence = state.presence.entry(S.into()).or_insert_with(Presence::default);
            presence.online = true;
            presence.busy = busy;
            presence.runtime_generation = Some(generation);
        }).unwrap();
    }

    fn pause(store: &AskStore) -> serde_json::Value {
        store.with_state(|state| cutex::asks::agent_pause::pause(state, S, "owner", None, Utc::now())).unwrap()
    }

    fn agent_pause(store: &AskStore) -> Option<cutex::asks::store::AgentPause> {
        store.inspect(|state| state.presence.get(S).and_then(|p| p.agent_pause.clone())).unwrap()
    }

    fn reset() {
        let mut pushed = pushed().lock().unwrap();
        pushed.confirmed.remove(S);
        pushed.retry_at.remove(S);
    }

    #[test]
    fn pause_resume_and_reconnect_push_once_per_state_and_generation() {
        let _serial = serial();
        reset();
        let store = store("tool-gate-push");
        online(&store, 1, true);
        let calls = std::cell::RefCell::new(Vec::new());
        let mut push = |session: &str, held: bool| { calls.borrow_mut().push((session.to_string(), held)); Ok(Push::Set) };
        let t = Instant::now();
        // Daemon start: the not-paused state is pushed once (opens a gate a
        // resume might have missed while cutex was down).
        reconcile_with(&store, &mut push, t).unwrap();
        reconcile_with(&store, &mut push, t).unwrap();
        assert_eq!(*calls.borrow(), vec![(S.to_string(), false)]);
        pause(&store);
        reconcile_with(&store, &mut push, t).unwrap();
        reconcile_with(&store, &mut push, t).unwrap();
        assert_eq!(calls.borrow().last().unwrap(), &(S.to_string(), true));
        assert_eq!(calls.borrow().len(), 2);
        let state = agent_pause(&store).unwrap();
        assert_eq!((state.tool_gate, state.at), (Some(true), Some(cutex::asks::store::AgentPauseAt::ToolBoundary)));
        assert_eq!(state.phase, AgentPausePhase::Pausing, "a running turn pauses at its next tool call");
        // Reconnect: a new runtime generation gets the state again.
        online(&store, 2, true);
        reconcile_with(&store, &mut push, t).unwrap();
        assert_eq!(calls.borrow().len(), 3);
        store.with_state(|state| cutex::asks::agent_pause::resume(state, S)).unwrap();
        reconcile_with(&store, &mut push, t).unwrap();
        assert_eq!(calls.borrow().last().unwrap(), &(S.to_string(), false));
        assert_eq!(calls.borrow().len(), 4);
    }

    #[test]
    fn runtime_without_gate_pauses_at_turn_end_and_failures_retry_later() {
        let _serial = serial();
        reset();
        let store = store("tool-gate-unsupported");
        online(&store, 7, true);
        pause(&store);
        let t = Instant::now();
        let mut failing = |_: &str, _: bool| -> anyhow::Result<Push> { anyhow::bail!("transport") };
        reconcile_with(&store, &mut failing, t).unwrap();
        assert_eq!(agent_pause(&store).unwrap().tool_gate, None);
        let count = std::cell::Cell::new(0);
        let mut unsupported = |_: &str, _: bool| { count.set(count.get() + 1); Ok(Push::Unsupported) };
        reconcile_with(&store, &mut unsupported, t + Duration::from_secs(1)).unwrap();
        assert_eq!(count.get(), 0, "a failed push waits for its retry time");
        reconcile_with(&store, &mut unsupported, t + RETRY + Duration::from_secs(1)).unwrap();
        assert_eq!(count.get(), 1);
        let state = agent_pause(&store).unwrap();
        assert_eq!((state.tool_gate, state.at), (Some(false), Some(cutex::asks::store::AgentPauseAt::TurnEnd)));
        assert_eq!(state.phase, AgentPausePhase::Pausing);
    }

    #[test]
    fn held_calls_complete_the_pause_and_are_cleared_by_a_new_runtime() {
        let _serial = serial();
        reset();
        let store = store("tool-gate-held");
        online(&store, 3, true);
        pause(&store);
        let t = Instant::now();
        reconcile_with(&store, &mut |_, _| Ok(Push::Set), t).unwrap();
        let params = json!({"threadId":"thread-1","held":true,"waitingCount":2,"waiting":[
            {"callId":"toolu_1","tool":"Bash","turnId":"turn-1","agentId":null,"sinceMs":1_762_000_000_000_i64},
            {"callId":"toolu_2","tool":"Read","turnId":"turn-1","agentId":"a1","sinceMs":1_762_000_000_500_i64},
            {"callId":"","tool":"Bad"}]});
        assert_eq!(held_from("other-thread", &params), None);
        let held = held_from("thread-1", &params).unwrap();
        assert_eq!(held.len(), 2);
        assert_eq!((held[1].agent_id.as_deref(), held[0].since.as_str()), (Some("a1"), "2025-11-01T12:26:40.000Z"));
        // Codex: a sub-agent thread's call waits at the root's gate under its own thread id.
        let codex = json!({"threadId":"thread-1","held":true,"waiting":[
            {"callId":"call_1","tool":"shell","turnId":"turn-1","threadId":"thread-1","sinceMs":1},
            {"callId":"call_2","tool":"apply_patch","turnId":"sub-turn","threadId":"thread-sub","sinceMs":2}]});
        let rows = held_from("thread-1", &codex).unwrap();
        assert_eq!(rows.iter().map(|row| row.agent_id.as_deref()).collect::<Vec<_>>(), [None, Some("thread-sub")]);
        store.with_state(|state| cutex::asks::agent_pause::note_held(state, S, held.clone(), Utc::now())).unwrap();
        let state = agent_pause(&store).unwrap();
        assert_eq!(state.phase, AgentPausePhase::Paused);
        assert!(state.paused_at.is_some());
        assert_eq!(state.held, held);
        let view = cutex::asks::agent_pause::view(Some(&state));
        assert_eq!(view["held"][0], json!({"callId":"toolu_1","tool":"Bash","turnId":"turn-1","since":"2025-11-01T12:26:40.000Z"}));
        assert_eq!((view["at"].as_str(), view["toolGate"].as_bool()), (Some("toolBoundary"), Some(true)));
        // A replacement runtime holds nothing: the stale list is dropped.
        online(&store, 4, true);
        reconcile_with(&store, &mut |_, _| Ok(Push::Set), t).unwrap();
        assert!(agent_pause(&store).unwrap().held.is_empty());
        // After resume a late report changes nothing.
        store.with_state(|state| cutex::asks::agent_pause::resume(state, S)).unwrap();
        assert!(!store.with_state(|state| cutex::asks::agent_pause::note_held(state, S, held, Utc::now())).unwrap());
    }

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
