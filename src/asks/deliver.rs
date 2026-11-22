//! r290 P14: explicit result delivery from another turn of the same handler.
//! Artifact IO is outside the transaction; answer, receipt and cancellation
//! commit together and serialize with continuation admission.
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use super::{model::*, store::{AskError, AskState}, tools::{ToolContext, DELEGATION_DELIVER}};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    delegation: String,
    text: String,
}

fn persistence(error: anyhow::Error) -> AskError {
    AskError::new("persistence_unavailable", super::truncate_chars(&format!("{error:#}"), 300))
}

fn validate(state: &AskState, caller: &str, id: &str) -> Result<(AskRecord, DeliveryProvenance), AskError> {
    use super::continuation::{Phase, Submission};
    let ask = state.asks.get(id).filter(|ask| ask.remote.is_none() && ask.target_cutex_session_id == caller)
        .ok_or_else(|| AskError::new("ask_not_found", "no such delegation to you"))?;
    let attempt = ask.current_attempt().ok_or_else(|| AskError::new("ask_not_waiting", "the delegation has no waiting attempt"))?;
    if !ask.is_open() || ask.follow_up_ended || ask.execution != ExecutionState::Running
        || attempt.state != AttemptState::Yielded || attempt.stop_requested
        || super::view::continuation_wait(ask).is_null()
        || ask.bounded_continuation.current.as_ref().is_some_and(|record|
            record.state != Phase::Cancelled && (record.state == Phase::Resuming
                || record.wake.as_ref().is_some_and(|wake| !matches!(wake.submission, Submission::Unclaimed))))
        || state.leases.get(caller).is_some_and(|lease| matches!(&lease.holder, LeaseHolder::Ask {ask_id,..} if ask_id == id))
    {
        return Err(AskError::new("ask_not_waiting", "deliver requires an open yielded delegation whose continuation has not been submitted"));
    }
    let presence = state.presence.get(caller).filter(|presence| presence.online && presence.busy
        && presence.runtime_generation == Some(attempt.runtime_generation))
        .ok_or_else(|| AskError::new("ask_not_current_handler", "delivery requires a turn of the current handler generation"))?;
    // Delivery starts no turn and does not reopen an admission gate. A
    // delegation-only pause must not block an already-authorized owner turn
    // from handing in work; whole-agent tool fences remain in force upstream.
    let turn = presence.turn_id.as_ref().filter(|turn| !turn.is_empty() && Some(turn.as_str()) != attempt.turn_id.as_deref())
        .ok_or_else(|| AskError::new("ask_not_current_handler", "delivery requires a current turn after the yielded turn"))?;
    Ok(((**ask).clone(), DeliveryProvenance {turn_id:turn.clone(), runtime_generation:attempt.runtime_generation, at:String::new()}))
}

pub fn handle(ctx: &ToolContext<'_>, args: Value, action: &str) -> Result<Value, AskError> {
    handle_with(ctx, args, action, &|| {})
}

pub(crate) fn handle_with(ctx: &ToolContext<'_>, args: Value, action: &str, before_commit: &dyn Fn()) -> Result<Value, AskError> {
    let args: Args = serde_json::from_value(args).map_err(|_| super::tools::invalid("delegation and text are required; no other fields are accepted"))?;
    if args.text.trim().is_empty() || args.text.chars().count() > 65_536 {
        return Err(super::tools::invalid("text must contain 1–65536 characters"));
    }
    let request = json!({"delegation":args.delegation,"text":args.text});
    let snapshot = ctx.store.shared().map_err(persistence)?;
    // Replays are immutable observations, including after closure/rotation.
    let key = format!("{}\u{1f}{action}", ctx.caller);
    if let Some(record) = snapshot.actions.get(&key) {
        if record.tool != format!("{}{}", super::operations::PREFIX, DELEGATION_DELIVER)
            || record.request_sha256 != crate::approvals::request_digest(&request) {
            return Err(AskError::new("idempotency_conflict", "action_id belongs to a different request"));
        }
        return record.result.clone().ok_or_else(|| AskError::new("outcome_unknown", "inspect the original action before retrying"));
    }
    let (ask, source) = validate(&snapshot, ctx.caller, &args.delegation)?;
    drop(snapshot);
    let artifact = ctx.store.put_artifact(args.text.as_bytes(), "text/markdown; charset=utf-8",
        &AskState::ask_sessions(&ask), Some(&ask.ask_id), ctx.now).map_err(persistence)?;
    before_commit();
    ctx.store.with_records(&[&ask.ask_id], |state| {
        super::operations::execute(state, ctx.caller, DELEGATION_DELIVER, action, &request, ctx.now, |next| {
            let (current, mut checked_source) = validate(next, ctx.caller, &ask.ask_id)?;
            if current.current_attempt().map(|a| (a.n, &a.turn_id, a.runtime_generation))
                != ask.current_attempt().map(|a| (a.n, &a.turn_id, a.runtime_generation))
                || checked_source != source {
                return Err(AskError::new("ask_delivery_conflict", "the handler turn or waiting attempt changed before delivery"));
            }
            checked_source.at = super::now_rfc3339(ctx.now);
            let result = commit(next, &current, checked_source, &artifact, &args.text, ctx.now);
            Ok((result, super::operations::EffectState::Committed))
        })
    }).map_err(persistence)?
}

