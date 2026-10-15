//! Runtime commands queued by the bus process or Management v2 and run by
//! the management process (asks design §5.9, §5.10).

use chrono::{DateTime, Utc};
use serde_json::json;

#[cfg(test)]
use super::model::*;
use super::scheduler::{AskRuntime, RespondOutcome};
use super::store::{AskStore, RuntimeCommand};
use super::truncate_chars;

/// Run one command; `Ok(true)` when it is done (or can never succeed).
pub fn run(runtime: &dyn AskRuntime, store: &AskStore, command: &RuntimeCommand, now: DateTime<Utc>) -> anyhow::Result<bool> {
    match command {
        RuntimeCommand::ResumeGate { .. } => Ok(false),
        RuntimeCommand::StopAttempt { ask_id, attempt } => stop_attempt(runtime, store, ask_id, *attempt, now),
        RuntimeCommand::RespondQuestion { question_id } => respond_question(runtime, store, question_id, now),
        RuntimeCommand::CancelJobs { ask_id, cutex_session_id, jobs } => {
            super::awaits::run_cancel(runtime, store, ask_id, cutex_session_id, jobs, now)
        }
    }
}

/// Requester stop of a running attempt: gate-preserving interrupt (N2).
/// Other asks to the target are unaffected; the observer captures the
/// partial message when the turn ends `interrupted`.
fn stop_attempt(runtime: &dyn AskRuntime, store: &AskStore, ask_id: &str, attempt: u32, now: DateTime<Utc>) -> anyhow::Result<bool> {
    let state = store.snapshot()?;
    let Some(ask) = state.asks.get(ask_id) else {
        return Ok(true);
    };
    let Some(record) = ask.attempts.iter().find(|record| record.n == attempt) else {
        return Ok(true);
    };
    if record.state.is_terminal() {
        return Ok(true);
    }
    let Some(turn_id) = record.turn_id.clone() else {
        // Not bound to a turn yet (receipt pending): try on the next tick.
        return Ok(false);
    };
    match runtime.interrupt_keep_gate(&ask.target_cutex_session_id, &turn_id) {
        Ok(()) => Ok(true),
        Err(error) => {
            let message = format!("{error:#}");
            if message.contains("n2_unavailable") || message.contains("not supported") {
                // The runtime would close its external-input gate for every
                // other requester: refuse the stop instead.
                store.with_state(|state| {
                    state.update_ask(ask_id, now, |ask| {
                        if let Some(record) = ask.attempts.iter_mut().find(|record| record.n == attempt) {
                            record.stop_requested = false;
                            record.last_error = Some("stop_unsupported: the target runtime cannot stop one ask without pausing all of its asks; the owner can stop it".into());
                        }
                    });
                    if let Some(ask) = state.asks.get(ask_id).cloned() {
                        super::delivery::record_state_delivery(state, &ask, "stopUnsupported", now);
                    }
                })?;
                return Ok(true);
            }
            Ok(false)
        }
    }
}

fn respond_question(runtime: &dyn AskRuntime, store: &AskStore, question_id: &str, now: DateTime<Utc>) -> anyhow::Result<bool> {
    let state = store.snapshot()?;
    let Some(question) = state.questions.get(question_id).cloned() else {
        return Ok(true);
    };
    let Some(resolution) = question.resolution.clone().filter(|resolution| !resolution.answers.is_null()) else {
        return Ok(true);
    };
    let Some(ask) = state.asks.get(&question.ask_id) else {
        return Ok(true);
    };
    let result = json!({ "answers": resolution.answers });
    let outcome = runtime.respond_question(
        &ask.target_cutex_session_id,
        question.runtime_generation,
        &format!("ask-{question_id}"),
        &question.native.request_id,
        &result,
    );
    let done = !matches!(outcome, RespondOutcome::NotSent(_));
    if let RespondOutcome::AlreadyResolved | RespondOutcome::Unknown(_) = &outcome {
        let note = match &outcome {
            RespondOutcome::Unknown(error) => format!("delivery outcome unknown: {}", truncate_chars(error, 200)),
            _ => "answered by someone else first".to_string(),
        };
        store.with_state(|state| {
            if let Some(ask) = state.asks.get(&question.ask_id).cloned() {
                state.notice(
                    "cutex/ask/question",
                    super::store::AskState::ask_sessions(&ask),
                    json!({"reason": "responseNotApplied", "questionId": question_id, "note": note}),
                );
            }
        })?;
        let _ = now;
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::scheduler::tests::{world, FakeRuntime};
    use crate::asks::store::tests::{new_ask, temp_store, REQUESTER, TARGET};

    /// Acceptance §12.3 item 5 (part): a requester stop of a running ask
    /// interrupts only that turn with the gate-preserving interrupt; the
    /// ask ends interrupted(stopped) and closed; other asks still run.
    #[test]
    fn requester_stop_interrupts_only_that_turn() {
        let (world, dir) = world("stop");
        let (store, store_dir) = temp_store("stop");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        let (first, second) = store
            .with_state(|state| {
                (state.create_ask(new_ask("a", "x"), now).unwrap().0.ask_id, state.create_ask(new_ask("b", "y"), now).unwrap().0.ask_id)
            })
            .unwrap();
        crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
        let stopped = crate::asks::tools::close_ask(&store, Some(REQUESTER), &first, "stop", now).unwrap();
        assert_eq!(stopped["stopping"], true);
        crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
        assert_eq!(runtime.interrupts.borrow().as_slice(), [format!("{TARGET}:turn-1")]);
        crate::asks::observe::observe(&store, TARGET, "turn/completed", &json!({"turn": {"id": "turn-1", "status": "interrupted"}}), now).unwrap();
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&first].attempts[0].interrupt_reason, Some(InterruptReason::Stopped));
        assert_eq!(state.asks[&first].closed, Some(CloseOutcome::Stopped));
        assert!(state.commands.is_empty());
        runtime.set(TARGET, |status| status.active_turn_id = None);
        assert_eq!(crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap().admitted, vec![second]);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    #[test]
    fn requester_answers_reach_the_native_request() {
        let (world, dir) = world("respond");
        let (store, store_dir) = temp_store("respond");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        store.with_state(|state| state.create_ask(new_ask("a", "x"), now).unwrap()).unwrap();
        crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
        let question = store
            .with_state(|state| {
                crate::asks::questions::record_question(state, TARGET, 1, &json!(42), &json!({"turnId": "turn-1", "questions": []}), now)
            })
            .unwrap()
            .unwrap();
        crate::asks::questions::respond_in_store(
            &store,
            &question,
            &crate::asks::questions::Responder::Agent(REQUESTER.into()),
            &json!({"q0": {"answers": ["yes"]}}),
            now,
        )
        .unwrap();
        crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
        let responses = runtime.responses.borrow();
        assert_eq!(responses[0].1, json!(42));
        assert_eq!(responses[0].2["answers"]["q0"]["answers"][0], "yes");
        assert!(store.snapshot().unwrap().commands.is_empty());
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }
}
