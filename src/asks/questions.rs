//! Questions raised inside an ask's turn (asks design §5.9).
//!
//! `request_user_input` (Codex) and AskUserQuestion (Claude) server requests
//! in the leased turn of an attempt become question records bound to the
//! attempt, runtime generation and native pending request. They go to the
//! requester by default (wake any wait, or a QUESTION event), fall through to
//! the owner when the requester is offline or after 10 minutes, and the owner
//! can always answer (the native pending request stays visible in Management
//! v2). The first accepted response wins (the native response is sent through
//! the same `server_requests` claim as the owner's clients); a later one gets
//! `already_resolved`, one for a superseded attempt or generation `stale`.
//! Questions are never an approval channel: approval requests are not
//! turned into questions, and an answer is task input only.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use super::model::*;
use super::store::{AskError, AskState, AskStore, RuntimeCommand};
use super::tools::ToolContext;
use super::{new_id, now_rfc3339, parse_time, QUESTION_TIMEOUT_SECS};

/// The native server request methods that become questions.
pub const QUESTION_METHODS: &[&str] = &["item/tool/requestUserInput"];

/// Record a question for the attempt holding `session`'s lease, if the
/// request belongs to its turn. Returns the question id.
pub fn record_question(
    state: &mut AskState,
    session: &str,
    runtime_generation: u64,
    request_id: &Value,
    params: &Value,
    now: DateTime<Utc>,
) -> Option<String> {
    let lease = state.leases.get(session)?.clone();
    let LeaseHolder::Ask { ask_id, attempt } = &lease.holder else {
        return None;
    };
    let turn_id = params.get("turnId").and_then(Value::as_str)?;
    if lease.turn_id.as_deref() != Some(turn_id) {
        return None;
    }
    if state
        .questions
        .values()
        .any(|question| question.native.request_id == *request_id && question.native.turn_id == turn_id)
    {
        return None;
    }
    let ask = state.asks.get(ask_id)?.clone();
    let question_id = new_id("q");
    let route = ask.options.questions;
    let record = QuestionRecord {
        question_id: question_id.clone(),
        ask_id: ask_id.clone(),
        attempt: *attempt,
        runtime_generation,
        native: NativeQuestionRef {
            thread_id: params.get("threadId").and_then(Value::as_str).unwrap_or_default().to_string(),
            turn_id: turn_id.to_string(),
            item_id: params.get("itemId").and_then(Value::as_str).unwrap_or_default().to_string(),
            request_id: request_id.clone(),
        },
        questions: params.get("questions").cloned().unwrap_or(Value::Null),
        route,
        routed_to_owner_at: (route == QuestionRoute::Owner || ask.requester.is_owner()).then(|| now_rfc3339(now)),
        state: QuestionState::Open,
        resolution: None,
        created_at: now_rfc3339(now),
    };
    state.questions.insert(question_id.clone(), record.clone());
    state.update_ask(ask_id, now, |ask| ask.hold = Some(Hold::WaitingForInput));
    if route == QuestionRoute::Requester {
        super::delivery::record_question_delivery(state, &ask, &question_id, now);
    }
    state.notice(
        "cutex/ask/question",
        AskState::ask_sessions(&ask),
        json!({"reason": "asked", "question": record}),
    );
    Some(question_id)
}

/// Drop `waitingForInput` once no question of the ask is open.
fn clear_input_hold(state: &mut AskState, ask_id: &str, now: DateTime<Utc>) {
    let open = state
        .questions
        .values()
        .any(|question| question.ask_id == ask_id && question.state == QuestionState::Open);
    if !open {
        state.update_ask(ask_id, now, |ask| {
            if ask.hold == Some(Hold::WaitingForInput) {
                ask.hold = None;
            }
        });
    }
}

/// Who answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Responder {
    Owner,
    Agent(String),
}

