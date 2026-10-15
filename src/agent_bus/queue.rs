//! Agent bus message queue, ack, and send de-duplication helpers.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::anyhow;
use serde_json::Value;
use uuid::Uuid;

use crate::agent_bus::delivery::AgentDeliveryMode;
use crate::agent_bus::model::AgentBusEnvelopeKind;
use crate::agent_bus::model::AgentBusMessage;
use crate::agent_bus::model::AgentBusRecentSend;
use crate::agent_bus::model::AgentBusSendOutcome;
use crate::agent_bus::model::AgentMessageKind;
use crate::agent_bus::model::UserSubmitMode;
use crate::agent_bus::store::prune_recent_agent_sends;
use crate::agent_bus::store::AgentBusState;

#[derive(Debug, Clone)]
pub struct AgentBusSendIdReservation {
    pub message_id: String,
    pub created_at_epoch_secs: u64,
    pub already_enqueued: bool,
}

#[allow(clippy::too_many_arguments)]
pub fn reserve_agent_bus_message_id(
    state: &Arc<Mutex<AgentBusState>>,
    sender: &str,
    target_id: &str,
    content: &str,
    kind: &AgentBusEnvelopeKind,
    delivery_mode: &AgentDeliveryMode,
    sender_kind: &AgentMessageKind,
    display_source: Option<&str>,
    submit_mode: Option<&UserSubmitMode>,
    control_type: Option<&str>,
    control_payload: Option<&Value>,
    external_action_id: Option<&str>,
    external_message_id: Option<&str>,
    now: u64,
) -> anyhow::Result<AgentBusSendIdReservation> {
    let dedupe_key = agent_bus_send_dedupe_key(
        sender,
        target_id,
        content,
        kind,
        delivery_mode,
        sender_kind,
        display_source,
        submit_mode,
        control_type,
        control_payload,
        external_action_id,
        external_message_id,
    );
    let Some(dedupe_key) = dedupe_key else {
        // No explicit message id: every send is a distinct message.
        return Ok(AgentBusSendIdReservation {
            message_id: Uuid::new_v4().to_string(),
            created_at_epoch_secs: now,
            already_enqueued: false,
        });
    };
    let mut state = state
        .lock()
        .map_err(|_| anyhow!("agent bus state lock poisoned"))?;
    prune_recent_agent_sends(&mut state, now);
    if let Some(record) = state.recent_sends.get(&dedupe_key) {
        return Ok(AgentBusSendIdReservation {
            message_id: record.id.clone(),
            created_at_epoch_secs: record.created_at_epoch_secs,
            already_enqueued: true,
        });
    }
    let (message_id, created_at_epoch_secs) = state
        .send_reservations
        .entry(dedupe_key)
        .or_insert_with(|| (Uuid::new_v4().to_string(), now));
    Ok(AgentBusSendIdReservation {
        message_id: message_id.clone(),
        created_at_epoch_secs: *created_at_epoch_secs,
        already_enqueued: false,
    })
}

pub fn ack_agent_messages(
    state: &Arc<Mutex<AgentBusState>>,
    agent_id: &str,
    message_ids: &[String],
) -> anyhow::Result<usize> {
    if message_ids.is_empty() {
        return Ok(0);
    }
    let message_ids = message_ids
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let mut state = state
        .lock()
        .map_err(|_| anyhow!("agent bus state lock poisoned"))?;
    let Some(queue) = state.messages.get_mut(agent_id) else {
        return Ok(0);
    };
    let before = queue.len();
    queue.retain(|message| !message_ids.contains(message.id.as_str()));
    let acked = before.saturating_sub(queue.len());
    if queue.is_empty() {
        state.messages.remove(agent_id);
    }
    Ok(acked)
}

/// One message held in the bus's in-memory queue for a registered runtime.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueuedAgentBusMessage {
    pub message_id: String,
    pub agent_id: String,
    pub agent_name: Option<String>,
    pub to_cutex_session_id: Option<String>,
    pub created_at_epoch_secs: u64,
    pub sender: String,
    pub sender_kind: String,
    pub control_type: Option<String>,
    pub content_preview: String,
}

