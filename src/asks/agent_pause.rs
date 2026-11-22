//! Durable whole-agent pause. This is deliberately independent of
//! `asks::control`: the older asks pause remains delegation-only.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use super::now_rfc3339;
use super::store::{AgentPause, AgentPauseAt, AgentPausePhase, AskError, AskState, AskStore, HeldToolCall};

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
        at: None,
        tool_gate: None,
        held: Vec::new(),
    });
    presence.paused = true;
    emit(state, session);
    json!({"agentPause": view(state.presence.get(session).and_then(|presence| presence.agent_pause.as_ref())), "changed": true})
}

/// Reopen native admission/backoff without changing any explicit admission pause.
pub(super) fn resume_runtime_gate(state: &mut AskState, session: &str, now: DateTime<Utc>) -> bool {
    let runtime_gate = super::control::runtime_gate_closed(state, session);
    if super::project_control::is_paused(state, session)
        || super::control::stop_held(state, session)
        || (state.pauses.contains_key(session) && !runtime_gate)
    {
        return false;
    }
    if runtime_gate {
        return super::control::owner_resume(state, session, "resumed", now);
    }
    // Transport retry backoff has no cutex pause. Resume-now must still reach
    // the native adapter; queue_command deduplicates until it is consumed.
    let queued = super::control::gate_resume_queued(state, session);
    super::control::queue_command(state,
        super::store::RuntimeCommand::ResumeGate { cutex_session_id: session.to_string() }, now);
    !queued
}

