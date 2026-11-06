//! A rename commits its passive notes first; this worker only drains online recipients.
use cutex::agent_bus::{
    delivery::AgentDeliveryMode,
    model::{AgentBusEnvelopeKind, AgentBusMessage, AgentMessageKind},
    rename_notice,
    store::AgentBusState,
};
use cutex::management::v2::agent_bus_state::{agent_bus_message_repository, AgentBusQueuedMessage};
use std::sync::{Arc, Mutex};

pub(super) fn spawn(state: Arc<Mutex<AgentBusState>>) {
    std::thread::Builder::new()
        .name("agent-name-notices".into())
        .spawn(move || loop {
            if let Err(error) = drain(&state) {
                eprintln!("agent-name notices: {error:#}");
            }
            std::thread::sleep(std::time::Duration::from_secs(5));
        })
        .expect("spawn agent-name notice dispatcher");
}

fn drain(state: &Arc<Mutex<AgentBusState>>) -> anyhow::Result<()> {
    let path = cutex::session::store::cutex_sessions_path()?;
    if cutex::session::store::pending_rename_notes_at(&path,1)?.is_empty() {return Ok(())}
    let registered = state
        .lock()
        .map_err(|_| anyhow::anyhow!("agent bus state lock poisoned"))?
        .agents
        .keys()
        .cloned()
        .collect::<std::collections::HashSet<_>>();
    let authority = cutex::agent_management::archive_authority_snapshot()?;
    let sessions=cutex::session::store::with_cutex_session_store(|store| {
        let mut online=cutex::session::model::CutexSessionStore::default();
        for record in store.sessions.values().filter(|record|
            record.current_runtime_agent_id.as_ref().is_some_and(|id|registered.contains(id))) {
            online.sessions.insert(record.cutex_session_id.clone(),record.clone());
        }
        online
    })?;
    let online = sessions
        .sessions
        .values()
        .filter(|record| {
            record.runtime_generation > 0
                && record
                    .current_runtime_agent_id
                    .as_ref()
                    .is_some_and(|id| registered.contains(id))
                && !cutex::agent_management::agent_execution_blocked_in(&authority, record)
        })
        .map(|record| record.cutex_session_id.clone())
        .collect::<Vec<_>>();
    for note in cutex::session::store::pending_rename_notes_for_at(&path, &online, 100)? {
        // Runtime occurrence and lifecycle are checked by the durable-ID route.
        let target = match cutex::agent_bus::routing::resolve_agent_target_for_sender_with_sessions(
            state,
            &note.recipient_session_id,
            None,
            true,
            Some(&sessions),
        ) {
            Ok(target) => target,
            Err(_) => continue,
        };
        let repo = agent_bus_message_repository()?;
        let id = rename_notice::message_id(&note.event_id, &note.recipient_session_id);
        if repo
            .snapshot_by_message_id(&id)?
            .is_some_and(|record| record.state != "pending")
        {
            cutex::session::store::mark_rename_note_delivered_at(
                &path,
                &note.event_id,
                &note.recipient_session_id,
            )?;
            continue;
        }
        let mut message = match repo.snapshot_by_message_id(&id)? {
            Some(_) => repo.canonical_message(&id)?,
            None => AgentBusMessage {
                id: id.clone(),
                kind: AgentBusEnvelopeKind::Message,
                from: rename_notice::SOURCE_ID.into(),
                to: target.clone(),
                from_cutex_session_id: None,
                to_cutex_session_id: Some(note.recipient_session_id.clone()),
                content: note.text.clone(),
                delivery_mode: AgentDeliveryMode::Passive,
                trigger_turn: false,
                created_at_epoch_secs: chrono::Utc::now().timestamp() as u64,
                sender_kind: AgentMessageKind::Agent,
                display_source: Some(rename_notice::SOURCE_ID.into()),
                submit_mode: None,
                control_type: Some(rename_notice::CONTROL_TYPE.into()),
                control_payload: Some(serde_json::json!({"eventId":note.event_id})),
                external_action_id: None,
                external_message_id: Some(id.clone()),
            },
        };
        let params = cutex::app_server::bus_bridge::inter_agent_params(
            "",
            "",
            &note.recipient_session_id,
            &message,
        )?;
        repo.record_queued(AgentBusQueuedMessage {
            owner_cutex_session_id: note.recipient_session_id.clone(),
            message_id: id.clone(),
            from_cutex_session_id: None,
            to_cutex_session_id: note.recipient_session_id.clone(),
            from_runtime_agent_id: None,
            to_runtime_agent_id: Some(target.clone()),
            delivery_mode: "passive".into(),
            content: note.text.clone(),
            queued_at: chrono::Utc::now(),
            canonical_envelope: message.clone(),
            semantic_sha256: cutex::app_server::bus_bridge::inter_agent_semantic_sha256(&params),
        })?;
        message.to = target.clone();
        cutex::agent_bus::queue::enqueue_agent_bus_message_once_with_id(
            state,
            &message.from,
            &target,
            &note.recipient_session_id,
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
            Some(id),
            message.created_at_epoch_secs,
        )?;
        cutex::session::store::mark_rename_note_delivered_at(
            &path,
            &note.event_id,
            &note.recipient_session_id,
        )?;
        cutex::agent_bus::server::notify_agent_bus_message_available();
    }
    Ok(())
}