fn commit(state: &mut AskState, ask: &AskRecord, source: DeliveryProvenance,
    artifact: &str, text: &str, now: DateTime<Utc>) -> Value {
    let attempt = ask.current_attempt().expect("validated waiting attempt");
    let answer_id = super::new_id("ans");
    let revision = state.answers.values().filter(|answer| answer.ask_id == ask.ask_id).count() as u32 + 1;
    let answer = AnswerRecord {
        answer_id:answer_id.clone(), ask_id:ask.ask_id.clone(), attempt:attempt.n, revision,
        status:AnswerStatus::Completed, artifact_id:Some(artifact.into()), bytes:text.len(),
        metadata:AnswerMetadata {
            // The original handling turn remains the provenance of this attempt.
            turn_id:attempt.turn_id.clone(), delivered_from:Some(source.clone()),
            owner_steered:attempt.owner_steered, shared_turn:attempt.shared_turn,
            included_notes:attempt.included_notes.clone(), profile:attempt.profile.clone(), model:attempt.model.clone(),
            compacted:attempt.compacted_mid_attempt, ..Default::default()
        },
        created_at:super::now_rfc3339(now), preview:super::views::preview(text),
    };
    let metadata = answer.metadata.clone();
    let preview = answer.preview.clone();
    state.answers.insert(answer_id.clone(), answer);
    state.update_ask(&ask.ask_id, now, |ask| {
        // Keep yielded turn timestamps/stage notes intact; the answer carries delivery time.
        let record = ask.current_attempt_mut().unwrap();
        record.state = AttemptState::Answered;
        record.compaction_reminder_pending = false;
        ask.answer_id = Some(answer_id.clone());
        ask.execution = ExecutionState::Answered;
        ask.hold = None;
        ask.awaiting = None;
        ask.resume = None;
        ask.bounded_continuation.cancel("explicit_delivery");
        if ask.options.review { ask.review = Some(ReviewState::AwaitingAcceptance); }
        else { ask.closed = Some(CloseOutcome::Completed); ask.closed_at = Some(super::now_rfc3339(now)); }
    });
    for question in state.questions.values_mut().filter(|q| q.ask_id == ask.ask_id
        && q.attempt == attempt.n && q.state == QuestionState::Open) { question.state = QuestionState::Stale; }
    let updated = state.asks[&ask.ask_id].clone();
    // The unrelated current turn/lease is deliberately preserved.
    super::delivery::record_answer_delivery(state, &updated, &answer_id, now);
    state.notice("cutex/ask/answered", AskState::ask_sessions(&updated),
        json!({"askId":updated.ask_id,"answerId":answer_id,"revision":revision,
            "status":AnswerStatus::Completed,"bytes":text.len(),"metadata":metadata,"preview":preview}));
    state.notice("cutex/delegation/continuationChanged", AskState::ask_sessions(&updated),
        json!({"projectId":updated.project_id,"resourceId":updated.ask_id,"revision":updated.revision}));
    json!({"ok":true,"ask_id":updated.ask_id,"answer_id":answer_id,"revision":revision,
        "state":updated.execution,"closed":updated.closed,"deliveredFrom":source,
        "detail":format!("Delivered from turn {}. The answer is recorded and the pending continuation is cancelled.", source.turn_id)})
}
