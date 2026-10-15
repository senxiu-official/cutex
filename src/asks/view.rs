//! JSON views of asks for Management v2 and the agent tools.

use serde_json::{json, Value};

use super::model::*;
use super::store::{queue_position, AskState};

/// The stored requester shape (`cutex_session_id`, kept one release) plus
/// `cutexSessionId` (r208).
pub fn requester_value(requester: &Requester) -> Value {
    if matches!(requester, Requester::Peer { .. }) { return super::views::requester_json(requester); }
    let mut value = serde_json::to_value(requester).unwrap_or(Value::Null);
    if let (Some(object), Some(session)) = (value.as_object_mut(), requester.session()) {
        object.insert("cutexSessionId".into(), json!(session));
    }
    value
}

/// The current attempt for clients (`AskSummary.attempt`, r208).
pub fn attempt_summary(attempt: &AttemptRecord) -> Value {
    json!({
        "n": attempt.n,
        "state": attempt.state,
        "turnId": attempt.turn_id,
        "startedAt": attempt.started_at,
        "endedAt": attempt.ended_at,
        "profile": attempt.profile,
        "model": attempt.model,
        "activity": attempt.activity,
        "interruptReason": attempt.interrupt_reason,
        "lastError": attempt.last_error,
        "progress": attempt.progress,
        "continuation": attempt.continuation,
        "stageNote": attempt.stage_note,
        "stopRequested": attempt.stop_requested,
    })
}


/// Job bindings for clients: id, state, ok, cancel state (delegation
/// await §7).
pub fn jobs_summary(ask: &AskRecord) -> Value {
    json!(ask
        .jobs
        .iter()
        .map(|job| json!({
            "jobId": job.job_id,
            "source": job.source,
            "status": job.terminal.as_ref().map(|terminal| terminal.status.clone()).unwrap_or_else(|| "running".into()),
            "exitCode": job.terminal.as_ref().and_then(|terminal| terminal.exit_code),
            "ok": job.terminal.as_ref().map(|terminal| terminal.ok),
            "cancel": job.cancel.as_ref().map(|cancel| cancel.state),
        }))
        .collect::<Vec<_>>())
}

/// Revisions for lists (no text).
pub fn revisions_summary(ask: &AskRecord) -> Value {
    json!(ask
        .revisions
        .iter()
        .map(|revision| json!({
            "id": revision.id,
            "level": revision.level,
            "supersedes": revision.supersedes,
            "sentAt": revision.sent_at,
            "acknowledgedAt": revision.acknowledged_at,
        }))
        .collect::<Vec<_>>())
}

/// The wait for clients (`{jobs, until, resume, since, timeoutAt, note}`).
pub fn await_summary(ask: &AskRecord) -> Value {
    match &ask.awaiting {
        Some(wait) => json!({"jobs": wait.jobs, "until": wait.until, "resume": wait.resume, "since": wait.since,
            "timeoutAt": wait.timeout_at, "note": wait.note, "attempt": wait.attempt}),
        None => Value::Null,
    }
}
/// Questions of `ask` routed to the owner and not answered yet.
pub fn owner_questions(state: &AskState, ask: &AskRecord) -> usize {
    state
        .questions
        .values()
        .filter(|question| {
            question.ask_id == ask.ask_id
                && question.state == QuestionState::Open
                && (question.route == QuestionRoute::Owner || question.routed_to_owner_at.is_some())
        })
        .count()
}

/// Full owner view (`GET /v2/asks/{id}`): the record plus queue position,
/// answer metadata, questions and attempt ids.
pub fn peer_remote(ask: &AskRecord) -> Value {
    ask.remote.as_ref().map(|r|json!({"peerId":r.peer_id,"label":crate::peers::wire::sanitize_name(&r.label),"targetName":crate::peers::wire::sanitize_name(&r.target_name),"syncState":r.sync.state,"lastError":r.sync.last_error.as_deref().map(crate::peers::model::sanitize_diagnostic),"interruptReason":r.sync.interrupt_reason,"remoteState":r.remote_state})).unwrap_or(Value::Null)
}
pub fn ask_requester(ask: &AskRecord) -> Value {
    let mut v=requester_value(&ask.requester);
    if let Some(i)=&ask.peer_inbound { v["claimedCaps"]=json!(i.request.requester.caps); }
    v
}
pub fn owner_view(state: &AskState, ask: &AskRecord) -> Value {
    let mut value = serde_json::to_value(ask).unwrap_or(Value::Null);
    if let Some(object) = value.as_object_mut() {
        object.insert("queuePosition".into(), json!(queue_position(state, ask)));
        object.insert("requester".into(), ask_requester(ask));
        object.remove("peerInbound");
        object.insert("lateAnswer".into(), json!(ask.remote.as_ref().and_then(|r|r.late_answer.as_ref())));
        object.insert("resultUndeliverable".into(), json!(ask.peer_inbound.as_ref().is_some_and(|i|i.result_undeliverable)));
        object.insert("remote".into(), peer_remote(ask));
        if let Some(attempts) = object.get_mut("attempts").and_then(Value::as_array_mut) {
            for (attempt, record) in attempts.iter_mut().zip(&ask.attempts) {
                if let Some(attempt) = attempt.as_object_mut() {
                    attempt.insert("attemptId".into(), json!(record.attempt_id(&ask.ask_id)));
                }
            }
        }
        object.insert(
            "answer".into(),
            ask.answer_id
                .as_deref()
                .and_then(|id| state.answers.get(id))
                .map(|answer| json!(answer))
                .unwrap_or(Value::Null),
        );
        let questions: Vec<&QuestionRecord> = state
            .questions
            .values()
            .filter(|question| question.ask_id == ask.ask_id)
            .collect();
        object.insert("questions".into(), json!(questions));
    }
    value
}

