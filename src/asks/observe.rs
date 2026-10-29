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
    if !attempt_open(state, ask_id, attempt) {
        return;
    }
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
        if !recent && record.last_message_artifact.is_none() {
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
    let Some(turn_id) = turn_id_of(method, params) else {
        return Ok(());
    };
    // Keep only identity and artifact readers, never an owned AskState (or
    // an old shared generation) while waiting for the subsequent write.
    let projection = store.project_snapshot(|state| {
        let lease = state.leases.get(session)?;
        if lease.turn_id.as_deref() != Some(turn_id.as_str()) { return None; }
        let mut readers = Vec::new();
        let mut own_message = None;
        if let LeaseHolder::Ask { ask_id, attempt } = &lease.holder {
            if let Some(ask) = state.asks.get(ask_id) {
                if method == "item/completed" && answer::is_final_agent_message(&params["item"]) {
                    readers = AskState::ask_sessions(ask);
                }
                if matches!(method, "item/started" | "item/completed") {
                    own_message = ask.attempts.iter().find(|record| record.n == *attempt)
                        .map(|record| record.native_message_id.clone());
                }
            }
        }
        Some((lease.holder.clone(), readers, own_message))
    })?;
    let Some((holder, readers, own_message)) = projection else {
        return Ok(());
    };
    if let LeaseHolder::Event { event_id } = &holder {
        if method == "thread/compacted" || is_compaction_item(method, params) {
            let event_id = event_id.clone();
            store.with_state(|state| note_event_compaction(state, &event_id, now))?;
            return Ok(());
        }
        if method == "turn/completed" {
            if crate::management::v2::model_overload_retry::holds_default(session, Some(&turn_id)) { return Ok(()); }
            let event_id = event_id.clone();
            store.with_state(|state| {
                super::delivery::acknowledge_event(state, &event_id, &turn_id, now);
                if let Some(event) = state.events.get_mut(&event_id) {
                    event.compaction_reminder_pending = false;
                }
                state.leases.remove(session);
            })?;
        }
        return Ok(());
    }
    let LeaseHolder::Ask { ask_id, attempt } = holder else {
        return Ok(());
    };
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
                        record.activity.last_activity_at=Some(now_rfc3339(now));
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
                    message_artifact = Some(store.put_artifact(text.as_bytes(), "text/markdown; charset=utf-8", &readers, Some(&ask_id), now)?);
                }
            }
            store.with_state(|state| {
                state.update_ask(&ask_id, now, |ask| {
                    let Some(record) = ask.attempts.iter_mut().find(|record| record.n == attempt) else {
                        return;
                    };
                    if method == "item/started" {
                        if is_tool_item(&kind) || kind == "agentMessage" {
                            record.activity.last_activity_at=Some(now_rfc3339(now));
                        }
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
                        record.activity.last_activity_at=Some(now_rfc3339(now));
                        record.last_message_artifact = Some(artifact.clone());
                        // A final answer is already visible. A reminder now
                        // only invites a second summary while completion is
                        // still in flight to the observer.
                        record.compaction_reminder_pending = false;
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
/// Appended to the error of an attempt [`requeue_quota_failure`] put back.
pub const QUOTA_REQUEUED_NOTE: &str =
    "[cutex: the delegation went back to the queue and is delivered again automatically once the usage limit resets.]";

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
    // The runtime's text may say the failed input is not retried (true of
    // the runtime alone); cutex does redeliver it, so say so.
    let error = turn_outcome(turn).1.map(|error| format!("{error} {QUOTA_REQUEUED_NOTE}"));
    store.with_state(|state| {
        // Another end-of-turn path already ended this attempt: nothing to
        // requeue (and not a second `quota_requeues` count).
        if !attempt_open(state, ask_id, attempt)
            || state.asks.get(ask_id).and_then(|a|a.attempts.iter().find(|r|r.n == attempt)).is_some_and(|r| turn.get("id").and_then(Value::as_str).is_some_and(|id|r.turn_id.as_deref()!=Some(id))) {
            return true;
        }
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

/// Whether `attempt` of `ask_id` may still end (checked under the store
/// lock by every end-of-turn writer): it exists, is not terminal (answered,
/// failed, interrupted, refused or yielded) and has no answer yet. A
/// continuation runs as a new attempt, so its turn is not affected.
pub(crate) fn attempt_open(state: &AskState, ask_id: &str, attempt: u32) -> bool {
    state
        .asks
        .get(ask_id)
        .and_then(|ask| ask.attempts.iter().find(|record| record.n == attempt))
        .is_some_and(|record| !record.state.is_terminal())
        && !state.answers.values().any(|answer| answer.ask_id == ask_id && answer.attempt == attempt)
}

/// Capture the answer of an attempt's terminal turn. Idempotent per
/// attempt: a second call for an attempt that already ended writes nothing.
pub fn finalize(
    store: &AskStore,
    session: &str,
    ask_id: &str,
    attempt: u32,
    turn: &Value,
    interrupt_reason: Option<InterruptReason>,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    finalize_inner(store, session, ask_id, attempt, turn, interrupt_reason, now, None, &|| {})
}

pub fn finalize_fenced(
    store: &AskStore, session: &str, ask_id: &str, attempt: u32, turn: &Value,
    interrupt_reason: Option<InterruptReason>, now: DateTime<Utc>, expected: &AdmissionLease,
) -> anyhow::Result<()> {
    finalize_inner(store, session, ask_id, attempt, turn, interrupt_reason, now, Some(expected), &|| {})
}

#[cfg(test)]
pub(crate) fn finalize_fenced_test(
    store: &AskStore, session: &str, ask_id: &str, attempt: u32, turn: &Value,
    interrupt_reason: Option<InterruptReason>, now: DateTime<Utc>, expected: &AdmissionLease,
    before_commit: &dyn Fn(),
) -> anyhow::Result<()> {
    finalize_inner(store, session, ask_id, attempt, turn, interrupt_reason, now, Some(expected), before_commit)
}

fn finalize_inner(
    store: &AskStore, session: &str, ask_id: &str, attempt: u32, turn: &Value,
    interrupt_reason: Option<InterruptReason>, now: DateTime<Utc>, expected: Option<&AdmissionLease>,
    before_commit: &dyn Fn(),
) -> anyhow::Result<()> {
    if crate::management::v2::model_overload_retry::holds_default(session, turn.get("id").and_then(Value::as_str)) && interrupt_reason.is_none() { return Ok(()); }
    let state = store.snapshot()?;
    let Some(ask) = state.asks.get(ask_id).cloned() else {
        return Ok(());
    };
    let Some(record) = ask.attempts.iter().find(|record| record.n == attempt).cloned() else {
        return Ok(());
    };
    if expected.is_some_and(|lease| state.leases.get(session) != Some(lease)
        || record.runtime_generation != lease.runtime_generation
        || record.turn_id != lease.turn_id
        || !matches!(&lease.holder, LeaseHolder::Ask { ask_id: id, attempt: n } if id == ask_id && *n == attempt)) { return Ok(()); }
    if turn.get("id").and_then(Value::as_str).is_some_and(|id| record.turn_id.as_deref().is_some_and(|current| current != id)) { return Ok(()); }
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
    let bounded_context = super::continuation::runtime::attempt_identity(&ask, attempt);
    let bounded_director = ask.bounded_continuation.current.as_ref()
        .and_then(|_| super::continuation::runtime::current_director(&ask));
    before_commit();
    store.with_state(|state| {
        // Decided under the store lock: the observer's `turn/completed` and
        // the scheduler's reconciliation (or a re-delivered notification)
        // may both reach here for one attempt; only the first records the
        // answer and its requester delivery.
        if expected.is_some_and(|lease| state.leases.get(session) != Some(lease))
            || !attempt_open(state, ask_id, attempt)
            || state.asks.get(ask_id).and_then(|a|a.attempts.iter().find(|r|r.n==attempt))
                .is_some_and(|r| r.turn_id != record.turn_id || r.runtime_generation != record.runtime_generation) {
            return;
        }
        if let Some(context) = &bounded_context {
            let end = match status {
                AnswerStatus::Completed if interrupt_reason.is_none() => super::continuation::EndKind::Normal,
                AnswerStatus::Completed | AnswerStatus::Interrupted => super::continuation::EndKind::Interrupted,
                AnswerStatus::Failed => super::continuation::EndKind::Failed,
            };
            if super::continuation::runtime::finish_turn(state, context, end,
                message_artifact.clone(), preview.clone(), now, bounded_director.as_deref()) {
                return;
            }
        }
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
                attempt_record.compaction_reminder_pending = false;
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
            ask.bounded_continuation.cancel("attempt_finalized");
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

    #[test]
    fn caller_projection_keeps_own_message_and_unknown_view_identity() {
        let (world, dir) = world("caller-identity");
        let (store, store_dir) = temp_store("caller-identity");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        let ask = store.with_state(|state| state.create_ask(new_ask("identity", "x"), now).unwrap().0.ask_id).unwrap();
        crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
        let own = store.inspect(|state| state.asks[&ask].attempts[0].native_message_id.clone()).unwrap();
        for (turn, id, view) in [
            ("turn-1", own.as_str(), json!({"schema":"future.unknown"})),
            ("wrong-turn", "wrong", json!({"schema":"future.unknown"})),
            ("turn-1", "no-view", Value::Null),
            ("turn-1", "foreign-note", json!({"schema":"future.unknown"})),
            ("turn-1", "foreign-note", json!({"schema":"future.unknown"})),
        ] {
            observe(&store, TARGET, "item/completed", &item(turn,
                json!({"type":"functionCallOutput", "id":id,"externalInputView":view})), now).unwrap();
        }
        store.inspect(|state| {
            assert_eq!(state.asks[&ask].attempts[0].included_notes, vec!["foreign-note".to_string()]);
            assert!(!state.asks[&ask].attempts[0].owner_steered);
        }).unwrap();
        std::fs::remove_dir_all(store_dir).unwrap();
        let _ = std::fs::remove_dir_all(dir);
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
        assert!(store.read_artifact(answer.artifact_id.as_ref().unwrap(), Some(TARGET)).unwrap().is_some());
        assert!(store.read_artifact(answer.artifact_id.as_ref().unwrap(), Some("unrelated-session")).unwrap().is_none());
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

    /// Two end-of-turn paths for one attempt (the observer's
    /// `turn/completed` and the scheduler's bound-lease reconciliation, or a
    /// re-delivered notification) record one answer and one requester
    /// delivery (owner report 2026-10-16: two identical rows for #78).
    #[test]
    fn an_attempt_ends_once_whoever_reports_it() {
        let (world, dir) = world("finalize-once");
        let (store, store_dir) = temp_store("finalize-once");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        let mut review = new_ask("a", "x");
        review.options.review = true;
        let ask = store.with_state(|state| state.create_ask(review, now).unwrap().0.ask_id).unwrap();
        crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
        observe(&store, TARGET, "turn/started", &json!({"turn": {"id": "turn-1"}}), now).unwrap();
        let turn = json!({"id": "turn-1", "status": "completed", "items": [{"type": "agentMessage", "text": "done"}]});
        // Both read the attempt as running before either wrote.
        finalize(&store, TARGET, &ask, 1, &turn, None, now).unwrap();
        finalize(&store, TARGET, &ask, 1, &turn, Some(InterruptReason::RuntimeRestart), now + chrono::Duration::milliseconds(219)).unwrap();
        // A late re-delivered notification is a no-op as well.
        observe(&store, TARGET, "turn/completed", &json!({"threadId": "t", "turn": turn}), now).unwrap();
        let state = store.snapshot().unwrap();
        assert_eq!(state.answers.len(), 1, "{:?}", state.answers);
        let answer = state.answers.values().next().unwrap();
        assert_eq!((answer.attempt, answer.revision), (1, 1));
        assert_eq!(state.asks[&ask].answer_id.as_deref(), Some(answer.answer_id.as_str()));
        assert_eq!(state.deliveries.values().filter(|d| d.kind == DeliveryKind::Answer).count(), 1);
        assert_eq!(state.notices.iter().filter(|n| n.method == "cutex/ask/answered").count(), 1);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    /// Copies stored by releases before the guard: of two answers of one
    /// attempt only the latest is actionable, shown as an admission row,
    /// claimed by a wait or coalesced into an event; accepting the ask
    /// settles both.
    #[test]
    fn stored_duplicate_answers_show_once() {
        let (world, dir) = world("dup-answers");
        let (store, store_dir) = temp_store("dup-answers");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        let mut review = new_ask("a", "x");
        review.options.review = true;
        let ask = store.with_state(|state| state.create_ask(review, now).unwrap().0.ask_id).unwrap();
        crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
        observe(&store, TARGET, "turn/started", &json!({"turn": {"id": "turn-1"}}), now).unwrap();
        observe(&store, TARGET, "turn/completed", &completed("turn-1", "completed"), now).unwrap();
        // Recreate the old race's copy: a second answer of attempt 1 with
        // its own delivery.
        let (first, copy) = store
            .with_state(|state| {
                let first = state.asks[&ask].answer_id.clone().unwrap();
                let mut copy = state.answers[&first].clone();
                copy.answer_id = "ans-copy".into();
                copy.revision = 2;
                state.answers.insert(copy.answer_id.clone(), copy);
                let updated = state.asks[&ask].clone();
                let delivery = crate::asks::delivery::record_answer_delivery(state, &updated, "ans-copy", now).unwrap();
                state.update_ask(&ask, now, |ask| ask.answer_id = Some("ans-copy".into()));
                let first_delivery = state.deliveries.values().find(|d| d.ref_id == first).unwrap().delivery_id.clone();
                (first_delivery, delivery)
            })
            .unwrap();
        let state = store.snapshot().unwrap();
        assert!(crate::asks::delivery::duplicate_answer(&state, &state.deliveries[&first]));
        assert!(!crate::asks::delivery::actionable(&state, &state.deliveries[&first]));
        assert!(crate::asks::delivery::actionable(&state, &state.deliveries[&copy]));
        let rows = crate::management::v2::admission::ask_next_step(&state, REQUESTER);
        assert_eq!(rows.iter().map(|row| row.id.clone()).collect::<Vec<_>>(), vec![copy.clone()]);
        // An event queued before the guard carrying both: still one row.
        store
            .with_state(|state| {
                for id in [&first, &copy] {
                    let delivery = state.deliveries.get_mut(id).unwrap();
                    delivery.state = DeliveryState::QueuedAsEvent;
                    delivery.event_id = Some("aev-old".into());
                }
            })
            .unwrap();
        let state = store.snapshot().unwrap();
        assert_eq!(crate::management::v2::admission::ask_next_step(&state, REQUESTER).len(), 1);
        // Back to pending: a wait and an event take the latest only.
        store
            .with_state(|state| {
                for id in [&first, &copy] {
                    let delivery = state.deliveries.get_mut(id).unwrap();
                    delivery.state = DeliveryState::Pending;
                    delivery.event_id = None;
                }
            })
            .unwrap();
        let claimed = store.with_state(|state| crate::asks::delivery::claim_for_wait(state, REQUESTER, &[], "w", now)).unwrap();
        assert_eq!(claimed.iter().map(|d| d.delivery_id.clone()).collect::<Vec<_>>(), vec![copy.clone()]);
        store
            .with_state(|state| {
                let delivery = state.deliveries.get_mut(&copy).unwrap();
                delivery.state = DeliveryState::Pending;
                delivery.claimed_by = None;
            })
            .unwrap();
        let event = store.with_state(|state| crate::asks::delivery::coalesce_into_event(state, REQUESTER, "m", now)).unwrap().unwrap();
        assert_eq!(store.snapshot().unwrap().events[&event].delivery_ids, vec![copy.clone()]);
        // Closing the ask settles the copy too.
        store.with_state(|state| crate::asks::delivery::settle_for_ask(state, REQUESTER, &ask, None, "accepted", now)).unwrap();
        assert_eq!(store.snapshot().unwrap().deliveries[&first].state, DeliveryState::Acknowledged);
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
        assert!(record.attempts[0].last_error.as_deref().unwrap().ends_with(QUOTA_REQUEUED_NOTE), "says cutex redelivers it");
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
