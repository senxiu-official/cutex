//! Returning answers, questions and wake-worthy state changes to the
//! requester (asks design §5.7): one delivery record per item and
//! requester, with exactly one continuation:
//!
//! `pending → claimedByWait` (an `agent_wait` returned it as its tool
//! result; a retried wait with the same `action_id` gets it again), or
//! `pending → queuedAsEvent → acknowledged` (one coalesced ANSWER/QUESTION
//! event admitted through the requester's own scheduler; acknowledged when
//! the receipt shows the turn). A wait can't claim an item already queued as
//! an event, and the other way round.

use chrono::{DateTime, Utc};

use super::model::*;
use super::store::AskState;
use super::{new_id, now_rfc3339};

fn push(state: &mut AskState, ask: &AskRecord, kind: DeliveryKind, ref_id: String, passive: bool, now: DateTime<Utc>) -> Option<String> {
    let requester = ask.requester.session()?.to_string();
    if state
        .deliveries
        .values()
        .any(|delivery| delivery.ask_id == ask.ask_id && delivery.kind == kind && delivery.ref_id == ref_id)
    {
        return None;
    }
    let delivery_id = new_id("dlv");
    state.deliveries.insert(
        delivery_id.clone(),
        DeliveryRecord {
            delivery_id: delivery_id.clone(),
            kind,
            ask_id: ask.ask_id.clone(),
            ref_id,
            requester_cutex_session_id: requester,
            state: DeliveryState::Pending,
            claimed_by: None,
            event_id: None,
            passive,
            settled_by: None,
            created_at: now_rfc3339(now),
            updated_at: now_rfc3339(now),
        },
    );
    Some(delivery_id)
}

/// An answer revision for the requester (none for owner asks: the desktop
/// shows the card instead).
pub fn record_answer_delivery(state: &mut AskState, ask: &AskRecord, answer_id: &str, now: DateTime<Utc>) -> Option<String> {
    push(state, ask, DeliveryKind::Answer, answer_id.to_string(), ask.options.notify == Notify::Passive, now)
}

/// A question routed to the requester. Always actionable (never passive).
pub fn record_question_delivery(state: &mut AskState, ask: &AskRecord, question_id: &str, now: DateTime<Utc>) -> Option<String> {
    push(state, ask, DeliveryKind::Question, question_id.to_string(), false, now)
}

/// A state change that wakes waits (§5.8): paused, blocked, held,
/// rejected, interrupted, failed. Final outcomes are delivered like answers;
/// holds are passive (shown by the next `agent_wait` / `agent_list`).
pub fn record_state_delivery(state: &mut AskState, ask: &AskRecord, reason: &str, now: DateTime<Utc>) -> Option<String> {
    if let Some(inbound) = state.asks.get_mut(&ask.ask_id).and_then(|a|a.peer_inbound.as_mut()) { inbound.last_error = Some(reason.to_string()); }
    let final_outcome = ask.execution.is_terminal();
    let passive = !final_outcome || ask.options.notify == Notify::Passive;
    let ref_id = format!("{reason}@{}", ask.revision);
    push(state, ask, DeliveryKind::State, ref_id, passive, now)
}

/// The state name of a "needs the owner" delivery (r208).
pub const BLOCKED_OWNER_ACTION: &str = "blockedOwnerAction";

/// The target is offline and cutex cannot bring it online (wake policy says
/// never, or the wake failed in a way only the owner can fix): an
/// actionable (non-passive) state item, once per ask, so the requester is
/// woken when idle instead of waiting forever.
pub fn record_owner_action_delivery(state: &mut AskState, ask: &AskRecord, now: DateTime<Utc>) -> Option<String> {
    record_owner_action_delivery_with(state, ask, false, now)
}

/// As [`record_owner_action_delivery`]; `passive` when the requester was
/// already told for this target and reason (`wake::record_owner_action`).
pub fn record_owner_action_delivery_with(state: &mut AskState, ask: &AskRecord, passive: bool, now: DateTime<Utc>) -> Option<String> {
    push(state, ask, DeliveryKind::State, BLOCKED_OWNER_ACTION.to_string(), passive, now)
}

/// The owner stopped the ask's turn (r208): an actionable state item so the
/// requester is woken when idle and decides (retry, reassign, close), also
/// for `notify: passive` asks. One per attempt.
pub fn record_owner_stopped_delivery(state: &mut AskState, ask: &AskRecord, now: DateTime<Utc>) -> Option<String> {
    let attempt = ask.current_attempt().map(|attempt| attempt.n).unwrap_or(0);
    let ref_id = format!("{}@attempt{attempt}", super::render::OWNER_STOPPED_STATE);
    push(state, ask, DeliveryKind::State, ref_id, false, now)
}

/// Whether a pending delivery still needs to reach its requester: not of an
/// ask the requester already settled (accepted, abandoned or cancelled),
/// which can only be left over from releases before r208.
pub fn actionable(state: &AskState, delivery: &DeliveryRecord) -> bool {
    delivery.state == DeliveryState::Pending
        && !state.asks.get(&delivery.ask_id).is_some_and(|ask| {
            matches!(ask.closed, Some(CloseOutcome::Accepted | CloseOutcome::Abandoned | CloseOutcome::Cancelled))
        })
}

