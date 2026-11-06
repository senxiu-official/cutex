//! Permanent pre-dispatch refusals must not retain a shared queue position.
//! Unknown authority/lineage and review waits remain fail-closed and retryable.
use super::{model::ExecutionState, routing_policy::RoutingError, store::{AskError, AskState}};
use chrono::{DateTime, Utc};

pub(super) fn permanent(code: &str) -> bool {
    use RoutingError::*;
    [RootClosed, ScopeMismatch, InvalidIdentifier, InvalidGrant, GrantNotFound,
        GrantRevoked, GrantExpired, GrantExhausted, GrantStale, TargetNotGranted,
        ActionPayloadConflict, DigestMismatch]
        .into_iter().any(|error| error.code() == code)
}

/// Use only while holding the existing authority -> AskStore transaction.
/// No manufactured attempt and no changes to another ask, lease or input.
pub(super) fn end(state: &mut AskState, ask_id: &str, error: &AskError, now: DateTime<Utc>) -> bool {
    if !permanent(error.code) || !state.asks.get(ask_id).is_some_and(|ask|
        ask.is_open() && (matches!(ask.execution, ExecutionState::Queued | ExecutionState::PendingReview) || ask.resume_pending())) {
        return false;
    }
    state.update_ask(ask_id, now, |ask| {
        ask.admission_failure = Some(error.code.into());
        ask.execution = ExecutionState::Failed;
        ask.hold = None;
        ask.closed = Some(super::model::CloseOutcome::Completed);
        ask.closed_at = Some(super::now_rfc3339(now));
    });
    if let Some(ask) = state.asks.get(ask_id).cloned() {
        super::delivery::record_state_delivery(state, &ask, error.code, now);
    }
    true
}
