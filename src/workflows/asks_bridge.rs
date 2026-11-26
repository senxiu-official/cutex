//! Step asks (design §3.1): every `agent()` call is an ask to its hidden step
//! session, so admission, stop, restart honesty, progress and usage are the
//! asks machinery.
//!
//! W1 uses `Requester::Owner` for step asks (never gated, no returns to an
//! agent, questions routed to the owner) instead of a new `Requester::Workflow`
//! variant: an older cutex could not parse an asks store holding the new tag,
//! which would make rolling back lossy. The run ↔ ask link lives in the
//! workflow journal; `actionId = <runId>:<stepKey>[:repair]` keeps a re-executed
//! script on the same ask.

use std::collections::HashMap;

use serde_json::Value;

use crate::asks::model::*;
use crate::asks::store::AskStore;

use super::model::RunError;

#[derive(Clone, Debug, PartialEq)]
pub enum AskPoll {
    Pending { activity: Option<String>, tokens: u64, blocked: bool, quota_reset_known: bool },
    Answered { text: String, tokens: u64 },
    Failed { code: String, message: String, tokens: u64 },
    Missing,
}

/// The owner ask action ids emitted by the workflow engine. Repair attempts
/// have the same prefix and stay subject to the same redelivery rules.
pub fn is_workflow_step(ask: &AskRecord) -> bool {
    if !matches!(ask.requester, Requester::Owner) {
        return false;
    }
    let mut parts = ask.action_id.split(':');
    let (Some(run), Some(step)) = (parts.next(), parts.next()) else { return false };
    let Some(run) = run.strip_prefix("wr-") else { return false };
    let Some((digest, occurrence)) = step.split_once('#') else { return false };
    run.len() == 16 && run.bytes().all(|b| b.is_ascii_hexdigit())
        && digest.len() == 16 && digest.bytes().all(|b| b.is_ascii_hexdigit())
        && occurrence.parse::<u64>().is_ok_and(|number| number > 0)
        && matches!(parts.next(), None | Some("repair")) && parts.next().is_none()
}

pub fn quota_reset_matches(
    ask: &AskRecord, pending: &crate::management::v2::quota_continue::Pending,
) -> bool {
    !pending.estimated && ask.current_attempt().is_some_and(|attempt| {
        attempt.runtime_generation == pending.runtime_generation
            && attempt.turn_id.is_some()
            && attempt.turn_id == pending.turn_id
    })
}

pub(crate) fn step_may_have_effects(ask: &AskRecord, attempt: &AttemptRecord) -> bool {
    attempt.activity.tool_calls != 0
        || !attempt.activity.in_flight_tools.is_empty()
        || attempt.native_diff_artifact.is_some()
        || !ask.jobs.is_empty()
}

fn run_error(code: &str, message: impl Into<String>) -> RunError {
    RunError { code: code.into(), message: message.into() }
}

/// Input + output tokens from a native `tokenUsage` value (Codex
/// `{total:{inputTokens,outputTokens,…}}`, or a flat object).
pub fn tokens_of(usage: Option<&Value>) -> u64 {
    let Some(usage) = usage else { return 0 };
    let source = usage.get("total").unwrap_or(usage);
    let field = |k: &str| source.get(k).and_then(Value::as_u64).unwrap_or(0);
    let sum = field("inputTokens") + field("outputTokens");
    if sum > 0 {
        sum
    } else {
        field("totalTokens")
    }
}

fn ask_tokens(ask: &AskRecord) -> u64 {
    ask.attempts.iter().map(|a| tokens_of(a.token_usage.as_ref())).max().unwrap_or(0)
}

