//! Runtime events → ask attempts (asks design §5.5, §5.6). Called by the
//! management process for every notification of a bound thread; only
//! sessions holding an admission lease do any work.
//!
//! - `turn/started` of the leased turn: the attempt is `running`.
//! - `item/*` in the leased turn: activity (tool calls, an activity line),
//!   the last final agent message (kept as an artifact), owner steering
//!   (`userMessage` → `ownerSteered`), other external input
//!   (`includedNotes`).
//! - `turn/diff/updated`, `thread/tokenUsage/updated`: answer metadata.
//! - `turn/completed`: the answer — the turn's last non-commentary agent
//!   message (`null` when none) with status, a new answer revision in the
//!   artifact store, one delivery per requester, and the lease released.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use super::answer;
use super::model::*;
use super::store::{AskState, AskStore};
use super::{new_id, now_rfc3339, truncate_chars};

/// Methods this observer reads (everything else is skipped cheaply).
pub fn is_relevant(method: &str) -> bool {
    matches!(
        method,
        "turn/started"
            | "turn/completed"
            | "item/started"
            | "item/completed"
            | "turn/diff/updated"
            | "thread/tokenUsage/updated"
            | "thread/compacted"
    )
}

/// One compaction shows up as a `contextCompaction` item and (older
/// servers) `thread/compacted`; notifications this close are the same one.
const COMPACTION_DEBOUNCE_SECS: i64 = 30;

/// The native context was compacted during an attempt (r208 backstop for a
/// remote compaction that drops the turn's ASK item): flag the attempt and
/// ask the scheduler to steer a brief reminder into the turn.
fn note_compaction(state: &mut AskState, ask_id: &str, attempt: u32, now: DateTime<Utc>) {
    state.update_ask(ask_id, now, |ask| {
        let Some(record) = ask.attempts.iter_mut().find(|record| record.n == attempt) else {
            return;
        };
        let recent = record
            .compacted_at
            .as_deref()
            .and_then(super::parse_time)
            .is_some_and(|at| now - at < chrono::Duration::seconds(COMPACTION_DEBOUNCE_SECS));
        record.compacted_mid_attempt = true;
        if !recent {
            record.compaction_reminder_pending = true;
        }
        record.compacted_at = Some(now_rfc3339(now));
    });
}

/// The requester's context was compacted in the turn carrying an
/// ANSWER/QUESTION event (r210 backstop: natives before r50 drop the turn's
/// event item, so the results are acknowledged but never seen): ask the
/// scheduler to steer a reminder of the delivered delegations.
fn note_event_compaction(state: &mut AskState, event_id: &str, now: DateTime<Utc>) {
    let Some(event) = state.events.get_mut(event_id) else {
        return;
    };
    let recent = event
        .compacted_at
        .as_deref()
        .and_then(super::parse_time)
        .is_some_and(|at| now - at < chrono::Duration::seconds(COMPACTION_DEBOUNCE_SECS));
    if !recent {
        event.compaction_reminder_pending = true;
    }
    event.compacted_at = Some(now_rfc3339(now));
}

fn turn_id_of(method: &str, params: &Value) -> Option<String> {
    match method {
        "turn/started" | "turn/completed" => params.pointer("/turn/id"),
        _ => params.get("turnId"),
    }
    .and_then(Value::as_str)
    .map(str::to_string)
}

/// Whether an item notification is a finished context compaction.
fn is_compaction_item(method: &str, params: &Value) -> bool {
    method == "item/completed" && params.pointer("/item/type").and_then(Value::as_str) == Some("contextCompaction")
}

/// Item types counted as tool calls in the activity summary.
fn is_tool_item(kind: &str) -> bool {
    matches!(
        kind,
        "commandExecution" | "fileChange" | "mcpToolCall" | "dynamicToolCall" | "webSearch" | "collabAgentToolCall" | "imageView"
    )
}

fn activity_line(item: &Value) -> Option<String> {
    let kind = item.get("type")?.as_str()?;
    let line = match kind {
        "commandExecution" => format!("running {}", item.get("command").and_then(Value::as_str).unwrap_or("a command")),
        "fileChange" => "editing files".to_string(),
        "mcpToolCall" => format!(
            "calling {}",
            item.get("tool").and_then(Value::as_str).unwrap_or("a tool")
        ),
        "dynamicToolCall" => format!("calling {}", item.get("tool").and_then(Value::as_str).unwrap_or("a tool")),
        "webSearch" => "searching the web".to_string(),
        "reasoning" => "thinking".to_string(),
        "agentMessage" => "writing".to_string(),
        _ => return None,
    };
    Some(truncate_chars(&line, 160))
}

