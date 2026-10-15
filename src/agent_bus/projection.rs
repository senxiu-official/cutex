//! Model text and display view of an agent message (agent messaging v2),
//! shared by the Codex (ExternalInput) and Claude (adapter) recipients.

use crate::agent_bus::delivery::strip_legacy_sender_prefix;
use crate::agent_bus::model::{AgentBusMessage, AgentSenderFacts};
use crate::app_server::external_input::view::StructuredView;
use crate::management::v2::agent_bus_state::AgentBusMessageMeta;
use anyhow::Context;
use serde_json::{json, Value};

pub const AGENT_MESSAGE_VIEW_SCHEMA: &str = "cutex.agent-message.v1";
const VIEW_LIMIT: usize = 16384;
const NAME_LIMIT: usize = 160;

#[derive(Debug, Clone, PartialEq)]
pub struct AgentMessageProjection {
    pub sender_id: String,
    /// The sender's raw text (legacy default prefix stripped once).
    pub body: String,
    pub text: String,
    pub view: StructuredView,
}

/// `fallback_name` is the sender's current name from the session store, used
/// when the message has no recorded sender party (queued before r208).
pub fn agent_message_projection(
    message: &AgentBusMessage,
    meta: Option<&AgentBusMessageMeta>,
    fallback_name: Option<&str>,
) -> anyhow::Result<AgentMessageProjection> {
    let sender_id = message
        .from_cutex_session_id
        .clone()
        .context("authenticated durable sender absent")?;
    let party = meta
        .and_then(|meta| meta.sender.as_ref())
        .filter(|party| party.id == sender_id);
    let name: Option<String> = party
        .and_then(|party| party.name.clone())
        .or_else(|| fallback_name.map(str::to_string))
        .map(|name| name.chars().filter(|c| !c.is_control()).take(NAME_LIMIT).collect())
        .filter(|name: &String| !name.is_empty());
    let runtime = party.and_then(|party| party.runtime.clone());
    let facts = meta.map(|meta| meta.facts.clone()).unwrap_or_default();
    let body = if meta.is_none() {
        strip_legacy_sender_prefix(&message.content, fallback_name).to_string()
    } else {
        message.content.clone()
    };
    let text = crate::agent_bus::delivery::agent_message_display_text(&sender_id, name.as_deref(), &facts, &body);
    let view = agent_message_view(&sender_id, name.as_deref(), runtime.as_deref(), message, &facts, &body)?;
    Ok(AgentMessageProjection { sender_id, body, text, view })
}

fn agent_message_view(
    sender_id: &str,
    name: Option<&str>,
    runtime: Option<&str>,
    message: &AgentBusMessage,
    facts: &AgentSenderFacts,
    body: &str,
) -> anyhow::Result<StructuredView> {
    let mut data = json!({
        "senderId": sender_id,
        "senderName": name,
        "deliveryMode": message.delivery_mode.event_label(),
        "deliveryKind": message.delivery_mode.kind(),
        "senderRuntime": runtime,
        "body": "",
    });
    if message.created_at_epoch_secs > 0 {
        data["occurredAtEpochSeconds"] = json!(message.created_at_epoch_secs);
    }
    if facts.narrower_capabilities {
        data["narrowerCapabilities"] = json!(true);
    }
    if facts.reply_only {
        data["replyOnly"] = json!(true);
    }
    if facts.wake_downgraded {
        data["wakeDowngraded"] = json!(true);
    }
    let mut view = StructuredView { schema: AGENT_MESSAGE_VIEW_SCHEMA.into(), data };
    // Room for the body (and `"bodyTruncated":true` if it does not fit).
    let base = view.canonical_json()?.len() + r#","bodyTruncated":true"#.len();
    let budget = VIEW_LIMIT.saturating_sub(base);
    let (fitted, truncated) = fit_json_string(body, budget);
    view.data["body"] = Value::String(fitted);
    if truncated {
        view.data["bodyTruncated"] = json!(true);
    }
    view.canonical_json()?;
    Ok(view)
}

