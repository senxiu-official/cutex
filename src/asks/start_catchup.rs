//! Recover a start notification that raced with binding the submit receipt.
//! An active turn alone proves no input was read. Require the original ASK
//! item in that exact turn, then compare the admission identity again at write.

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::model::{AdmissionLease, AttemptState, ExecutionState, LeaseHolder};
use super::scheduler::AskRuntime;
use super::store::AskStore;

#[cfg(test)]
#[path = "start_catchup_tests.rs"]
mod tests;

pub(super) fn reconcile(
    runtime: &dyn AskRuntime,
    store: &AskStore,
    lease: &AdmissionLease,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    let (LeaseHolder::Ask { ask_id, attempt }, Some(turn_id)) = (&lease.holder, &lease.turn_id)
    else {
        return Ok(());
    };
    let occurrence = store.project_snapshot(|state| {
        if state.leases.get(&lease.cutex_session_id) != Some(lease)
            || !super::observe::attempt_open(state, ask_id, *attempt)
        {
            return None;
        }
        let ask = state.asks.get(ask_id)?;
        if ask.execution.is_terminal() {
            return None;
        }
        let record = ask.current_attempt()?;
        (record.n == *attempt
            && matches!(
                record.state,
                AttemptState::Submitting | AttemptState::Admitted
            )
            && !record.stop_requested
            && record.runtime_generation == lease.runtime_generation
            && record.native_message_id == lease.native_message_id
            && record.turn_id == lease.turn_id)
            .then_some((ask.admission_occurrence, record.admission_occurrence))
    })?;
    let Some(occurrence) = occurrence else {
        return Ok(());
    };
    if !super::lease_reconcile::start_check_due(store.dir(), lease, now) {
        return Ok(());
    }
    // Native IO must not retain the ask-store lock or a whole-state snapshot.
    let Some(turn) = runtime.read_turn(&lease.cutex_session_id, turn_id) else {
        return Ok(());
    };
    if turn["id"].as_str() != Some(turn_id)
        || turn["status"].as_str() != Some("inProgress")
        || !turn["items"].as_array().is_some_and(|items| {
            items.iter().any(|item| {
                item["type"] == "functionCallOutput"
                    && item["id"].as_str() == Some(lease.native_message_id.as_str())
                    && item
                        .pointer("/externalInputView/schema")
                        .and_then(Value::as_str)
                        == Some(super::views::ASK_VIEW)
                    && item
                        .pointer("/externalInputView/data/askId")
                        .and_then(Value::as_str)
                        == Some(ask_id.as_str())
                    && item
                        .pointer("/externalInputView/data/attempt")
                        .and_then(Value::as_u64)
                        == Some(u64::from(*attempt))
            })
        })
    {
        return Ok(());
    }
    let status = runtime.target_status(&lease.cutex_session_id);
    if !status.online
        || status.runtime_generation != lease.runtime_generation
        || status.active_turn_id.as_ref() != Some(turn_id)
    {
        return Ok(());
    }
    store.with_records(&[ask_id], |state| {
        if state.leases.get(&lease.cutex_session_id) != Some(lease)
            || !super::observe::attempt_open(state, ask_id, *attempt)
        {
            return;
        }
        let valid = state
            .asks
            .get(ask_id)
            .filter(|ask| !ask.execution.is_terminal() && ask.admission_occurrence == occurrence.0)
            .and_then(|ask| ask.current_attempt())
            .is_some_and(|record| {
                record.n == *attempt
                    && record.admission_occurrence == occurrence.1
                    && !record.stop_requested
                    && matches!(
                        record.state,
                        AttemptState::Submitting | AttemptState::Admitted
                    )
                    && record.runtime_generation == lease.runtime_generation
                    && record.native_message_id == lease.native_message_id
                    && record.turn_id == lease.turn_id
            });
        if !valid {
            return;
        }
        state.update_ask(ask_id, now, |ask| {
            let record = ask.current_attempt_mut().expect("checked under store lock");
            record.state = AttemptState::Running;
            // This is the time we proved the start, not an invented event time.
            if record.started_at.is_none() {
                record.started_at = Some(super::now_rfc3339(now));
                record.started_at_inferred = true;
            }
            ask.execution = ExecutionState::Running;
        });
    })
}