/// Which leased attempt (if any) a turn belongs to.
fn leased_attempt(state: &AskState, session: &str, turn_id: &str) -> Option<(String, u32)> {
    let lease = state.leases.get(session)?;
    if lease.turn_id.as_deref() != Some(turn_id) {
        return None;
    }
    match &lease.holder {
        LeaseHolder::Ask { ask_id, attempt } => Some((ask_id.clone(), *attempt)),
        _ => None,
    }
}

/// Observe one runtime notification of `session`.
pub fn observe(store: &AskStore, session: &str, method: &str, params: &Value, now: DateTime<Utc>) -> anyhow::Result<()> {
    observe_with(store, session, method, params, now, &|| false)
}

/// [`observe`], where `requeue_on_quota` says whether an ask attempt that
/// failed on the target's exhausted usage limit goes back to the queue
/// (held `blocked(quota)` until the limit resets) instead of failing the
/// delegation. Asked only for such a failure. The management process
/// answers it for Claude targets whose quota auto-continue is on (that
/// tracks the reset and holds the target until then).
pub fn observe_with(
    store: &AskStore,
    session: &str,
    method: &str,
    params: &Value,
    now: DateTime<Utc>,
    requeue_on_quota: &dyn Fn() -> bool,
) -> anyhow::Result<()> {
    if !is_relevant(method) {
        return Ok(());
    }
    let Some(state) = store.peek()? else {
        return Ok(());
    };
    let Some(lease) = state.leases.get(session) else {
        return Ok(());
    };
    let Some(turn_id) = turn_id_of(method, params) else {
        return Ok(());
    };
    if lease.turn_id.as_deref() != Some(turn_id.as_str()) {
        return Ok(());
    }
    if let LeaseHolder::Event { event_id } = &lease.holder {
        if method == "thread/compacted" || is_compaction_item(method, params) {
            let event_id = event_id.clone();
            store.with_state(|state| note_event_compaction(state, &event_id, now))?;
            return Ok(());
        }
        if method == "turn/completed" {
            let event_id = event_id.clone();
            store.with_state(|state| {
                super::delivery::acknowledge_event(state, &event_id, &turn_id, now);
                state.leases.remove(session);
            })?;
        }
        return Ok(());
    }
    let Some((ask_id, attempt)) = leased_attempt(&state, session, &turn_id) else {
        return Ok(());
    };
    let ask = state.asks.get(&ask_id).cloned();
    if method == "thread/compacted" || is_compaction_item(method, params) {
        store.with_state(|state| note_compaction(state, &ask_id, attempt, now))?;
        if method == "thread/compacted" {
            return Ok(());
        }
    }
    match method {
        "turn/started" => {
            store.with_state(|state| {
                state.update_ask(&ask_id, now, |ask| {
                    if let Some(record) = ask.attempts.iter_mut().find(|record| record.n == attempt) {
                        if matches!(record.state, AttemptState::Submitting | AttemptState::Admitted) {
                            record.state = AttemptState::Running;
                        }
                        record.started_at.get_or_insert_with(|| now_rfc3339(now));
                    }
                    if !ask.execution.is_terminal() {
                        ask.execution = ExecutionState::Running;
                    }
                })
            })?;
        }
        "item/started" | "item/completed" => {
            let item = params.get("item").cloned().unwrap_or(Value::Null);
            let kind = item.get("type").and_then(Value::as_str).unwrap_or("").to_string();
            let mut message_artifact = None;
            if method == "item/completed" && answer::is_final_agent_message(&item) {
                if let Some(text) = answer::agent_message_text(&item) {
                    let readers: Vec<String> = ask
                        .as_ref()
                        .map(AskState::ask_sessions)
                        .unwrap_or_default();
                    message_artifact = Some(store.put_artifact(text.as_bytes(), "text/markdown; charset=utf-8", &readers, Some(&ask_id), now)?);
                }
            }
            let own_message = ask
                .as_ref()
                .and_then(|ask| ask.attempts.iter().find(|record| record.n == attempt))
                .map(|record| record.native_message_id.clone());
            store.with_state(|state| {
                state.update_ask(&ask_id, now, |ask| {
                    let Some(record) = ask.attempts.iter_mut().find(|record| record.n == attempt) else {
                        return;
                    };
                    if method == "item/started" {
                        if is_tool_item(&kind) {
                            record.activity.tool_calls += 1;
                        }
                        if let Some(line) = activity_line(&item) {
                            record.activity.line = Some(line);
                        }
                        if kind == "userMessage" {
                            record.owner_steered = true;
                        }
                    }
                    if kind == "functionCallOutput"
                        && item.get("externalInputView").is_some_and(|view| !view.is_null() && !super::views::is_delegation_view(view))
                    {
                        if let Some(id) = item.get("id").and_then(Value::as_str) {
                            if Some(id) != own_message.as_deref() && !record.included_notes.iter().any(|note| note == id) {
                                record.included_notes.push(id.to_string());
                            }
                        }
                    }
                    if let Some(artifact) = &message_artifact {
                        record.last_message_artifact = Some(artifact.clone());
                    }
                });
                if method == "item/completed" {
                    super::awaits::observe_item(state, &ask_id, attempt, &item, now);
                }
            })?;
        }
        "turn/diff/updated" => {
            if let Some(diff) = params.get("diff").and_then(Value::as_str) {
                let artifact = store.put_artifact(diff.as_bytes(), "text/x-diff; charset=utf-8", &[], Some(&ask_id), now)?;
                store.with_state(|state| {
                    state.update_ask(&ask_id, now, |ask| {
                        if let Some(record) = ask.attempts.iter_mut().find(|record| record.n == attempt) {
                            record.native_diff_artifact = Some(artifact);
                        }
                    })
                })?;
            }
        }
        "thread/tokenUsage/updated" => {
            let usage = params.get("tokenUsage").cloned();
            store.with_state(|state| {
                state.update_ask(&ask_id, now, |ask| {
                    if let Some(record) = ask.attempts.iter_mut().find(|record| record.n == attempt) {
                        record.token_usage = usage;
                    }
                })
            })?;
        }
        "turn/completed" => {
            let turn = params.get("turn").cloned().unwrap_or(Value::Null);
            if crate::management::v2::quota_continue::quota_failure(&turn)
                && requeue_quota_failure(store, session, &ask_id, attempt, &turn, requeue_on_quota, now)?
            {
                return Ok(());
            }
            finalize(store, session, &ask_id, attempt, &turn, None, now)?;
        }
        _ => {}
    }
    Ok(())
}

