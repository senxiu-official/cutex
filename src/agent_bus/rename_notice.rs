//! Passive, durable owner-rename notices. Authority is the transactional outbox.
use super::{
    delivery::AgentDeliveryMode,
    model::{AgentBusEnvelopeKind, AgentBusMessage, AgentMessageKind},
};
use anyhow::{ensure, Context};

pub const CONTROL_TYPE: &str = "cutex.agent.formalNameChanged.v1";
pub const SOURCE_ID: &str = "Cutex Name Service";
pub const EVENT_TYPE: &str = "agent_name_changed";

pub fn message_id(event: &str, recipient: &str) -> String {
    format!(
        "rename-{}",
        crate::role_revision::sha256_bytes(format!("{event}\0{recipient}").as_bytes()).as_str()
    )
}

pub fn is_notice(message: &AgentBusMessage) -> bool {
    message.control_type.as_deref() == Some(CONTROL_TYPE)
}

pub fn model_text(message: &AgentBusMessage) -> anyhow::Result<String> {
    let path = crate::session::store::cutex_sessions_path()?;
    model_text_at(&path, message)
}

pub(crate) fn model_text_at(
    path: &std::path::Path,
    message: &AgentBusMessage,
) -> anyhow::Result<String> {
    ensure!(
        is_notice(message)
            && message.kind == AgentBusEnvelopeKind::Message
            && message.sender_kind == AgentMessageKind::Agent
            && message.from == SOURCE_ID
            && message.from_cutex_session_id.is_none()
            && message.delivery_mode == AgentDeliveryMode::Passive
            && !message.trigger_turn,
        "invalid formal-name notice envelope"
    );
    let recipient = message
        .to_cutex_session_id
        .as_deref()
        .context("rename notice recipient absent")?;
    let payload = message
        .control_payload
        .as_ref()
        .context("rename notice event absent")?;
    let event = payload
        .get("eventId")
        .and_then(serde_json::Value::as_str)
        .context("rename event absent")?;
    ensure!(
        message.id == message_id(event, recipient),
        "rename notice ID mismatch"
    );
    let note = crate::session::store::rename_note_at(path, event, recipient)?
        .context("rename notice has no durable owner event")?;
    ensure!(
        note.text == message.content,
        "rename notice text differs from owner event"
    );
    Ok(format!(
        "Message Type: NOTICE\nSender: {SOURCE_ID}\nPayload:\n{}",
        note.text
    ))
}