pub fn poll(store: &AskStore, ask_ids: &[String]) -> HashMap<String, AskPoll> {
    let mut out = HashMap::new();
    let state = match store.peek() {
        Ok(Some(state)) => state,
        _ => {
            for id in ask_ids {
                out.insert(id.clone(), AskPoll::Missing);
            }
            return out;
        }
    };
    let has_quota_holds = ask_ids.iter().any(|id| state.asks.get(id)
        .is_some_and(|ask| matches!(ask.hold, Some(Hold::Blocked { by: BlockedReason::Quota }))));
    let quota_state = has_quota_holds.then(|| {
        crate::management::v2::quota_continue::Store::open_default()
            .and_then(|store| store.snapshot()).ok()
    }).flatten();
    for id in ask_ids {
        let history;
        let state = if state.asks.contains_key(id) {
            &state
        } else {
            history = store.lookup_snapshot(&[id]).ok();
            history.as_ref().unwrap_or(&state)
        };
        let Some(ask) = state.asks.get(id) else {
            out.insert(id.clone(), AskPoll::Missing);
            continue;
        };
        let tokens = ask_tokens(ask);
        let poll = match ask.execution {
            ExecutionState::Answered => {
                let text = ask
                    .answer_id
                    .as_ref()
                    .and_then(|answer| state.answers.get(answer))
                    .and_then(|answer| answer.artifact_id.clone())
                    .and_then(|artifact| store.read_artifact(&artifact, None).ok().flatten())
                    .map(|(_, bytes)| String::from_utf8_lossy(&bytes).into_owned());
                match text {
                    Some(text) => AskPoll::Answered { text, tokens },
                    None => AskPoll::Failed { code: "outcome_unknown".into(), message: "the step's final answer is unavailable".into(), tokens },
                }
            }
            ExecutionState::Failed => AskPoll::Failed {
                code: if ask.current_attempt().is_some_and(|attempt| {
                    attempt.handler_failure.as_ref().is_some_and(|failure| matches!(
                        failure.kind,
                        crate::asks::handler_failure::FailureKind::Quota
                            | crate::asks::handler_failure::FailureKind::RateLimited
                    )) && step_may_have_effects(ask, attempt)
                }) { "outcome_unknown" } else { "step_failed" }.into(),
                message: ask.attempts.last().and_then(|a| a.last_error.clone()).unwrap_or_else(|| "the step's turn failed".into()),
                tokens,
            },
            ExecutionState::Interrupted => {
                let reason = ask.attempts.last().and_then(|a| a.interrupt_reason);
                let code = match reason {
                    Some(InterruptReason::RuntimeRestart) => "runtime_restart",
                    Some(InterruptReason::OutcomeUnknown) => "outcome_unknown",
                    Some(InterruptReason::Stopped) | Some(InterruptReason::OwnerStopped) => "stopped",
                    _ => "interrupted",
                };
                AskPoll::Failed { code: code.into(), message: "the step's turn was interrupted".into(), tokens }
            }
            ExecutionState::Cancelled | ExecutionState::Rejected => {
                AskPoll::Failed { code: "stopped".into(), message: "the step was stopped".into(), tokens }
            }
            ExecutionState::PendingReview | ExecutionState::Queued | ExecutionState::Admitted | ExecutionState::Running => {
                let quota = matches!(ask.hold, Some(Hold::Blocked { by: BlockedReason::Quota }));
                let blocked = quota || matches!(ask.hold, Some(Hold::Blocked { by: BlockedReason::Login }));
                let quota_reset_known = quota && {
                    quota_state.as_ref().and_then(|state| state.sessions.get(&ask.target_cutex_session_id))
                        .and_then(|session| session.pending.as_ref())
                        .is_some_and(|pending| quota_reset_matches(ask, pending))
                };
                let activity = ask.attempts.last().and_then(|a| a.activity.line.clone());
                AskPoll::Pending { activity, tokens, blocked, quota_reset_known }
            }
        };
        out.insert(id.clone(), poll);
    }
    out
}

/// Create (or find again, same actionId) the owner ask of one step.
pub fn create_step_ask(
    store: &AskStore,
    session_id: &str,
    action_id: &str,
    title: &str,
    text: &str,
) -> Result<String, RunError> {
    let world = crate::asks::scheduler::HostWorld::load();
    let now = chrono::Utc::now();
    let directory = crate::asks::scheduler::SchedulerWorld::directory(&world).map_err(|e| run_error(e.code, e.detail.clone()))?;
    let target = directory.resolve(None, session_id).map_err(|e| run_error(e.code, e.detail.clone()))?.clone();
    let body = crate::asks::tools::ask_body(store, text, Vec::new(), &[target.cutex_session_id.clone()], now)
        .map_err(|e| run_error(e.code, e.detail.clone()))?;
    let new = crate::asks::store::NewAsk {
        ask_id: None,
        action_id: action_id.to_string(),
        requester: Requester::Owner,
        target_cutex_session_id: target.cutex_session_id.clone(),
        target_name: target.name.clone(),
        project_id: target.project.clone(),
        body,
        title: Some(title.chars().take(120).collect()),
        options: AskOptions {
            if_busy: IfBusy::Queue,
            review: false,
            questions: QuestionRoute::Owner,
            notify: Notify::Turn,
            context: ContextOption::Brief,
        },
        follow_up_of: None,
        parent_ask: None,
        gate: None,
    };
    let (ask, _) = crate::asks::tools::create_ask(store, &world, new, now).map_err(|e| run_error(e.code, e.detail.clone()))?;
    Ok(ask.ask_id)
}

/// Stop an open step ask (owner authority).
pub fn stop_ask(store: &AskStore, ask_id: &str) {
    let _ = crate::asks::tools::close_ask_with(store, None, ask_id, "stop", false, chrono::Utc::now());
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn token_usage_shapes() {
        assert_eq!(tokens_of(Some(&json!({"total": {"inputTokens": 10, "outputTokens": 5, "totalTokens": 99}}))), 15);
        assert_eq!(tokens_of(Some(&json!({"totalTokens": 7}))), 7);
        assert_eq!(tokens_of(None), 0);
    }
}