/// An ask attempt whose turn failed because the target's usage limit is
/// exhausted (owner report 2026-10-14: a Claude Director's delegation
/// failed on `rate_limit` and was closed as failed; nothing ran it again
/// after the reset). The attempt ends `failed` (its error kept), but the
/// delegation stays open: back to `queued` (a continuation stays a pending
/// continuation), held `blocked(quota)`, no answer and no requester
/// delivery. The scheduler redelivers it as the next attempt once the
/// target is no longer quota-blocked. Not for a stopped attempt, an
/// `ifBusy: fail` ask, or past [`super::QUOTA_REQUEUE_LIMIT`]. Returns
/// whether the ask was requeued (else the caller finalizes as usual).
fn requeue_quota_failure(
    store: &AskStore,
    session: &str,
    ask_id: &str,
    attempt: u32,
    turn: &Value,
    requeue_on_quota: &dyn Fn() -> bool,
    now: DateTime<Utc>,
) -> anyhow::Result<bool> {
    let Some(ask) = store.snapshot()?.asks.get(ask_id).cloned() else {
        return Ok(false);
    };
    let stop_requested = ask.attempts.iter().find(|record| record.n == attempt).is_none_or(|record| record.stop_requested);
    if !ask.is_open()
        || ask.remote.is_some()
        || stop_requested
        || ask.options.if_busy == IfBusy::Fail
        || ask.quota_requeues >= super::QUOTA_REQUEUE_LIMIT
        || !requeue_on_quota()
    {
        return Ok(false);
    }
    let (_, error) = turn_outcome(turn);
    store.with_state(|state| {
        let mut requeued = false;
        state.update_ask(ask_id, now, |ask| {
            let Some(record) = ask.attempts.iter_mut().find(|item| item.n == attempt) else {
                return;
            };
            record.ended_at = Some(now_rfc3339(now));
            record.last_error = error.clone();
            record.state = AttemptState::Failed;
            ask.quota_requeues += 1;
            ask.hold = Some(Hold::Blocked { by: BlockedReason::Quota });
            // An await registered in the failed turn is void; a pending
            // continuation (`resume`) is kept and admitted again.
            ask.awaiting = None;
            if ask.resume.is_none() {
                ask.execution = ExecutionState::Queued;
            }
            requeued = true;
        });
        if !requeued {
            return false;
        }
        for question in state.questions.values_mut() {
            if question.ask_id == ask_id && question.attempt == attempt && question.state == QuestionState::Open {
                question.state = QuestionState::Stale;
            }
        }
        if let Some(lease) = state.leases.get(session) {
            if matches!(&lease.holder, LeaseHolder::Ask { ask_id: id, attempt: n } if id == ask_id && *n == attempt) {
                state.leases.remove(session);
            }
        }
        if let Some(updated) = state.asks.get(ask_id).cloned() {
            // Waiters learn it is held (as for any blocked hold).
            super::delivery::record_state_delivery(state, &updated, "held", now);
        }
        true
    })
}