pub fn resume(state: &mut AskState, session: &str) -> Value {
    // Native admission or transport backoff can wait without a whole-agent pause.
    let gate_changed = resume_runtime_gate(state, session, Utc::now());
    let asks_paused = state.pauses.contains_key(session) || super::project_control::is_paused(state, session);
    let changed = state.presence.get_mut(session).is_some_and(|presence| {
        let changed = presence.agent_pause.take().is_some();
        presence.paused = asks_paused;
        changed
    });
    if changed {
        emit(state, session);
    }
    json!({"agentPause": null, "changed": changed || gate_changed})
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

/// Most held calls listed in the pause state (the runtime bounds its own list too).
pub const MAX_HELD: usize = 64;

/// The live runtime was told about the pause (r276 tool gate): with a gate the
/// pause takes effect at its next tool call, otherwise at the end of the turn.
pub fn note_gate(state: &mut AskState, session: &str, tool_gate: bool) -> bool {
    let Some(pause) = state.presence.get_mut(session).and_then(|presence| presence.agent_pause.as_mut()) else {
        return false;
    };
    let at = if tool_gate { AgentPauseAt::ToolBoundary } else { AgentPauseAt::TurnEnd };
    if pause.tool_gate == Some(tool_gate) && pause.at == Some(at) {
        return false;
    }
    pause.tool_gate = Some(tool_gate);
    pause.at = Some(at);
    if !tool_gate {
        pause.held.clear();
    }
    emit(state, session);
    true
}

/// The runtime reported the calls it holds. The first held call completes the
/// pause (`paused`) although its turn is still open; resume lets it run.
/// Without a pause (a late report after resume) nothing changes.
pub fn note_held(state: &mut AskState, session: &str, mut held: Vec<HeldToolCall>, now: DateTime<Utc>) -> bool {
    let Some(pause) = state.presence.get_mut(session).and_then(|presence| presence.agent_pause.as_mut()) else {
        return false;
    };
    held.truncate(MAX_HELD);
    let settles = !held.is_empty() && pause.phase == AgentPausePhase::Pausing;
    if pause.held == held && !settles {
        return false;
    }
    if !held.is_empty() {
        pause.tool_gate = Some(true);
        pause.at = Some(AgentPauseAt::ToolBoundary);
    }
    pause.held = held;
    if settles {
        pause.phase = AgentPausePhase::Paused;
        pause.paused_at = Some(now_rfc3339(now));
    }
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
    use crate::asks::control;
    use crate::asks::model::Hold;
    use crate::asks::store::Presence;
    use crate::asks::store::tests::{new_ask, TARGET};

    #[test]
    fn resume_without_cutex_pause_queues_native_resume_and_rearms_after_consumption() {
        for agent_paused in [false, true] {
            let mut state = AskState::default();
            if agent_paused {
                pause(&mut state, TARGET, "director", Some("cutex.director"), Utc::now());
            }
            assert!(state.pauses.is_empty());
            assert_eq!(resume(&mut state, TARGET)["changed"], true);
            assert!(!is_paused(&state, TARGET));
            assert!(control::gate_resume_queued(&state, TARGET));
            assert_eq!(state.commands.len(), 1);
            assert!(matches!(&state.commands[0].command,
                super::super::store::RuntimeCommand::ResumeGate { cutex_session_id } if cutex_session_id == TARGET));
            assert_eq!(resume(&mut state, TARGET)["changed"], false);
            assert_eq!(state.commands.len(), 1);
            state.commands.clear(); // Adapter consumed the prior explicit resume.
            assert_eq!(resume(&mut state, TARGET)["changed"], true);
            assert!(control::gate_resume_queued(&state, TARGET));
            assert_eq!(state.commands.len(), 1);
        }
    }

    #[test]
    fn resume_without_agent_pause_preserves_owner_pause_and_queues_no_command() {
        let now = Utc::now();
        let mut state = AskState::default();
        let ask = state.create_ask(new_ask("owner-pause-no-agent-pause", "work"), now).unwrap().0;
        control::owner_pause(&mut state, TARGET, now);
        let original = control::pause_json(state.pauses.get(TARGET));
        let original_hold = state.asks[&ask.ask_id].hold.clone();
        assert_eq!(resume(&mut state, TARGET)["changed"], false);
        assert_eq!(control::pause_json(state.pauses.get(TARGET)), original);
        assert_eq!(state.asks[&ask.ask_id].hold, original_hold);
        assert!(state.commands.is_empty());
    }

    #[test]
    fn resume_runtime_gate_without_agent_pause_releases_held_ask_once() {
        let now = Utc::now();
        let mut state = AskState::default();
        let ask = state.create_ask(new_ask("resume-runtime-gate", "work"), now).unwrap().0;
        assert!(control::runtime_gate_pause(&mut state, TARGET, 7, now));
        assert_eq!(state.asks[&ask.ask_id].hold,
            Some(Hold::HeldByRuntime { native: Some(control::NATIVE_GATE_CLOSED.into()) }));
        assert!(!state.presence.contains_key(TARGET));

        assert_eq!(resume(&mut state, TARGET)["changed"], true);
        assert!(!control::runtime_gate_closed(&state, TARGET));
        assert_eq!(state.asks[&ask.ask_id].hold, None);
        assert!(control::gate_resume_queued(&state, TARGET));
        assert_eq!(state.commands.len(), 1);
        assert_eq!(resume(&mut state, TARGET)["changed"], false);
        assert_eq!(state.commands.len(), 1, "repeated resume must not duplicate native commands");
    }

    #[test]
    fn resume_clears_agent_and_runtime_pauses_together() {
        let now = Utc::now();
        let mut state = AskState::default();
        pause(&mut state, TARGET, "director", Some("cutex.director"), now);
        control::runtime_gate_pause(&mut state, TARGET, 7, now);
        assert_eq!(resume(&mut state, TARGET)["changed"], true);
        assert!(!is_paused(&state, TARGET));
        assert!(!state.presence[TARGET].paused);
        assert!(state.pauses.is_empty());
        assert!(control::gate_resume_queued(&state, TARGET));
    }

    #[test]
    fn resume_preserves_explicit_admission_pauses_and_stop_holds() {
        for kind in ["ownerPause", "ownerStop", "runtimeGateWithStop"] {
            let now = Utc::now();
            let mut state = AskState::default();
            let ask = state.create_ask(new_ask(kind, "work"), now).unwrap().0;
            pause(&mut state, TARGET, "director", Some("cutex.director"), now);
            match kind {
                "ownerPause" => { control::owner_pause(&mut state, TARGET, now); }
                "ownerStop" => control::owner_stop(&mut state, TARGET, now),
                _ => {
                    control::runtime_gate_pause(&mut state, TARGET, 7, now);
                    state.pauses.get_mut(TARGET).unwrap().stop_hold = true;
                }
            }
            let original = control::pause_json(state.pauses.get(TARGET));
            let original_hold = state.asks[&ask.ask_id].hold.clone();
            assert_eq!(resume(&mut state, TARGET)["changed"], true);
            assert_eq!(control::pause_json(state.pauses.get(TARGET)), original);
            assert_eq!(state.asks[&ask.ask_id].hold, original_hold);
            assert!(state.presence[TARGET].paused);
            assert!(state.commands.is_empty(), "must not reopen a gate behind an owner hold");
            assert_eq!(resume(&mut state, TARGET)["changed"], false);
        }
    }

    #[test]
    fn gate_then_owner_pause_survives_agent_resume_and_runtime_restart() {
        let now = Utc::now();
        let mut state = AskState::default();
        let ask = state.create_ask(new_ask("gate-then-owner-pause", "work"), now).unwrap().0;
        pause(&mut state, TARGET, "director", Some("cutex.director"), now);
        assert!(control::runtime_gate_pause(&mut state, TARGET, 7, now));
        assert!(control::owner_pause(&mut state, TARGET, now));
        assert!(!control::owner_pause(&mut state, TARGET, now), "repeat owner pause is unchanged");
        let owner_pause = &state.pauses[TARGET];
        assert_eq!(owner_pause.origin.as_deref(), Some(control::ORIGIN_OWNER_PAUSE));
        assert_eq!(owner_pause.runtime_generation, None);
        assert!(!control::runtime_gate_closed(&state, TARGET));
        assert_eq!(state.asks[&ask.ask_id].hold, Some(Hold::Paused { by: super::super::model::PausedBy::Owner }));

        assert_eq!(resume(&mut state, TARGET)["changed"], true, "whole-agent pause is released");
        assert!(state.presence[TARGET].agent_pause.is_none());
        assert!(state.presence[TARGET].paused, "explicit owner admission pause remains");
        assert_eq!(state.pauses[TARGET].origin.as_deref(), Some(control::ORIGIN_OWNER_PAUSE));
        assert!(state.commands.is_empty(), "Director resume cannot reopen the owner's gate");
        assert!(!control::runtime_gate_opened(&mut state, TARGET, Some(8), "runtimeRestart", now));
        assert!(!control::runtime_gate_opened(&mut state, TARGET, None, "turnStarted", now));
        assert_eq!(state.asks[&ask.ask_id].hold, Some(Hold::Paused { by: super::super::model::PausedBy::Owner }));
        assert!(control::owner_resume(&mut state, TARGET, "resumed", now));
        assert_eq!(state.asks[&ask.ask_id].hold, None);
        assert!(control::gate_resume_queued(&state, TARGET));
    }

    #[test]
    fn runtime_gate_resume_preserves_project_pause() {
        use crate::asks::project_control::{Mode, RunControl, Scope};
        let now = Utc::now();
        let mut state = AskState::default();
        state.project_control.projects.insert("alpha".into(), RunControl {
            project_id: "alpha".into(), epoch: 1, pause_id: Some("pause-project".into()),
            requested_by: None, requested_at: Some(now), mode: Mode::AfterTurn, scope: Scope::Members,
            membership_revision: 1, excluded_session_ids: Default::default(),
            members: [TARGET.to_string()].into(), interrupt_unknown: Default::default(),
        });
        pause(&mut state, TARGET, "director", Some("cutex.director"), now);
        control::runtime_gate_pause(&mut state, TARGET, 7, now);
        assert_eq!(resume(&mut state, TARGET)["changed"], true);
        assert!(super::super::project_control::is_paused(&state, TARGET));
        assert!(is_paused(&state, TARGET));
        assert!(state.presence[TARGET].paused);
        assert!(state.presence[TARGET].agent_pause.is_none());
        assert!(control::runtime_gate_closed(&state, TARGET), "project pause prevents native gate resume");
        assert!(state.commands.is_empty(), "must not reopen native backoff behind a project pause");
    }

    #[test]
    fn project_pause_without_agent_or_runtime_pause_queues_no_native_resume() {
        use crate::asks::project_control::{Mode, RunControl, Scope};
        let mut state = AskState::default();
        state.project_control.projects.insert("alpha".into(), RunControl {
            project_id: "alpha".into(), epoch: 1, pause_id: Some("pause-project".into()),
            requested_by: None, requested_at: Some(Utc::now()), mode: Mode::AfterTurn, scope: Scope::Members,
            membership_revision: 1, excluded_session_ids: Default::default(),
            members: [TARGET.to_string()].into(), interrupt_unknown: Default::default(),
        });
        assert_eq!(resume(&mut state, TARGET)["changed"], false);
        assert!(super::super::project_control::is_paused(&state, TARGET));
        assert_eq!(state.project_control.projects["alpha"].pause_id.as_deref(), Some("pause-project"));
        assert!(state.commands.is_empty());
    }

    #[test]
    fn runtime_gate_resume_command_survives_store_reopen() {
        let (store, dir) = crate::asks::store::tests::temp_store("agent-resume-runtime-gate");
        store.with_state(|state| {
            control::runtime_gate_pause(state, TARGET, 7, Utc::now());
        }).unwrap();
        assert_eq!(resume_in_store(&store, TARGET).unwrap()["changed"], true);
        drop(store);
        let reopened = AskStore::at(&dir);
        let state = reopened.snapshot().unwrap();
        assert!(!control::runtime_gate_closed(&state, TARGET));
        assert!(control::gate_resume_queued(&state, TARGET));
        assert_eq!(state.commands.len(), 1);
        drop(reopened);
        let _ = std::fs::remove_dir_all(dir);
    }

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