/// Every message in every runtime's in-memory queue (owner stuck-message
/// view; `GET /api/messages/queued`).
pub fn queued_agent_messages(
    state: &Arc<Mutex<AgentBusState>>,
) -> anyhow::Result<Vec<QueuedAgentBusMessage>> {
    let state = state
        .lock()
        .map_err(|_| anyhow!("agent bus state lock poisoned"))?;
    let mut queued = state
        .messages
        .iter()
        .flat_map(|(agent_id, queue)| {
            let agent_name = state.agents.get(agent_id).map(|agent| agent.name.clone());
            queue.iter().map(move |message| QueuedAgentBusMessage {
                message_id: message.id.clone(),
                agent_id: agent_id.clone(),
                agent_name: agent_name.clone(),
                to_cutex_session_id: message.to_cutex_session_id.clone(),
                created_at_epoch_secs: message.created_at_epoch_secs,
                sender: message.from.clone(),
                sender_kind: message.sender_kind.sender_label().to_string(),
                control_type: message.control_type.clone(),
                content_preview: crate::agent_bus::audit::content_preview(&message.content, 200),
            })
        })
        .collect::<Vec<_>>();
    queued.sort_by(|a, b| {
        (a.created_at_epoch_secs, &a.message_id, &a.agent_id)
            .cmp(&(b.created_at_epoch_secs, &b.message_id, &b.agent_id))
    });
    Ok(queued)
}

/// Remove these message ids from every runtime's in-memory queue (owner
/// discard: the record is already terminal, so no redrive brings them
/// back). Returns `(agent_id, message_id)` pairs removed.
pub fn withdraw_agent_messages(
    state: &Arc<Mutex<AgentBusState>>,
    message_ids: &[String],
) -> anyhow::Result<Vec<(String, String)>> {
    if message_ids.is_empty() {
        return Ok(Vec::new());
    }
    let wanted = message_ids
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let mut state = state
        .lock()
        .map_err(|_| anyhow!("agent bus state lock poisoned"))?;
    let mut withdrawn = Vec::new();
    state.messages.retain(|agent_id, queue| {
        queue.retain(|message| {
            if wanted.contains(message.id.as_str()) {
                withdrawn.push((agent_id.clone(), message.id.clone()));
                false
            } else {
                true
            }
        });
        !queue.is_empty()
    });
    withdrawn.sort();
    Ok(withdrawn)
}