/// The terminal status of a native turn.
fn turn_outcome(turn: &Value) -> (AnswerStatus, Option<String>) {
    let error = turn
        .get("error")
        .filter(|error| !error.is_null())
        .map(|error| {
            truncate_chars(
                error.get("message").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| error.to_string()).as_str(),
                300,
            )
        });
    match turn.get("status").and_then(Value::as_str) {
        Some("interrupted") => (AnswerStatus::Interrupted, error),
        Some("failed") => (AnswerStatus::Failed, error),
        _ => (AnswerStatus::Completed, error),
    }
}

/// Capture the answer of an attempt's terminal turn.
pub fn finalize(
    store: &AskStore,
    session: &str,
    ask_id: &str,
    attempt: u32,
    turn: &Value,
    interrupt_reason: Option<InterruptReason>,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    let state = store.snapshot()?;
    let Some(ask) = state.asks.get(ask_id).cloned() else {
        return Ok(());
    };
    let Some(record) = ask.attempts.iter().find(|record| record.n == attempt).cloned() else {
        return Ok(());
    };
    let (status, error) = turn_outcome(turn);
    // Items in `turn/completed` (Claude adapter) win over observed items.
    let items: Vec<Value> = turn.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
    // Job submits only reported with the completed turn bind as well.
    if items.iter().any(|item| super::awaits::submitted_job(item).is_some()) {
        store.with_state(|state| {
            for item in &items {
                super::awaits::observe_item(state, ask_id, attempt, item, now);
            }
        })?;
    }
    // A completed turn that registered delegation_await is a stage, not the
    // answer (delegation await §2.2).
    let stop_requested = record.stop_requested;
    if status == AnswerStatus::Completed
        && !stop_requested
        && ask.is_open()
        && ask.awaiting.as_ref().is_some_and(|wait| wait.attempt == attempt)
    {
        return super::awaits::yield_attempt(store, session, ask_id, attempt, turn, now);
    }
    let readers = AskState::ask_sessions(&ask);
    let message_artifact = match answer::final_message(&items) {
        Some(text) => Some(store.put_artifact(text.as_bytes(), "text/markdown; charset=utf-8", &readers, Some(ask_id), now)?),
        None => record.last_message_artifact.clone(),
    };
    let bytes = message_artifact
        .as_deref()
        .and_then(|artifact| state.artifacts.get(artifact).map(|record| record.bytes))
        .or_else(|| {
            message_artifact
                .as_deref()
                .and_then(|artifact| store.read_artifact(artifact, None).ok().flatten())
                .map(|(record, _)| record.bytes)
        })
        .unwrap_or(0);
    let native_diff = record
        .native_diff_artifact
        .as_deref()
        .and_then(|artifact| store.read_artifact(artifact, None).ok().flatten())
        .map(|(_, bytes)| truncate_chars(&String::from_utf8_lossy(&bytes), 20_000));
    let preview = message_artifact
        .as_deref()
        .and_then(|artifact| store.read_artifact(artifact, None).ok().flatten())
        .and_then(|(_, bytes)| super::views::preview(&String::from_utf8_lossy(&bytes)));
    let started = record.started_at.as_deref().and_then(super::parse_time);
    store.with_state(|state| {
        let revision = state.answers.values().filter(|answer| answer.ask_id == ask_id).count() as u32 + 1;
        let answer_id = new_id("ans");
        let metadata = AnswerMetadata {
            turn_id: record.turn_id.clone(),
            duration_ms: started.map(|started| (now - started).num_milliseconds().max(0) as u64),
            token_usage: record.token_usage.clone(),
            last_error: error.clone(),
            owner_steered: record.owner_steered,
            shared_turn: record.shared_turn,
            included_notes: record.included_notes.clone(),
            profile: record.profile.clone(),
            model: record.model.clone(),
            native_diff: native_diff.clone(),
            compacted: record.compacted_mid_attempt
                || state
                    .asks
                    .get(ask_id)
                    .and_then(|ask| ask.attempts.iter().find(|item| item.n == attempt))
                    .is_some_and(|item| item.compacted_mid_attempt),
        };
        state.answers.insert(
            answer_id.clone(),
            AnswerRecord {
                answer_id: answer_id.clone(),
                ask_id: ask_id.to_string(),
                attempt,
                revision,
                status,
                artifact_id: message_artifact.clone(),
                bytes,
                metadata: metadata.clone(),
                created_at: now_rfc3339(now),
                preview: preview.clone(),
            },
        );
        let owner_paused = state.pauses.contains_key(session);
        state.update_ask(ask_id, now, |ask| {
            if let Some(attempt_record) = ask.attempts.iter_mut().find(|item| item.n == attempt) {
                attempt_record.ended_at = Some(now_rfc3339(now));
                attempt_record.last_error = error.clone();
                attempt_record.state = match status {
                    AnswerStatus::Completed => AttemptState::Answered,
                    AnswerStatus::Failed => AttemptState::Failed,
                    AnswerStatus::Interrupted => AttemptState::Interrupted,
                };
                if status == AnswerStatus::Interrupted {
                    attempt_record.interrupt_reason = Some(if attempt_record.stop_requested {
                        InterruptReason::Stopped
                    } else if let Some(reason) = interrupt_reason {
                        reason
                    } else {
                        let _ = owner_paused;
                        InterruptReason::OwnerStopped
                    });
                }
            }
            ask.answer_id = Some(answer_id.clone());
            ask.hold = None;
            // An await registered in a failed / interrupted turn is void.
            ask.awaiting = None;
            ask.resume = None;
            let stopped = ask.current_attempt().is_some_and(|attempt| attempt.stop_requested);
            match status {
                AnswerStatus::Completed => {
                    ask.execution = ExecutionState::Answered;
                    if ask.options.review {
                        ask.review = Some(ReviewState::AwaitingAcceptance);
                    } else {
                        ask.closed = Some(CloseOutcome::Completed);
                        ask.closed_at = Some(now_rfc3339(now));
                    }
                }
                AnswerStatus::Failed => {
                    ask.execution = ExecutionState::Failed;
                    ask.closed = Some(CloseOutcome::Completed);
                    ask.closed_at = Some(now_rfc3339(now));
                }
                AnswerStatus::Interrupted => {
                    ask.execution = ExecutionState::Interrupted;
                    if stopped {
                        ask.closed = Some(CloseOutcome::Stopped);
                        ask.closed_at = Some(now_rfc3339(now));
                    }
                }
            }
        });
        // Open questions of this attempt can no longer be answered.
        for question in state.questions.values_mut() {
            if question.ask_id == ask_id && question.attempt == attempt && question.state == QuestionState::Open {
                question.state = QuestionState::Stale;
            }
        }
        if let Some(lease) = state.leases.get(session) {
            if matches!(&lease.holder, LeaseHolder::Ask { ask_id: id, attempt: n } if id == ask_id && *n == attempt) {
                state.leases.remove(session);
            }
        }
        if let Some(updated) = state.asks.get(ask_id).cloned() {
            super::delivery::record_answer_delivery(state, &updated, &answer_id, now);
            // Owner decision 2026-10-08: the owner stopped the ask's turn
            // (stop, or interrupt-and-send): the requester is woken when idle
            // to decide (retry, reassign, close), even for passive asks.
            let owner_stopped = updated
                .attempts
                .iter()
                .find(|item| item.n == attempt)
                .is_some_and(|item| item.interrupt_reason == Some(InterruptReason::OwnerStopped));
            if owner_stopped {
                super::delivery::record_owner_stopped_delivery(state, &updated, now);
            }
            state.notice(
                "cutex/ask/answered",
                AskState::ask_sessions(&updated),
                json!({
                    "askId": ask_id,
                    "answerId": answer_id,
                    "revision": revision,
                    "status": status,
                    "bytes": bytes,
                    "metadata": metadata,
                    "preview": preview,
                }),
            );
        }
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::scheduler::tests::{world, FakeRuntime};
    use crate::asks::store::tests::{new_ask, temp_store, REQUESTER, TARGET};

    fn completed(turn: &str, status: &str) -> Value {
        json!({"threadId": "t", "turn": {"id": turn, "status": status, "items": [], "error": null}})
    }

    fn item(turn: &str, item: Value) -> Value {
        json!({"threadId": "t", "turnId": turn, "item": item})
    }

    /// Acceptance §12.3 item 7 (answers, part): the final non-commentary
    /// message is the answer; a turn without one answers `null`; owner
    /// steering is flagged; the lease is released; one delivery per answer.
    #[test]
    fn answers_are_captured_from_the_leased_turn_only() {
        let (world, dir) = world("observe");
        let (store, store_dir) = temp_store("observe");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        let ask = store.with_state(|state| state.create_ask(new_ask("a", "x"), now).unwrap().0.ask_id).unwrap();
        crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
        // Another turn's events are ignored.
        observe(&store, TARGET, "item/completed", &item("other", json!({"type": "agentMessage", "text": "nope"})), now).unwrap();
        observe(&store, TARGET, "turn/started", &json!({"turn": {"id": "turn-1"}}), now).unwrap();
        assert_eq!(store.snapshot().unwrap().asks[&ask].execution, ExecutionState::Running);
        observe(&store, TARGET, "item/started", &item("turn-1", json!({"type": "commandExecution", "command": "cargo test"})), now).unwrap();
        observe(&store, TARGET, "item/started", &item("turn-1", json!({"type": "userMessage", "id": "u"})), now).unwrap();
        observe(&store, TARGET, "item/completed", &item("turn-1", json!({"type": "agentMessage", "phase": "final_answer", "text": "All tests pass."})), now).unwrap();
        observe(&store, TARGET, "item/completed", &item("turn-1", json!({"type": "agentMessage", "phase": "commentary", "text": "checking"})), now).unwrap();
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&ask].attempts[0].activity.tool_calls, 1);
        assert_eq!(state.asks[&ask].attempts[0].activity.line.as_deref(), Some("running cargo test"));
        observe(&store, TARGET, "turn/completed", &completed("turn-1", "completed"), now).unwrap();
        let state = store.snapshot().unwrap();
        let record = &state.asks[&ask];
        assert_eq!(record.execution, ExecutionState::Answered);
        assert_eq!(record.closed, Some(CloseOutcome::Completed));
        assert!(state.leases.is_empty());
        let answer = &state.answers[record.answer_id.as_ref().unwrap()];
        assert!(answer.metadata.owner_steered);
        let (_, text) = store.read_artifact(answer.artifact_id.as_ref().unwrap(), Some(REQUESTER)).unwrap().unwrap();
        assert_eq!(text, b"All tests pass.");
        assert_eq!(state.deliveries.values().filter(|d| d.kind == DeliveryKind::Answer).count(), 1);
        assert!(state.notices.iter().any(|n| n.method == "cutex/ask/answered"));

        // A review ask whose turn has no final message answers null and
        // awaits acceptance.
        let mut review = new_ask("r", "y");
        review.options.review = true;
        let review = store.with_state(|state| state.create_ask(review, now).unwrap().0.ask_id).unwrap();
        runtime.set(TARGET, |status| status.active_turn_id = None);
        crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
        observe(&store, TARGET, "turn/completed", &completed("turn-2", "completed"), now).unwrap();
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&review].review, Some(ReviewState::AwaitingAcceptance));
        assert!(state.asks[&review].closed.is_none());
        assert!(state.answers[state.asks[&review].answer_id.as_ref().unwrap()].artifact_id.is_none());
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    #[test]
    fn interrupted_and_failed_turns() {
        let (world, dir) = world("interrupt");
        let (store, store_dir) = temp_store("interrupt");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        let ask = store.with_state(|state| state.create_ask(new_ask("a", "x"), now).unwrap().0.ask_id).unwrap();
        crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
        observe(&store, TARGET, "item/completed", &item("turn-1", json!({"type": "agentMessage", "text": "partial"})), now).unwrap();
        observe(&store, TARGET, "turn/completed", &completed("turn-1", "interrupted"), now).unwrap();
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&ask].execution, ExecutionState::Interrupted);
        assert_eq!(state.asks[&ask].attempts[0].interrupt_reason, Some(InterruptReason::OwnerStopped));
        assert!(state.asks[&ask].is_open(), "an interrupted ask stays open for a retry");
        let answer = &state.answers[state.asks[&ask].answer_id.as_ref().unwrap()];
        assert_eq!(answer.status, AnswerStatus::Interrupted);
        assert!(answer.artifact_id.is_some(), "partial message kept");
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }
    fn quota_failed(turn: &str) -> Value {
        // claude-r5 wording (`codexErrorInfo: other`), owner report 2026-10-14.
        json!({"threadId": "t", "turn": {"id": turn, "status": "failed", "items": [], "error": {
            "codexErrorInfo": "other", "additionalDetails": null,
            "message": "Official Claude returned an error (rate_limit). You may submit a fresh instruction; the failed input is not automatically retried."}}})
    }

    /// A delegation whose attempt failed on the target's exhausted Claude
    /// usage limit stays open, waits `blocked(quota)` while the target is
    /// held and is redelivered as the next attempt afterwards.
    #[test]
    fn quota_failed_attempts_go_back_to_the_queue() {
        let (world, dir) = world("quota-requeue");
        let (store, store_dir) = temp_store("quota-requeue");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        let ask = store.with_state(|state| state.create_ask(new_ask("a", "x"), now).unwrap().0.ask_id).unwrap();
        crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
        let turn = store.snapshot().unwrap().asks[&ask].attempts[0].turn_id.clone().unwrap();
        observe_with(&store, TARGET, "turn/completed", &quota_failed(&turn), now, &|| true).unwrap();
        let state = store.snapshot().unwrap();
        let record = &state.asks[&ask];
        assert!(record.is_open(), "not failed");
        assert_eq!(record.execution, ExecutionState::Queued);
        assert_eq!(record.hold, Some(Hold::Blocked { by: BlockedReason::Quota }));
        assert_eq!(record.quota_requeues, 1);
        assert_eq!(record.attempts[0].state, AttemptState::Failed);
        assert!(record.attempts[0].last_error.as_deref().unwrap().contains("rate_limit"));
        assert!(record.answer_id.is_none() && state.answers.is_empty(), "no answer");
        assert!(state.leases.is_empty());
        assert!(!state.deliveries.values().any(|d| d.kind == DeliveryKind::Answer));
        // Held while the target is quota-blocked.
        runtime.set(TARGET, |status| {
            status.active_turn_id = None;
            status.blocked = Some(BlockedReason::Quota);
        });
        crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&ask].attempts.len(), 1);
        assert_eq!(state.asks[&ask].hold, Some(Hold::Blocked { by: BlockedReason::Quota }));
        // The limit reset: redelivered as attempt 2, which answers.
        runtime.set(TARGET, |status| status.blocked = None);
        crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&ask].attempts.len(), 2);
        assert_eq!(state.asks[&ask].attempts[1].n, 2);
        let turn = state.asks[&ask].attempts[1].turn_id.clone().unwrap();
        observe(&store, TARGET, "item/completed", &item(&turn, json!({"type": "agentMessage", "phase": "final_answer", "text": "done"})), now).unwrap();
        observe(&store, TARGET, "turn/completed", &completed(&turn, "completed"), now).unwrap();
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&ask].execution, ExecutionState::Answered);
        assert_eq!(state.asks[&ask].closed, Some(CloseOutcome::Completed));

        // Without the policy (Codex, auto-continue off) or past the limit:
        // the attempt fails the delegation as before.
        for (requeue, used) in [(false, 0), (true, crate::asks::QUOTA_REQUEUE_LIMIT)] {
            let other = store.with_state(|state| state.create_ask(new_ask(&format!("b{used}{requeue}"), "y"), now).unwrap().0.ask_id).unwrap();
            store.with_state(|state| state.asks.get_mut(&other).unwrap().quota_requeues = used).unwrap();
            runtime.set(TARGET, |status| status.active_turn_id = None);
            crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
            let turn = store.snapshot().unwrap().asks[&other].attempts[0].turn_id.clone().unwrap();
            observe_with(&store, TARGET, "turn/completed", &quota_failed(&turn), now, &|| requeue).unwrap();
            let state = store.snapshot().unwrap();
            assert_eq!(state.asks[&other].execution, ExecutionState::Failed);
            assert_eq!(state.asks[&other].closed, Some(CloseOutcome::Completed));
        }
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }
}
