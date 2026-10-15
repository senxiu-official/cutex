//! Cross-host `agent_tell` notes (docs/PEERS.md "Cross-host tells").
//!
//! The receiving Management daemon enqueues a peer tell on the local Agent Bus
//! as an ordinary passive agent message with a reserved control record. The
//! remote sender never becomes a local session or Agent Bus identity: the
//! durable message has no `fromCutexSessionId`, and recipients see a service
//! source (`cutex-peers`) whose text names the remote agent as
//! `<name>@<peer label>`. Older binaries skip this control type.
use crate::agent_bus::delivery::AgentDeliveryMode;
use crate::agent_bus::model::{AgentBusEnvelopeKind, AgentBusMessage, AgentBusSendRequest};
use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};

pub const CONTROL_TYPE: &str = "cutex.peer.tell.v1";
pub const SOURCE_ID: &str = "cutex-peers";
pub const EVENT_TYPE: &str = "peer_message";
pub const MESSAGE_ID_PREFIX: &str = "ptell-";

/// Provenance the receiving daemon established from the authenticated peer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerTellFacts {
    /// The sending host's instance ID (`ci-…`).
    pub peer_id: String,
    /// This host's label for the sending host.
    pub host: String,
    /// The sender's session ID on its own host.
    pub sender_id: String,
    #[serde(default)]
    pub sender_name: Option<String>,
    pub tell_id: String,
}

impl PeerTellFacts {
    /// `<name>@<host>` (the remote session ID when it has no name).
    pub fn sender_label(&self) -> String {
        let name = self
            .sender_name
            .as_deref()
            .map(crate::peers::wire::sanitize_name)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| crate::peers::wire::sanitize_name(&self.sender_id));
        format!("{name}@{}", crate::peers::wire::sanitize_name(&self.host))
    }
}

pub fn is_peer_tell(message: &AgentBusMessage) -> bool {
    message.control_type.as_deref() == Some(CONTROL_TYPE)
}

pub fn is_peer_tell_request(payload: &AgentBusSendRequest) -> bool {
    payload.control_type.as_deref() == Some(CONTROL_TYPE)
}

fn facts(value: Option<&serde_json::Value>) -> anyhow::Result<PeerTellFacts> {
    let facts: PeerTellFacts =
        serde_json::from_value(value.cloned().context("peer tell facts absent")?)
            .context("invalid peer tell facts")?;
    ensure!(
        crate::peers::valid_id(&facts.peer_id)
            && !facts.host.is_empty()
            && !facts.sender_id.is_empty()
            && facts.sender_id.len() <= 128
            && !facts.tell_id.is_empty()
            && facts.tell_id.len() <= 128,
        "invalid peer tell facts"
    );
    Ok(facts)
}

/// Bus send gate: only a service caller (never an MCP-fenced agent, which
/// cannot set control records at all) can enqueue this shape.
pub fn validate_request(payload: &AgentBusSendRequest, sender: &str) -> anyhow::Result<()> {
    let facts = facts(payload.control_payload.as_ref())?;
    ensure!(
        payload.kind == AgentBusEnvelopeKind::Message
            && payload
                .sender_kind
                .as_ref()
                .is_none_or(|kind| kind.is_agent())
            && payload.from_agent_id.is_none()
            && payload.from_session_id.is_none()
            && payload.display_source.is_none()
            && payload.submit_mode.is_none()
            && payload.external_action_id.is_none()
            && payload.resolved_delivery_mode() == AgentDeliveryMode::Passive
            && payload
                .external_message_id
                .as_deref()
                .is_some_and(|id| id.starts_with(MESSAGE_ID_PREFIX))
            && sender == facts.sender_label(),
        "invalid peer tell envelope"
    );
    Ok(())
}

