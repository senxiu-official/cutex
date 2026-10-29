//! Durable whole-agent pause. This is deliberately independent of
//! `asks::control`: the older asks pause remains delegation-only.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use super::now_rfc3339;
use super::store::{AgentPause, AgentPausePhase, AskError, AskState, AskStore};

pub const PAUSE_METHOD: &str = "cutex/agent/pause";
pub const RESUME_METHOD: &str = "cutex/agent/resume";
pub const CHANGED_EVENT: &str = "cutex/agent/pauseChanged";

pub fn is_paused(state: &AskState, session: &str) -> bool {
    state.presence.get(session).is_some_and(|presence| presence.agent_pause.is_some())
        || super::project_control::is_paused(state, session)
}

pub fn view(pause: Option<&AgentPause>) -> Value {
    pause.map_or(Value::Null, |pause| {
        serde_json::to_value(pause).expect("agent pause serializes")
    })
}

fn emit(state: &mut AskState, session: &str) {
    let pause = view(state.presence.get(session).and_then(|presence| presence.agent_pause.as_ref()));
    state.notice(
        CHANGED_EVENT,
        vec![session.to_string()],
        json!({"agentPause": pause}),
    );
}

/// Install the gate immediately. The already-running turn is never
/// interrupted; it is the only turn allowed to finish behind this gate.
pub fn pause(
    state: &mut AskState,
    session: &str,
    by: &str,
    by_session: Option<&str>,
    now: DateTime<Utc>,
) -> Value {
    if let Some(existing) = state.presence.get(session).and_then(|presence| presence.agent_pause.as_ref()) {
        return json!({"agentPause": view(Some(existing)), "changed": false});
    }
    let presence = state.presence.entry(session.to_string()).or_default();
    let busy = presence.busy;
    let at = now_rfc3339(now);
    presence.agent_pause = Some(AgentPause {
        cutex_session_id: session.to_string(),
        phase: if busy { AgentPausePhase::Pausing } else { AgentPausePhase::Paused },
        requested_at: at.clone(),
        paused_at: (!busy).then_some(at),
        by: by.into(),
        by_cutex_session_id: by_session.map(str::to_string),
    });
    presence.paused = true;
    emit(state, session);
    json!({"agentPause": view(state.presence.get(session).and_then(|presence| presence.agent_pause.as_ref())), "changed": true})
}

pub fn resume(state: &mut AskState, session: &str) -> Value {
    let asks_paused = state.pauses.contains_key(session) || super::project_control::is_paused(state, session);
    let changed = state.presence.get_mut(session).is_some_and(|presence| {
        let changed = presence.agent_pause.take().is_some();
        presence.paused = asks_paused;
        changed
    });
    if changed {
        emit(state, session);
    }
    json!({"agentPause": null, "changed": changed})
}

/// Reconcile the phase with actual runtime activity. Called on terminal
/// events and periodic presence refresh, so restart cannot strand `pausing`.
pub fn settle(state: &mut AskState, session: &str, busy: bool, now: DateTime<Utc>) -> bool {
    let Some(pause) = state.presence.get_mut(session).and_then(|presence| presence.agent_pause.as_mut()) else {
        return false;
    };
    if busy || pause.phase == AgentPausePhase::Paused {
        return false;
    }
    pause.phase = AgentPausePhase::Paused;
    pause.paused_at = Some(now_rfc3339(now));
    emit(state, session);
    true
}

pub fn pause_in_store(
    store: &AskStore,
    session: &str,
    by: &str,
    by_session: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Value, AskError> {
    store
        .with_state(|state| pause(state, session, by, by_session, now))
        .map_err(|error| AskError::new("persistence_unavailable", format!("{error:#}")))
}

pub fn resume_in_store(store: &AskStore, session: &str) -> Result<Value, AskError> {
    store
        .with_state(|state| resume(state, session))
        .map_err(|error| AskError::new("persistence_unavailable", format!("{error:#}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::store::Presence;

    #[test]
    fn busy_pause_settles_and_resume_removes_gate() {
        let now = "2026-10-25T00:00:00Z".parse().unwrap();
        let mut state = AskState::default();
        state.presence.insert(
            "cutex.a".into(),
            Presence {
                online: true,
                busy: true,
                ..Default::default()
            },
        );
        assert_eq!(
            pause(&mut state, "cutex.a", "owner", None, now)["agentPause"]["phase"],
            "pausing"
        );
        assert!(!settle(&mut state, "cutex.a", true, now));
        assert!(settle(&mut state, "cutex.a", false, now));
        assert_eq!(state.presence["cutex.a"].agent_pause.as_ref().unwrap().phase, AgentPausePhase::Paused);
        assert_eq!(resume(&mut state, "cutex.a")["changed"], true);
        assert!(!is_paused(&state, "cutex.a"));
    }

    #[test]
    fn pause_survives_store_reopen() {
        let (store, dir) = crate::asks::store::tests::temp_store("agent-pause-restart");
        let now = "2026-10-25T00:00:00Z".parse().unwrap();
        store.with_state(|state| {
            state.presence.insert(
                "cutex.a".into(),
                Presence {
                    online: true,
                    busy: true,
                    ..Default::default()
                },
            );
        }).unwrap();
        pause_in_store(&store, "cutex.a", "director", Some("cutex.director"), now).unwrap();
        drop(store);

        let reopened = AskStore::at(&dir);
        let state = reopened.snapshot().unwrap();
        let pause = state.presence["cutex.a"].agent_pause.as_ref().expect("durable pause");
        assert_eq!(pause.phase, AgentPausePhase::Pausing);
        assert_eq!(pause.by, "director");
        assert_eq!(pause.by_cutex_session_id.as_deref(), Some("cutex.director"));
        assert!(is_paused(&state, "cutex.a"));
        reopened.with_state(|state| {
            state.presence.get_mut("cutex.a").unwrap().busy = false;
            assert!(settle(state, "cutex.a", false, now));
        }).unwrap();
        drop(reopened);

        let settled = AskStore::at(&dir).snapshot().unwrap();
        let pause = settled.presence["cutex.a"].agent_pause.as_ref().expect("durable settled pause");
        assert_eq!(pause.phase, AgentPausePhase::Paused);
        assert!(pause.paused_at.is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pause_blocks_owner_admission_without_consuming_the_queue() {
        let now = "2026-10-25T00:00:00Z".parse().unwrap();
        let mut state = AskState::default();
        pause(&mut state, "cutex.a", "owner", None, now);
        assert!(!crate::asks::scheduler::owner_may_flush(
            &state,
            "cutex.a",
            &crate::management::v2::admission::AdmissionSnapshot::default(),
            None,
        ));
        assert!(state.presence["cutex.a"].agent_pause.is_some());
    }
}