/// The requester read or closed the ask: acknowledge its pending answer
/// items (up to `max_revision` when given) and state items, so they are not
/// sent again as an event later. Returns how many were settled.
pub fn settle_for_ask(
    state: &mut AskState,
    requester: &str,
    ask_id: &str,
    max_revision: Option<u32>,
    by: &str,
    now: DateTime<Utc>,
) -> usize {
    let revisions: std::collections::BTreeMap<String, u32> = state
        .answers
        .values()
        .filter(|answer| answer.ask_id == ask_id)
        .map(|answer| (answer.answer_id.clone(), answer.revision))
        .collect();
    let mut settled = 0;
    for delivery in state.deliveries.values_mut() {
        if delivery.ask_id != ask_id
            || delivery.requester_cutex_session_id != requester
            || delivery.state != DeliveryState::Pending
        {
            continue;
        }
        let covered = match delivery.kind {
            DeliveryKind::Answer => match (max_revision, revisions.get(&delivery.ref_id)) {
                (None, _) => true,
                (Some(max), Some(revision)) => *revision <= max,
                (Some(_), None) => false,
            },
            DeliveryKind::State => true,
            DeliveryKind::Question => false,
        };
        if covered {
            delivery.state = DeliveryState::Acknowledged;
            delivery.settled_by = Some(by.to_string());
            delivery.updated_at = now_rfc3339(now);
            settled += 1;
        }
    }
    settled
}

/// Claim deliveries for an `agent_wait`: pending items of `asks` (all of
/// the requester's when empty), plus items this same `action_id` claimed
/// before (a retried wait whose result was lost).
pub fn claim_for_wait(
    state: &mut AskState,
    requester: &str,
    asks: &[String],
    action_id: &str,
    now: DateTime<Utc>,
) -> Vec<DeliveryRecord> {
    let mut claimed = Vec::new();
    for delivery in state.deliveries.values_mut() {
        if delivery.requester_cutex_session_id != requester
            || (!asks.is_empty() && !asks.contains(&delivery.ask_id))
        {
            continue;
        }
        let mine_before = delivery.state == DeliveryState::ClaimedByWait
            && delivery.claimed_by.as_deref() == Some(action_id);
        if delivery.state == DeliveryState::Pending {
            delivery.state = DeliveryState::ClaimedByWait;
            delivery.claimed_by = Some(action_id.to_string());
            delivery.updated_at = now_rfc3339(now);
            claimed.push(delivery.clone());
        } else if mine_before {
            claimed.push(delivery.clone());
        }
    }
    claimed.sort_by(|left, right| left.created_at.cmp(&right.created_at));
    claimed
}

/// Requesters with actionable (non-passive) pending deliveries.
pub fn requesters_with_pending(state: &AskState) -> Vec<String> {
    let mut sessions: Vec<String> = state
        .deliveries
        .values()
        .filter(|delivery| actionable(state, delivery) && !delivery.passive)
        .map(|delivery| delivery.requester_cutex_session_id.clone())
        .collect();
    sessions.sort();
    sessions.dedup();
    sessions
}

/// Move a requester's actionable pending deliveries into one event record.
/// Returns the event id, or `None` when there is nothing to send.
pub fn coalesce_into_event(state: &mut AskState, requester: &str, native_message_id: &str, now: DateTime<Utc>) -> Option<String> {
    let ids: Vec<String> = state
        .deliveries
        .values()
        .filter(|delivery| {
            delivery.requester_cutex_session_id == requester && actionable(state, delivery) && !delivery.passive
        })
        .map(|delivery| delivery.delivery_id.clone())
        .collect();
    coalesce_selected_into_event(state, requester, native_message_id, ids, now)
}

/// Claim the selected ridealong deliveries into one event.
pub fn coalesce_selected_into_event(state: &mut AskState, requester: &str, native_message_id: &str, ids: Vec<String>, now: DateTime<Utc>) -> Option<String> {
    let ids: Vec<String> = ids.into_iter().filter(|id| {
        state.deliveries.get(id).is_some_and(|delivery| {
            delivery.requester_cutex_session_id == requester && actionable(state, delivery) && !delivery.passive
        })
    }).collect();
    if ids.is_empty() {
        return None;
    }
    let event_id = new_id("aev");
    for id in &ids {
        if let Some(delivery) = state.deliveries.get_mut(id) {
            delivery.state = DeliveryState::QueuedAsEvent;
            delivery.event_id = Some(event_id.clone());
            delivery.updated_at = now_rfc3339(now);
        }
    }
    state.events.insert(
        event_id.clone(),
        EventOutboxRecord {
            event_id: event_id.clone(),
            cutex_session_id: requester.to_string(),
            delivery_ids: ids,
            native_message_id: native_message_id.to_string(),
            event_artifact: None,
            turn_id: None,
            acknowledged: false,
            created_at: now_rfc3339(now),
            view: None,
            compacted_at: None,
            compaction_reminder_pending: false,
        },
    );
    Some(event_id)
}