/// Model text of a queued peer tell (Codex ExternalInput and Claude inbox).
pub fn model_text(message: &AgentBusMessage) -> anyhow::Result<(PeerTellFacts, String)> {
    ensure!(
        message.kind == AgentBusEnvelopeKind::Message
            && message.sender_kind.is_agent()
            && message.from_cutex_session_id.is_none()
            && message.delivery_mode == AgentDeliveryMode::Passive
            && message
                .external_message_id
                .as_deref()
                .is_some_and(|id| id.starts_with(MESSAGE_ID_PREFIX)),
        "invalid peer tell provenance"
    );
    let facts = facts(message.control_payload.as_ref())?;
    let label = facts.sender_label();
    ensure!(message.from == label, "peer tell sender changed");
    let host = crate::peers::wire::sanitize_name(&facts.host);
    let id = crate::peers::wire::sanitize_name(&facts.sender_id);
    Ok((
        facts,
        format!(
            "Message Type: MESSAGE\nSender: {label} (an agent on another host, {host}; its id there: {id})\nOrigin: another host. Paths and ids in this note refer to that host, not this machine.\nReply: agent_tell to=\"{label}\" (works only if that host lets you reach it)\nPayload:\n{}",
            message.content
        ),
    ))
}

pub const VIEW_SCHEMA: &str = "cutex.peer-message.v1";

/// Display view of a peer tell (Claude native inbox; clients render it like
/// an agent note from `senderName`).
pub fn view(
    message: &AgentBusMessage,
    facts: &PeerTellFacts,
) -> anyhow::Result<crate::app_server::external_input::view::StructuredView> {
    use crate::app_server::external_input::view::StructuredView;
    let mut data = serde_json::json!({
        "senderName": facts.sender_label(),
        "senderId": crate::peers::wire::sanitize_name(&facts.sender_id),
        "host": crate::peers::wire::sanitize_name(&facts.host),
        "peerId": facts.peer_id,
        "deliveryMode": message.delivery_mode.event_label(),
        "deliveryKind": message.delivery_mode.kind(),
        "body": "",
    });
    if message.created_at_epoch_secs > 0 {
        data["occurredAtEpochSeconds"] = serde_json::json!(message.created_at_epoch_secs);
    }
    let mut view = StructuredView {
        schema: VIEW_SCHEMA.into(),
        data,
    };
    let base = view.canonical_json()?.len() + r#","bodyTruncated":true"#.len();
    let (body, truncated) = crate::agent_bus::projection::fit_json_string(
        &message.content,
        16384usize.saturating_sub(base),
    );
    view.data["body"] = serde_json::Value::String(body);
    if truncated {
        view.data["bodyTruncated"] = serde_json::json!(true);
    }
    view.canonical_json()?;
    Ok(view)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn facts_json() -> serde_json::Value {
        json!({"peerId":"ci-0123456789abcdef0123456789abcdef","host":"alpha","senderId":"cutex.remote","senderName":"worker","tellId":"ptl-1"})
    }

    pub(crate) fn message() -> AgentBusMessage {
        serde_json::from_value(json!({
            "id":"m1","kind":"message","from":"worker@alpha","to":"helper",
            "toCutexSessionId":"cutex.local","content":"see /tmp/x","deliveryMode":"passive",
            "triggerTurn":false,"createdAtEpochSecs":1,"senderKind":"agent",
            "controlType":CONTROL_TYPE,"controlPayload":facts_json(),
            "externalMessageId":"ptell-abc"
        }))
        .unwrap()
    }

    #[test]
    fn text_names_the_remote_sender_and_host() {
        let (facts, text) = model_text(&message()).unwrap();
        assert_eq!(facts.sender_label(), "worker@alpha");
        assert!(text.starts_with("Message Type: MESSAGE\nSender: worker@alpha (an agent on another host, alpha; its id there: cutex.remote)\n"));
        assert!(text.ends_with("Payload:\nsee /tmp/x"));
        let view = view(&message(), &facts).unwrap();
        assert_eq!(view.data["senderName"], "worker@alpha");
        assert_eq!(view.data["deliveryKind"], "note");
        assert_eq!(view.data["body"], "see /tmp/x");
        let mut long = message();
        long.content = "\"x\n".repeat(9000);
        assert_eq!(
            super::view(&long, &facts).unwrap().data["bodyTruncated"],
            true
        );
    }

    #[test]
    fn provenance_is_checked() {
        let mut forged = message();
        forged.from_cutex_session_id = Some("cutex.local-agent".into());
        assert!(model_text(&forged).is_err());
        let mut renamed = message();
        renamed.from = "Cutex Alarm".into();
        assert!(model_text(&renamed).is_err());
        let mut wake = message();
        wake.delivery_mode = AgentDeliveryMode::AfterTurn;
        assert!(model_text(&wake).is_err());
        let mut no_facts = message();
        no_facts.control_payload = None;
        assert!(model_text(&no_facts).is_err());
    }
}