pub fn poll_agent_messages(
    state: &Arc<Mutex<AgentBusState>>,
    agent_id: &str,
    ack_mode: bool,
    now: u64,
) -> anyhow::Result<(String, Vec<AgentBusMessage>)> {
    let mut state = state
        .lock()
        .map_err(|_| anyhow!("agent bus state lock poisoned"))?;
    let agent_name = if let Some(agent) = state.agents.get_mut(agent_id) {
        agent.last_seen_epoch_secs = now;
        agent.name.clone()
    } else {
        agent_id.to_string()
    };
    let messages = if ack_mode {
        state
            .messages
            .get(agent_id)
            .map(|queue| queue.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default()
    } else {
        state
            .messages
            .remove(agent_id)
            .map(|queue| queue.into_iter().collect::<Vec<_>>())
            .unwrap_or_default()
    };
    Ok((agent_name, messages))
}

#[allow(clippy::too_many_arguments)]
pub fn enqueue_agent_bus_message_once(
    state: &Arc<Mutex<AgentBusState>>,
    sender: &str,
    target_id: &str,
    target_name: &str,
    content: &str,
    kind: AgentBusEnvelopeKind,
    delivery_mode: AgentDeliveryMode,
    sender_kind: AgentMessageKind,
    display_source: Option<String>,
    submit_mode: Option<UserSubmitMode>,
    control_type: Option<String>,
    control_payload: Option<Value>,
    external_action_id: Option<String>,
    external_message_id: Option<String>,
    now: u64,
) -> anyhow::Result<AgentBusSendOutcome> {
    enqueue_agent_bus_message_once_with_participants(
        state,
        sender,
        target_id,
        target_name,
        content,
        kind,
        delivery_mode,
        sender_kind,
        display_source,
        submit_mode,
        control_type,
        control_payload,
        external_action_id,
        external_message_id,
        None,
        None,
        now,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn enqueue_agent_bus_message_once_with_participants(
    state: &Arc<Mutex<AgentBusState>>,
    sender: &str,
    target_id: &str,
    target_name: &str,
    content: &str,
    kind: AgentBusEnvelopeKind,
    delivery_mode: AgentDeliveryMode,
    sender_kind: AgentMessageKind,
    display_source: Option<String>,
    submit_mode: Option<UserSubmitMode>,
    control_type: Option<String>,
    control_payload: Option<Value>,
    external_action_id: Option<String>,
    external_message_id: Option<String>,
    from_cutex_session_id: Option<String>,
    to_cutex_session_id: Option<String>,
    now: u64,
) -> anyhow::Result<AgentBusSendOutcome> {
    enqueue_agent_bus_message_once_with_id(
        state,
        sender,
        target_id,
        target_name,
        content,
        kind,
        delivery_mode,
        sender_kind,
        display_source,
        submit_mode,
        control_type,
        control_payload,
        external_action_id,
        external_message_id,
        from_cutex_session_id,
        to_cutex_session_id,
        None,
        now,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn enqueue_agent_bus_message_once_with_id(
    state: &Arc<Mutex<AgentBusState>>,
    sender: &str,
    target_id: &str,
    target_name: &str,
    content: &str,
    kind: AgentBusEnvelopeKind,
    delivery_mode: AgentDeliveryMode,
    sender_kind: AgentMessageKind,
    display_source: Option<String>,
    submit_mode: Option<UserSubmitMode>,
    control_type: Option<String>,
    control_payload: Option<Value>,
    external_action_id: Option<String>,
    external_message_id: Option<String>,
    from_cutex_session_id: Option<String>,
    to_cutex_session_id: Option<String>,
    stable_message_id: Option<String>,
    now: u64,
) -> anyhow::Result<AgentBusSendOutcome> {
    let dedupe_key = agent_bus_send_dedupe_key(
        sender,
        target_id,
        content,
        &kind,
        &delivery_mode,
        &sender_kind,
        display_source.as_deref(),
        submit_mode.as_ref(),
        control_type.as_deref(),
        control_payload.as_ref(),
        external_action_id.as_deref(),
        external_message_id.as_deref(),
    );
    let mut state = state
        .lock()
        .map_err(|_| anyhow!("agent bus state lock poisoned"))?;
    prune_recent_agent_sends(&mut state, now);
    if let Some(record) = dedupe_key
        .as_ref()
        .and_then(|key| state.recent_sends.get(key))
        .cloned()
    {
        return Ok(AgentBusSendOutcome {
            record,
            deduplicated: true,
        });
    }
    if let Some(stable_id) = stable_message_id.as_deref() {
        if let Some(message) = state
            .messages
            .get(target_id)
            .and_then(|queue| queue.iter().find(|message| message.id == stable_id))
            .cloned()
        {
            return Ok(AgentBusSendOutcome {
                record: AgentBusRecentSend {
                    id: message.id,
                    kind: message.kind,
                    from: message.from,
                    to: message.to,
                    to_name: target_name.to_string(),
                    delivery_mode: message.delivery_mode,
                    trigger_turn: message.trigger_turn,
                    queued: true,
                    created_at_epoch_secs: message.created_at_epoch_secs,
                    sender_kind: message.sender_kind,
                    display_source: message.display_source,
                    submit_mode: message.submit_mode,
                    control_type: message.control_type,
                    control_payload: message.control_payload,
                    external_action_id: message.external_action_id,
                    external_message_id: message.external_message_id,
                },
                deduplicated: true,
            });
        }
    }

    let message_id = stable_message_id.unwrap_or_else(|| Uuid::new_v4().to_string());
    let trigger_turn = delivery_mode.trigger_turn();
    let message = AgentBusMessage {
        id: message_id.clone(),
        kind: kind.clone(),
        from: sender.to_string(),
        to: target_id.to_string(),
        from_cutex_session_id,
        to_cutex_session_id,
        content: content.to_string(),
        delivery_mode: delivery_mode.clone(),
        trigger_turn,
        created_at_epoch_secs: now,
        sender_kind: sender_kind.clone(),
        display_source: display_source.clone(),
        submit_mode: submit_mode.clone(),
        control_type: control_type.clone(),
        control_payload: control_payload.clone(),
        external_action_id: external_action_id.clone(),
        external_message_id: external_message_id.clone(),
    };
    state
        .messages
        .entry(target_id.to_string())
        .or_default()
        .push_back(message);
    let record = AgentBusRecentSend {
        id: message_id,
        kind,
        from: sender.to_string(),
        to: target_id.to_string(),
        to_name: target_name.to_string(),
        delivery_mode,
        trigger_turn,
        queued: true,
        created_at_epoch_secs: now,
        sender_kind,
        display_source,
        submit_mode,
        control_type,
        control_payload,
        external_action_id,
        external_message_id,
    };
    if let Some(dedupe_key) = dedupe_key {
        state.recent_sends.insert(dedupe_key, record.clone());
    }
    state
        .send_reservations
        .retain(|_, (reserved_id, _)| reserved_id != &record.id);
    Ok(AgentBusSendOutcome {
        record,
        deduplicated: false,
    })
}

/// Sends are deduplicated only on an explicit, caller-supplied message id
/// (`external_message_id`). Identical content without an id is a new message.
#[allow(clippy::too_many_arguments)]
fn agent_bus_send_dedupe_key(
    from: &str,
    to: &str,
    _content: &str,
    _kind: &AgentBusEnvelopeKind,
    _delivery_mode: &AgentDeliveryMode,
    _sender_kind: &AgentMessageKind,
    _display_source: Option<&str>,
    _submit_mode: Option<&UserSubmitMode>,
    _control_type: Option<&str>,
    _control_payload: Option<&Value>,
    _external_action_id: Option<&str>,
    external_message_id: Option<&str>,
) -> Option<String> {
    let id = external_message_id.filter(|id| !id.is_empty())?;
    Some(format!("{from}\u{1f}{to}\u{1f}{id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ack_removes_only_requested_message_ids() {
        let state = Arc::new(Mutex::new(AgentBusState::default()));
        enqueue_agent_bus_message_once(
            &state,
            "sender",
            "target",
            "target-name",
            "first",
            AgentBusEnvelopeKind::Message,
            AgentDeliveryMode::AfterTurn,
            AgentMessageKind::Agent,
            None,
            None,
            None,
            None,
            None,
            None,
            1,
        )
        .expect("first message should enqueue");
        let second = enqueue_agent_bus_message_once(
            &state,
            "sender",
            "target",
            "target-name",
            "second",
            AgentBusEnvelopeKind::Message,
            AgentDeliveryMode::AfterTurn,
            AgentMessageKind::Agent,
            None,
            None,
            None,
            None,
            None,
            None,
            1,
        )
        .expect("second message should enqueue")
        .record
        .id;

        assert_eq!(
            ack_agent_messages(&state, "target", std::slice::from_ref(&second))
                .expect("ack should work"),
            1
        );
        let state = state.lock().expect("state should lock");
        let queue = state
            .messages
            .get("target")
            .expect("queue should still have first message");
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].content, "first");
    }

    #[test]
    fn withdraw_removes_ids_from_every_queue_and_lists_what_is_queued() {
        let state = Arc::new(Mutex::new(AgentBusState::default()));
        let send = |target: &str, content: &str, now: u64| {
            enqueue_agent_bus_message_once(
                &state,
                "sender",
                target,
                "target-name",
                content,
                AgentBusEnvelopeKind::Message,
                AgentDeliveryMode::AfterTurn,
                AgentMessageKind::Agent,
                None,
                None,
                None,
                None,
                None,
                None,
                now,
            )
            .expect("enqueue")
            .record
            .id
        };
        let old = send("target-a", "old stuck message", 1);
        let keep = send("target-a", "fresh", 5);
        let other = send("target-b", "other runtime", 3);
        let queued = queued_agent_messages(&state).unwrap();
        assert_eq!(
            queued.iter().map(|m| m.message_id.as_str()).collect::<Vec<_>>(),
            vec![old.as_str(), other.as_str(), keep.as_str()]
        );
        assert_eq!(queued[0].agent_id, "target-a");
        assert_eq!(queued[0].content_preview, "old stuck message");

        let withdrawn =
            withdraw_agent_messages(&state, &[old.clone(), other.clone(), "absent".into()]).unwrap();
        assert_eq!(withdrawn.len(), 2);
        assert!(withdrawn.contains(&("target-a".to_string(), old.clone())));
        assert!(withdrawn.contains(&("target-b".to_string(), other)));
        // Idempotent: nothing left to withdraw.
        assert!(withdraw_agent_messages(&state, &[old]).unwrap().is_empty());
        let state = state.lock().unwrap();
        assert!(!state.messages.contains_key("target-b"), "empty queue removed");
        assert_eq!(state.messages["target-a"].len(), 1);
        assert_eq!(state.messages["target-a"][0].id, keep);
    }

    #[test]
    fn identical_content_without_message_id_is_never_deduplicated() {
        let state = Arc::new(Mutex::new(AgentBusState::default()));
        let send = |external_message_id: Option<&str>| {
            enqueue_agent_bus_message_once(
                &state,
                "sender",
                "target",
                "target-name",
                "same text",
                AgentBusEnvelopeKind::Message,
                AgentDeliveryMode::AfterTurn,
                AgentMessageKind::Agent,
                None,
                None,
                None,
                None,
                None,
                external_message_id.map(str::to_string),
                1,
            )
            .unwrap()
        };
        let first = send(None);
        let second = send(None);
        assert!(!first.deduplicated && !second.deduplicated);
        assert_ne!(first.record.id, second.record.id);
        let reserved = |now| {
            reserve_agent_bus_message_id(
                &state,
                "sender",
                "target",
                "same text",
                &AgentBusEnvelopeKind::Message,
                &AgentDeliveryMode::AfterTurn,
                &AgentMessageKind::Agent,
                None,
                None,
                None,
                None,
                None,
                None,
                now,
            )
            .unwrap()
        };
        assert_ne!(reserved(1).message_id, reserved(1).message_id);
        // An explicit id is the only dedupe key, regardless of content.
        let explicit = send(Some("client-1"));
        let retry = send(Some("client-1"));
        assert!(!explicit.deduplicated && retry.deduplicated);
        assert_eq!(explicit.record.id, retry.record.id);
        assert_eq!(state.lock().unwrap().messages["target"].len(), 3);
    }

    #[test]
    fn concurrent_durable_send_reservations_share_one_identity_until_enqueue() {
        let state = Arc::new(Mutex::new(AgentBusState::default()));
        let reserve = |now| {
            reserve_agent_bus_message_id(
                &state,
                "sender",
                "target",
                "hello",
                &AgentBusEnvelopeKind::Message,
                &AgentDeliveryMode::Soon,
                &AgentMessageKind::Agent,
                None,
                None,
                None,
                None,
                None,
                Some("client-message-1"),
                now,
            )
            .expect("reservation should succeed")
        };
        let first = reserve(10);
        let concurrent = reserve(11);
        assert_eq!(first.message_id, concurrent.message_id);
        assert_eq!(
            first.created_at_epoch_secs,
            concurrent.created_at_epoch_secs
        );
        assert!(!first.already_enqueued);
        assert!(!concurrent.already_enqueued);

        let enqueue = |id: String, now| {
            enqueue_agent_bus_message_once_with_id(
                &state,
                "sender",
                "target",
                "target-name",
                "hello",
                AgentBusEnvelopeKind::Message,
                AgentDeliveryMode::Soon,
                AgentMessageKind::Agent,
                None,
                None,
                None,
                None,
                None,
                Some("client-message-1".to_string()),
                Some("cutex.sender".to_string()),
                Some("cutex.target".to_string()),
                Some(id),
                now,
            )
            .expect("enqueue should succeed")
        };
        let original = enqueue(first.message_id.clone(), first.created_at_epoch_secs);
        let replay = enqueue(concurrent.message_id, concurrent.created_at_epoch_secs);
        assert!(!original.deduplicated);
        assert!(replay.deduplicated);
        assert_eq!(original.record.id, replay.record.id);
        assert_eq!(state.lock().unwrap().messages["target"].len(), 1);

        let after_enqueue = reserve(12);
        assert!(after_enqueue.already_enqueued);
        assert_eq!(after_enqueue.message_id, original.record.id);
    }

    #[test]
    fn poll_without_ack_drains_queue_but_ack_mode_keeps_it() {
        let state = Arc::new(Mutex::new(AgentBusState::default()));
        enqueue_agent_bus_message_once(
            &state,
            "sender",
            "target",
            "target-name",
            "hello",
            AgentBusEnvelopeKind::Message,
            AgentDeliveryMode::AfterTurn,
            AgentMessageKind::Agent,
            None,
            None,
            None,
            None,
            None,
            None,
            1,
        )
        .expect("message should enqueue");

        let (_agent_name, messages) =
            poll_agent_messages(&state, "target", true, 2).expect("ack poll should work");
        assert_eq!(messages.len(), 1);
        assert_eq!(
            state
                .lock()
                .expect("state should lock")
                .messages
                .get("target")
                .map(|queue| queue.len()),
            Some(1)
        );

        let (_agent_name, messages) =
            poll_agent_messages(&state, "target", false, 3).expect("drain poll should work");
        assert_eq!(messages.len(), 1);
        assert!(!state
            .lock()
            .expect("state should lock")
            .messages
            .contains_key("target"));
    }
}