/// The receipt shows the event's turn: acknowledge every delivery in it.
pub fn acknowledge_event(state: &mut AskState, event_id: &str, turn_id: &str, now: DateTime<Utc>) {
    let Some(event) = state.events.get_mut(event_id) else {
        return;
    };
    event.turn_id = Some(turn_id.to_string());
    event.acknowledged = true;
    for id in event.delivery_ids.clone() {
        if let Some(delivery) = state.deliveries.get_mut(&id) {
            delivery.state = DeliveryState::Acknowledged;
            delivery.updated_at = now_rfc3339(now);
        }
    }
}

/// Nothing was delivered (refused / absent): put the items back.
pub fn release_event(state: &mut AskState, event_id: &str, now: DateTime<Utc>) {
    let Some(event) = state.events.remove(event_id) else {
        return;
    };
    for id in event.delivery_ids {
        if let Some(delivery) = state.deliveries.get_mut(&id) {
            if delivery.state == DeliveryState::QueuedAsEvent {
                delivery.state = DeliveryState::Pending;
                delivery.event_id = None;
                delivery.updated_at = now_rfc3339(now);
            }
        }
    }
}

/// Over budget: degrade a requester's pending event deliveries to passive
/// (§8.3); the owner is notified by the caller.
pub fn degrade_to_passive(state: &mut AskState, requester: &str, now: DateTime<Utc>) -> usize {
    let mut count = 0;
    for delivery in state.deliveries.values_mut() {
        if delivery.requester_cutex_session_id == requester && delivery.state == DeliveryState::Pending && !delivery.passive {
            delivery.passive = true;
            delivery.updated_at = now_rfc3339(now);
            count += 1;
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::store::tests::{new_ask, REQUESTER};

    #[test]
    fn one_continuation_per_answer() {
        let mut state = AskState::default();
        let now = Utc::now();
        let (ask, _) = state.create_ask(new_ask("a", "x"), now).unwrap();
        let (other, _) = state.create_ask(new_ask("b", "y"), now).unwrap();
        let first = record_answer_delivery(&mut state, &ask, "ans-1", now).unwrap();
        assert!(record_answer_delivery(&mut state, &ask, "ans-1", now).is_none(), "deduplicated");
        record_answer_delivery(&mut state, &other, "ans-2", now).unwrap();
        // A wait on `ask` claims only its answer; a retry with the same action id gets it again.
        let claimed = claim_for_wait(&mut state, REQUESTER, &[ask.ask_id.clone()], "w1", now);
        assert_eq!(claimed.len(), 1);
        assert_eq!(claim_for_wait(&mut state, REQUESTER, &[ask.ask_id.clone()], "w1", now).len(), 1);
        assert!(claim_for_wait(&mut state, REQUESTER, &[ask.ask_id.clone()], "w2", now).is_empty());
        // The other answer becomes one event; a wait can't claim it then.
        assert_eq!(requesters_with_pending(&state), vec![REQUESTER.to_string()]);
        let event = coalesce_into_event(&mut state, REQUESTER, "msg-1", now).unwrap();
        assert!(claim_for_wait(&mut state, REQUESTER, &[], "w3", now).is_empty());
        assert!(coalesce_into_event(&mut state, REQUESTER, "msg-2", now).is_none());
        release_event(&mut state, &event, now);
        let event = coalesce_into_event(&mut state, REQUESTER, "msg-3", now).unwrap();
        acknowledge_event(&mut state, &event, "turn-1", now);
        assert!(state.deliveries.values().any(|d| d.state == DeliveryState::Acknowledged));
        assert_eq!(state.deliveries[&first].state, DeliveryState::ClaimedByWait);
    }

    #[test]
    fn owner_asks_have_no_deliveries_and_passive_ones_are_not_events() {
        let mut state = AskState::default();
        let now = Utc::now();
        let mut owner = new_ask("o", "x");
        owner.requester = Requester::Owner;
        let (owner_ask, _) = state.create_ask(owner, now).unwrap();
        assert!(record_answer_delivery(&mut state, &owner_ask, "ans", now).is_none());
        let mut passive = new_ask("p", "x");
        passive.options.notify = Notify::Passive;
        let (passive_ask, _) = state.create_ask(passive, now).unwrap();
        record_answer_delivery(&mut state, &passive_ask, "ans", now).unwrap();
        assert!(requesters_with_pending(&state).is_empty());
        assert_eq!(claim_for_wait(&mut state, REQUESTER, &[], "w", now).len(), 1);
        let (normal, _) = state.create_ask(new_ask("n", "x"), now).unwrap();
        record_answer_delivery(&mut state, &normal, "ans-n", now).unwrap();
        assert_eq!(degrade_to_passive(&mut state, REQUESTER, now), 1);
        assert!(requesters_with_pending(&state).is_empty());
    }
}
