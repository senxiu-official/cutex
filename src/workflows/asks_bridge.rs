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
    Pending { activity: Option<String>, tokens: u64, blocked: bool },
    Answered { text: String, tokens: u64 },
    Failed { code: String, message: String, tokens: u64 },
    Missing,
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
    for id in ask_ids {
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
                    None => AskPoll::Failed { code: "no_answer".into(), message: "the step ended without a final message".into(), tokens },
                }
            }
            ExecutionState::Failed => AskPoll::Failed {
                code: "step_failed".into(),
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
                let blocked = matches!(ask.hold, Some(Hold::Blocked { by: BlockedReason::Login | BlockedReason::Quota }));
                let activity = ask.attempts.last().and_then(|a| a.activity.line.clone());
                AskPoll::Pending { activity, tokens, blocked }
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