/// Resolve a question (first accepted response wins) and queue the native
/// response.
pub fn resolve(
    state: &mut AskState,
    question_id: &str,
    responder: &Responder,
    answers: &Value,
    current_generation: Option<u64>,
    now: DateTime<Utc>,
) -> Result<QuestionRecord, AskError> {
    let question = state
        .questions
        .get(question_id)
        .cloned()
        .ok_or_else(|| AskError::new("question_not_found", "no such question"))?;
    let ask = state
        .asks
        .get(&question.ask_id)
        .cloned()
        .ok_or_else(|| AskError::new("question_not_found", "no such question"))?;
    if let Responder::Agent(session) = responder {
        if ask.requester.session() != Some(session.as_str()) {
            return Err(AskError::new("question_not_found", "no such question on your delegations"));
        }
    }
    match question.state {
        QuestionState::Resolved => {
            return Err(AskError::new("already_resolved", "this question was already answered")
                .with(json!({"by": question.resolution.as_ref().map(|resolution| &resolution.by)})))
        }
        QuestionState::Stale => return Err(AskError::new("stale", "the turn that asked this question is over")),
        QuestionState::Open => {}
    }
    let current_attempt = ask.current_attempt().map(|attempt| attempt.n);
    if current_attempt != Some(question.attempt)
        || current_generation.is_some_and(|generation| generation != question.runtime_generation)
    {
        if let Some(record) = state.questions.get_mut(question_id) {
            record.state = QuestionState::Stale;
        }
        clear_input_hold(state, &question.ask_id, now);
        return Err(AskError::new("stale", "the attempt or runtime that asked this question was superseded"));
    }
    if !answers.is_object() {
        return Err(AskError::new("invalid_arguments", "answers must be an object keyed by question id"));
    }
    let by = match responder {
        Responder::Owner => "owner".to_string(),
        Responder::Agent(session) => session.clone(),
    };
    let record = state.questions.get_mut(question_id).expect("exists");
    record.state = QuestionState::Resolved;
    record.resolution = Some(QuestionResolution { by, answers: answers.clone(), at: now_rfc3339(now) });
    let resolved = record.clone();
    super::control::queue_command(state, RuntimeCommand::RespondQuestion { question_id: question_id.to_string() }, now);
    clear_input_hold(state, &question.ask_id, now);
    state.notice(
        "cutex/ask/question",
        AskState::ask_sessions(&ask),
        json!({"reason": "resolved", "question": resolved}),
    );
    Ok(resolved)
}

/// The native side resolved the request (`serverRequest/resolved`, e.g. the
/// owner answered in a client): close the question as answered by the owner.
pub fn native_resolved(state: &mut AskState, session: &str, request_id: &Value, now: DateTime<Utc>) {
    let target_of = |ask_id: &str| state.asks.get(ask_id).map(|ask| ask.target_cutex_session_id.clone());
    let matching: Vec<String> = state
        .questions
        .values()
        .filter(|question| question.state == QuestionState::Open && question.native.request_id == *request_id)
        .filter(|question| target_of(&question.ask_id).as_deref() == Some(session))
        .map(|question| question.question_id.clone())
        .collect();
    for question_id in matching {
        if let Some(question) = state.questions.get_mut(&question_id) {
            question.state = QuestionState::Resolved;
            question.resolution = Some(QuestionResolution { by: "owner".into(), answers: Value::Null, at: now_rfc3339(now) });
            let ask_id = question.ask_id.clone();
            clear_input_hold(state, &ask_id, now);
        }
    }
}