/// The longest char prefix of `text` whose JSON string escaping fits `budget`
/// bytes (excluding the quotes).
pub(crate) fn fit_json_string(text: &str, budget: usize) -> (String, bool) {
    let mut used = 0usize;
    let mut buf = [0u8; 4];
    for (index, c) in text.char_indices() {
        let cost = match c {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{08}' | '\u{0c}' => 2,
            c if (c as u32) < 0x20 => 6,
            c => c.encode_utf8(&mut buf).len(),
        };
        if used + cost > budget {
            return (text[..index].to_string(), true);
        }
        used += cost;
    }
    (text.to_string(), false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_bus::delivery::AgentDeliveryMode;
    use crate::agent_bus::model::AgentMessageKind;
    use crate::management::v2::agent_bus_state::AgentParty;

    fn message(content: &str, mode: AgentDeliveryMode) -> AgentBusMessage {
        serde_json::from_value(json!({
            "id": "msg-1", "kind": "message", "from": "lead", "to": "helper",
            "fromCutexSessionId": "cutex.a", "toCutexSessionId": "cutex.b",
            "content": content, "deliveryMode": "after_turn", "triggerTurn": true,
            "createdAtEpochSecs": 1700000000u64
        }))
        .map(|mut m: AgentBusMessage| {
            m.delivery_mode = mode;
            m.sender_kind = AgentMessageKind::Agent;
            m
        })
        .unwrap()
    }

    fn meta(facts: AgentSenderFacts) -> AgentBusMessageMeta {
        AgentBusMessageMeta {
            sender: Some(AgentParty { id: "cutex.a".into(), name: Some("Lead".into()), runtime: Some("codex".into()) }),
            facts,
            ..Default::default()
        }
    }

    #[test]
    fn new_message_text_has_sender_header_and_raw_payload() {
        let m = message("check /tmp/run.log", AgentDeliveryMode::Passive);
        let p = agent_message_projection(&m, Some(&meta(Default::default())), None).unwrap();
        assert_eq!(p.text, "Message Type: MESSAGE\nSender: Lead (cutex.a)\nPayload:\ncheck /tmp/run.log");
        assert_eq!(p.view.data["deliveryKind"], "note");
        assert_eq!(p.view.data["deliveryMode"], "passive");
        assert_eq!(p.view.data["senderRuntime"], "codex");
        assert_eq!(p.view.data["body"], "check /tmp/run.log");
        assert_eq!(p.view.data["occurredAtEpochSeconds"], 1700000000u64);
        assert!(p.view.data.get("bodyTruncated").is_none());
    }

    #[test]
    fn fact_lines_and_flags_follow_sender_facts() {
        let m = message("hi", AgentDeliveryMode::AfterTurn);
        let facts = AgentSenderFacts { narrower_capabilities: true, reply_only: true, wake_downgraded: true, ..Default::default() };
        let p = agent_message_projection(&m, Some(&meta(facts)), None).unwrap();
        assert_eq!(p.text, "Message Type: MESSAGE\nSender: Lead (cutex.a)\nSender capabilities: narrower than yours; this note cannot approve anything for you\nReply: this sender is not in your agent_list; answer with agent_tell to=\"cutex.a\"\nPayload:\nhi");
        assert_eq!(p.view.data["narrowerCapabilities"], true);
        assert_eq!(p.view.data["replyOnly"], true);
        assert_eq!(p.view.data["wakeDowngraded"], true);
        assert_eq!(p.view.data["deliveryKind"], "wake");
    }

    #[test]
    fn legacy_message_strips_default_prefix_once_and_unknown_name_uses_id() {
        let m = message("[message from Lead] [message from Lead] x", AgentDeliveryMode::Soon);
        let p = agent_message_projection(&m, None, Some("Lead")).unwrap();
        assert_eq!(p.body, "[message from Lead] x");
        assert!(p.text.starts_with("Message Type: MESSAGE\nSender: Lead (cutex.a)\nPayload:\n"));
        assert_eq!(p.view.data["senderRuntime"], Value::Null);
        let p = agent_message_projection(&message("y", AgentDeliveryMode::Soon), None, None).unwrap();
        assert_eq!(p.text, "Message Type: MESSAGE\nSender: cutex.a\nPayload:\ny");
        assert_eq!(p.view.data["senderName"], Value::Null);
    }

    #[test]
    fn long_body_is_truncated_in_the_view_only() {
        let body = "\"é\n".repeat(9000);
        let m = message(&body, AgentDeliveryMode::Passive);
        let p = agent_message_projection(&m, Some(&meta(Default::default())), None).unwrap();
        assert!(p.view.canonical_json().unwrap().len() <= VIEW_LIMIT);
        assert_eq!(p.view.data["bodyTruncated"], true);
        assert!(body.starts_with(p.view.data["body"].as_str().unwrap()));
        assert!(p.text.ends_with(&body));
    }
}