/// Compact list row (`GET /v2/asks`, `agent_list`).
pub fn summary(state: &AskState, ask: &AskRecord) -> Value {
    json!({
        "askId": ask.ask_id,
        "hops": ask.hops,
        "lateAnswer": ask.remote.as_ref().and_then(|r|r.late_answer.as_ref()),
        "resultUndeliverable": ask.peer_inbound.as_ref().is_some_and(|i|i.result_undeliverable),
        "title": ask.title,
        "requester": ask_requester(ask),
        "target": ask.target_cutex_session_id,
        "targetName": ask.target_name,
        "remote": peer_remote(ask),
        "projectId": ask.project_id,
        "execution": ask.execution,
        "hold": ask.hold,
        "holdSince": ask.hold_since,
        "review": ask.review,
        "closed": ask.closed,
        "queuePosition": queue_position(state, ask),
        "attempts": ask.attempts.len(),
        "answerId": ask.answer_id,
        "openQuestions": state
            .questions
            .values()
            .filter(|question| question.ask_id == ask.ask_id && question.state == QuestionState::Open)
            .count(),
        "attempt": ask.current_attempt().map(attempt_summary),
        "closedAt": ask.closed_at,
        "followUpOf": ask.follow_up_of,
        "ownerQuestions": owner_questions(state, ask),
        "answerPreview": ask.answer_id.as_deref().and_then(|id| state.answers.get(id)).and_then(|answer| answer.preview.clone()),
        "createdAt": ask.created_at,
        "updatedAt": ask.updated_at,
        "revision": ask.revision,
        "await": await_summary(ask),
        "resume": ask.resume.as_ref().map(|resume| json!({"reason": resume.reason, "since": resume.since, "by": resume.by})),
        "jobs": jobs_summary(ask),
        "revisions": revisions_summary(ask),
        "supersededBy": ask.superseded_by,
    })
}

/// The requester's view in tool results: state, hold, the activity of the
/// current attempt only (§5.5), never other turns.
pub fn requester_view(state: &AskState, ask: &AskRecord) -> Value {
    let attempt = ask.current_attempt();
    json!({
        "ask_id": ask.ask_id,
        "action_id": ask.action_id,
        "target": ask.target_name.clone().unwrap_or_else(|| ask.target_cutex_session_id.clone()),
        "target_id": ask.target_cutex_session_id,
        "title": ask.title,
        "state": ask.execution,
        "hold": ask.hold,
        "hold_since": ask.hold_since,
        "queue_position": queue_position(state, ask),
        "review": ask.review,
        "attempt": attempt.map(|attempt| attempt.n),
        "activity": attempt.map(|attempt| &attempt.activity),
        "interrupt_reason": attempt.and_then(|attempt| attempt.interrupt_reason).or_else(||ask.remote.as_ref().and_then(|r|r.sync.interrupt_reason)),
        "remote": peer_remote(ask),
        "target_reverted": ask.target_reverted,
        "closed": ask.closed,
        "approval_id": ask.gate.as_ref().and_then(|gate| gate.approval_id.clone()),
        "waiting": ask.awaiting.as_ref().filter(|_| ask.is_waiting()).map(|wait| json!({"jobs": wait.jobs, "since": wait.since, "timeout_at": wait.timeout_at, "note": wait.note})),
        "resuming": ask.resume_pending(),
        "jobs": jobs_summary(ask),
        "revisions": revisions_summary(ask),
        "superseded_by": ask.superseded_by,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::store::tests::new_ask;

    #[test]
    fn views_carry_queue_position_and_attempt_ids() {
        let mut state = AskState::default();
        let now = chrono::Utc::now();
        let (ask, _) = state.create_ask(new_ask("v", "Review"), now).unwrap();
        state.update_ask(&ask.ask_id, now, |ask| {
            ask.attempts.push(AttemptRecord::submitting(1, "m".into(), 3, "t".into()));
        });
        let ask = state.asks[&ask.ask_id].clone();
        let owner = owner_view(&state, &ask);
        assert_eq!(owner["queuePosition"], 1);
        assert_eq!(owner["attempts"][0]["attemptId"], format!("{}#1", ask.ask_id));
        assert_eq!(summary(&state, &ask)["title"], "Review");
        // List rows show a pending stop.
        assert_eq!(summary(&state, &ask)["attempt"]["stopRequested"], false);
        state.update_ask(&ask.ask_id, now, |ask| ask.attempts[0].stop_requested = true);
        let ask = state.asks[&ask.ask_id].clone();
        assert_eq!(summary(&state, &ask)["attempt"]["stopRequested"], true);
        crate::management::v2::schema_export::assert_valid(
            "asks",
            "askList",
            &serde_json::json!({"hostId": "h", "asks": [summary(&state, &ask)], "hasMore": false}),
        );
        assert_eq!(requester_view(&state, &ask)["attempt"], 1);
    }
}