/// Fall through to the owner: questions open longer than the timeout, or
/// whose requester is offline (§5.9). Returns the ids routed now.
pub fn fall_through(state: &mut AskState, offline: &dyn Fn(&str) -> bool, now: DateTime<Utc>) -> Vec<String> {
    let timeout = chrono::Duration::seconds(QUESTION_TIMEOUT_SECS);
    let mut routed = Vec::new();
    let candidates: Vec<(String, String)> = state
        .questions
        .values()
        .filter(|question| question.state == QuestionState::Open && question.routed_to_owner_at.is_none())
        .map(|question| (question.question_id.clone(), question.ask_id.clone()))
        .collect();
    for (question_id, ask_id) in candidates {
        let Some(ask) = state.asks.get(&ask_id).cloned() else {
            continue;
        };
        let question = &state.questions[&question_id];
        let expired = parse_time(&question.created_at).is_some_and(|created| now - created >= timeout);
        let requester_offline = ask.requester.session().is_some_and(offline);
        if expired || requester_offline {
            let record = state.questions.get_mut(&question_id).expect("exists");
            record.routed_to_owner_at = Some(now_rfc3339(now));
            let snapshot = record.clone();
            state.notice(
                "cutex/ask/question",
                AskState::ask_sessions(&ask),
                json!({"reason": if expired { "timeout" } else { "requesterOffline" }, "routedToOwner": true, "question": snapshot}),
            );
            routed.push(question_id);
        }
    }
    routed
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RespondArgs {
    question: String,
    answers: Value,
}

/// `question_answer(question, answers)`.
pub fn question_answer(ctx: &ToolContext<'_>, args: Value, action_id: &str) -> Result<Value, AskError> {
    let args: RespondArgs = serde_json::from_value(args)
        .map_err(|error| AskError::new("invalid_arguments", super::truncate_chars(&error.to_string(), 300)))?;
    let request = json!({"question": args.question, "answers": args.answers});
    super::tools::once(ctx, super::tools::QUESTION_ANSWER, action_id, &request, || {
        respond_in_store(ctx.store, &args.question, &Responder::Agent(ctx.caller.to_string()), &args.answers, ctx.now).map(|question| {
            json!({
                "ok": true,
                "question_id": question.question_id,
                "ask_id": question.ask_id,
                "detail": "Answer recorded; it is sent to the agent's pending question. It is task input, not an approval.",
            })
        })
    })
}

/// Resolve in the store (bus process or Management v2).
pub fn respond_in_store(store: &AskStore, question_id: &str, responder: &Responder, answers: &Value, now: DateTime<Utc>) -> Result<QuestionRecord, AskError> {
    let current_generation = store
        .snapshot()
        .ok()
        .and_then(|state| state.questions.get(question_id).cloned())
        .and_then(|question| state_generation(&question));
    store
        .with_state(|state| resolve(state, question_id, responder, answers, current_generation, now))
        .map_err(|error| AskError::new("persistence_unavailable", format!("{error:#}")))?
}

/// The target's current runtime generation (session store), if known.
fn state_generation(question: &QuestionRecord) -> Option<u64> {
    let _ = question;
    crate::session::store::with_cutex_session_store(|store| {
        store
            .sessions
            .values()
            .find(|record| record.app_server_thread_id() == Some(question.native.thread_id.as_str()))
            .map(|record| record.runtime_generation)
    })
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::scheduler::tests::{world, FakeRuntime};
    use crate::asks::store::tests::{new_ask, temp_store, REQUESTER, TARGET};

    /// Acceptance §12.3 item 6 (part): two questions on one ask;
    /// simultaneous owner and requester responses (one wins, one gets
    /// already_resolved); fall-through to the owner after 10 minutes.
    #[test]
    fn questions_first_response_wins_and_fall_through() {
        let (world, dir) = world("questions");
        let (store, store_dir) = temp_store("questions");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        let ask = store.with_state(|state| state.create_ask(new_ask("a", "x"), now).unwrap().0.ask_id).unwrap();
        crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
        let params = |n: u32| json!({"threadId": "thread-Kimi", "turnId": "turn-1", "itemId": format!("item-{n}"), "questions": [{"id": "q0", "question": "Which DB?"}]});
        let (first, second, other_turn) = store
            .with_state(|state| {
                let first = record_question(state, TARGET, 1, &json!(7), &params(1), now).unwrap();
                let second = record_question(state, TARGET, 1, &json!(8), &params(2), now).unwrap();
                assert!(record_question(state, TARGET, 1, &json!(7), &params(1), now).is_none(), "deduplicated");
                let other = record_question(state, TARGET, 1, &json!(9), &json!({"turnId": "turn-x"}), now);
                (first, second, other)
            })
            .unwrap();
        assert!(other_turn.is_none());
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&ask].hold, Some(Hold::WaitingForInput));
        assert_eq!(state.deliveries.values().filter(|d| d.kind == DeliveryKind::Question).count(), 2);
        // The owner answers first; the requester then gets already_resolved.
        let answers = json!({"q0": {"answers": ["sqlite"]}});
        store.with_state(|state| resolve(state, &first, &Responder::Owner, &answers, Some(1), now)).unwrap().unwrap();
        let late = store
            .with_state(|state| resolve(state, &first, &Responder::Agent(REQUESTER.into()), &answers, Some(1), now))
            .unwrap()
            .unwrap_err();
        assert_eq!(late.code, "already_resolved");
        // Another agent can't answer someone else's question.
        let stranger = store
            .with_state(|state| resolve(state, &second, &Responder::Agent(TARGET.into()), &answers, Some(1), now))
            .unwrap()
            .unwrap_err();
        assert_eq!(stranger.code, "question_not_found");
        // A superseded generation is stale.
        let stale = store
            .with_state(|state| resolve(state, &second, &Responder::Agent(REQUESTER.into()), &answers, Some(2), now))
            .unwrap()
            .unwrap_err();
        assert_eq!(stale.code, "stale");
        let state = store.snapshot().unwrap();
        assert_eq!(state.commands.len(), 1, "one native response queued");
        assert_eq!(state.asks[&ask].hold, None);

        // Fall-through after 10 minutes.
        let (store2, dir2) = temp_store("questions2");
        let ask2 = store2.with_state(|state| state.create_ask(new_ask("b", "y"), now).unwrap().0.ask_id).unwrap();
        let runtime2 = FakeRuntime::idle(&[TARGET]);
        crate::asks::scheduler::tick(&world, &runtime2, &store2, now).unwrap();
        store2.with_state(|state| record_question(state, TARGET, 1, &json!(1), &params(1), now)).unwrap().unwrap();
        let online = |_: &str| false;
        assert!(store2.with_state(|state| fall_through(state, &online, now + chrono::Duration::minutes(5))).unwrap().is_empty());
        assert_eq!(store2.with_state(|state| fall_through(state, &online, now + chrono::Duration::minutes(11))).unwrap().len(), 1);
        let _ = ask2;
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(dir2);
        let _ = std::fs::remove_dir_all(store_dir);
    }
}
