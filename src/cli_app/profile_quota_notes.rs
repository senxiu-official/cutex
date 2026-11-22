//! Passive quota notes use the normal durable bus inbox and never start a turn.
use cutex::agent_bus::{
    delivery::AgentDeliveryMode,
    model::{AgentBusEnvelopeKind, AgentBusMessage, AgentMessageKind},
    store::AgentBusState,
};
use std::sync::{Arc, Mutex};
pub(super) fn spawn(state: Arc<Mutex<AgentBusState>>) -> anyhow::Result<()> {
    cutex::profile_quota_agents::alerts::spawn(move |note| {
        let target = cutex::agent_bus::routing::resolve_agent_target_for_sender_with_sessions(
            &state,
            &note.recipient,
            None,
            true,
            None,
        )?;
        let repo = cutex::management::v2::agent_bus_state::agent_bus_message_repository()?;
        if repo
            .snapshot_by_message_id(&note.id)?
            .is_some_and(|r| r.state != "pending")
        {
            return Ok(());
        }
        let mut message = match repo.snapshot_by_message_id(&note.id)? {
            Some(_) => repo.canonical_message(&note.id)?,
            None => AgentBusMessage {
                id: note.id.clone(),
                kind: AgentBusEnvelopeKind::Message,
                from: "cutex.profile-quota".into(),
                to: target.clone(),
                from_cutex_session_id: None,
                to_cutex_session_id: Some(note.recipient.clone()),
                content: note.text.clone(),
                delivery_mode: AgentDeliveryMode::Passive,
                trigger_turn: false,
                created_at_epoch_secs: chrono::Utc::now().timestamp() as u64,
                sender_kind: AgentMessageKind::Agent,
                display_source: Some("cutex.profile-quota".into()),
                submit_mode: None,
                control_type: None,
                control_payload: None,
                external_action_id: None,
                external_message_id: Some(note.id.clone()),
            },
        };
        let params =
            cutex::app_server::bus_bridge::inter_agent_params("", "", &note.recipient, &message)?;
        repo.record_queued(
            cutex::management::v2::agent_bus_state::AgentBusQueuedMessage {
                owner_cutex_session_id: note.recipient.clone(),
                message_id: note.id.clone(),
                from_cutex_session_id: None,
                to_cutex_session_id: note.recipient.clone(),
                from_runtime_agent_id: None,
                to_runtime_agent_id: Some(target.clone()),
                delivery_mode: "passive".into(),
                content: message.content.clone(),
                queued_at: chrono::Utc::now(),
                canonical_envelope: message.clone(),
                semantic_sha256: cutex::app_server::bus_bridge::inter_agent_semantic_sha256(
                    &params,
                ),
            },
        )?;
        message.to = target.clone();
        cutex::agent_bus::queue::enqueue_agent_bus_message_once_with_id(
            &state,
            &message.from,
            &target,
            &note.recipient,
            &message.content,
            message.kind,
            message.delivery_mode,
            message.sender_kind,
            message.display_source,
            message.submit_mode,
            message.control_type,
            message.control_payload,
            message.external_action_id,
            message.external_message_id,
            message.from_cutex_session_id,
            message.to_cutex_session_id,
            Some(note.id.clone()),
            message.created_at_epoch_secs,
        )?;
        cutex::agent_bus::server::notify_agent_bus_message_available();
        Ok(())
    })
}
