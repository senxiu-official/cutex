//! Cross-host `agent_tell` notes (docs/PEERS.md "Cross-host tells").
//!
//! The receiving Management daemon enqueues a peer tell on the local Agent Bus
//! as an ordinary passive agent message with a reserved control record. The
//! remote sender never becomes a local session or Agent Bus identity: the
//! durable message has no `fromCutexSessionId`, and recipients see a service
//! source (`cutex-peers`) whose text names the remote agent as
//! `<name>@<peer label>`. Older binaries skip this control type.
//!
//! A cross-host wake (`inbound.tellWake`) uses its own control type
//! [`CONTROL_TYPE_V2`] with `after_turn` delivery, so it takes the ordinary
//! busy-wake path; `cutex.peer.tell.v1` stays passive-only (older bridges
//! fail a non-passive v1 record, but skip or keep an unknown type).
use crate::agent_bus::delivery::AgentDeliveryMode;
use crate::agent_bus::model::{AgentBusEnvelopeKind, AgentBusMessage, AgentBusSendRequest};
use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};

pub const CONTROL_TYPE: &str = "cutex.peer.tell.v1";
/// A cross-host wake: the same facts, `after_turn` delivery.
pub const CONTROL_TYPE_V2: &str = "cutex.peer.tell.v2";
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
    /// Set on a note that was sent as a wake but delivered as a note
    /// (`rate_limited`: this host's hourly wake limit for the peer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_downgraded: Option<String>,
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

fn is_control_type(control_type: Option<&str>) -> bool {
    matches!(control_type, Some(CONTROL_TYPE | CONTROL_TYPE_V2))
}

/// The delivery a peer tell control type requires: v1 a passive note, v2
/// (a cross-host wake) `after_turn`.
fn required_mode(control_type: Option<&str>) -> AgentDeliveryMode {
    if control_type == Some(CONTROL_TYPE_V2) {
        AgentDeliveryMode::AfterTurn
    } else {
        AgentDeliveryMode::Passive
    }
}

/// A cross-host note (v1) or wake (v2).
pub fn is_peer_tell(message: &AgentBusMessage) -> bool {
    is_control_type(message.control_type.as_deref())
}

pub fn is_peer_tell_request(payload: &AgentBusSendRequest) -> bool {
    is_control_type(payload.control_type.as_deref())
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
            && payload.resolved_delivery_mode() == required_mode(payload.control_type.as_deref())
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
            && message.delivery_mode == required_mode(message.control_type.as_deref())
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
    let delivery = if message.delivery_mode == AgentDeliveryMode::AfterTurn {
        format!("Wake: {label}, an agent on another host, started this turn with this message (this host lets {host} wake its agents).\n")
    } else if facts.wake_downgraded.is_some() {
        format!("Delivery: {label} sent this as a wake, but this host's hourly wake limit for {host} was reached, so it arrived as a note.\n")
    } else {
        String::new()
    };
    Ok((
        facts,
        format!(
            "Message Type: MESSAGE\nSender: {label} (an agent on another host, {host}; its id there: {id})\nOrigin: another host. Paths and ids in this note refer to that host, not this machine.\n{delivery}Reply: agent_tell to=\"{label}\" (works only if that host lets you reach it)\nPayload:\n{}",
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
    if facts.wake_downgraded.is_some() {
        data["wakeDowngraded"] = serde_json::json!(true);
    }
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

    fn facts() -> PeerTellFacts {
        serde_json::from_value(facts_json()).unwrap()
    }

    /// The bus send gate: v1 only as a passive note, v2 (cross-host wake)
    /// only after_turn.
    #[test]
    fn send_gate_pairs_control_type_and_delivery() {
        let request = |wake| {
            crate::agent_bus::client::peer_tell_send_request(
                "helper",
                "x",
                &facts(),
                "ptell-1",
                wake,
            )
            .unwrap()
        };
        let note = request(false);
        assert_eq!(note.control_type.as_deref(), Some(CONTROL_TYPE));
        assert_eq!(note.resolved_delivery_mode(), AgentDeliveryMode::Passive);
        validate_request(&note, "worker@alpha").unwrap();
        let wake = request(true);
        assert_eq!(wake.control_type.as_deref(), Some(CONTROL_TYPE_V2));
        assert_eq!(wake.resolved_delivery_mode(), AgentDeliveryMode::AfterTurn);
        assert!(is_peer_tell_request(&wake));
        validate_request(&wake, "worker@alpha").unwrap();
        assert!(validate_request(&wake, "other@alpha").is_err());
        let mut v1_wake = note.clone();
        v1_wake.delivery_mode = Some(AgentDeliveryMode::AfterTurn);
        v1_wake.trigger_turn = Some(true);
        assert!(validate_request(&v1_wake, "worker@alpha").is_err());
        let mut v2_note = wake.clone();
        v2_note.delivery_mode = Some(AgentDeliveryMode::Passive);
        v2_note.trigger_turn = Some(false);
        assert!(validate_request(&v2_note, "worker@alpha").is_err());
    }

    #[test]
    fn wake_text_says_another_host_started_the_turn() {
        let mut wake = message();
        wake.control_type = Some(CONTROL_TYPE_V2.into());
        wake.delivery_mode = AgentDeliveryMode::AfterTurn;
        assert!(is_peer_tell(&wake));
        let (facts, text) = model_text(&wake).unwrap();
        assert!(text.contains("\nWake: worker@alpha, an agent on another host, started this turn with this message"), "{text}");
        assert!(text.ends_with("Payload:\nsee /tmp/x"));
        let view = view(&wake, &facts).unwrap();
        assert_eq!(view.data["deliveryKind"], "wake");
        assert!(view.data.get("wakeDowngraded").is_none());
        let mut passive_v2 = wake.clone();
        passive_v2.delivery_mode = AgentDeliveryMode::Passive;
        assert!(model_text(&passive_v2).is_err());
        // A plain note has neither line; a downgraded one says so.
        let (_, note) = model_text(&message()).unwrap();
        assert!(!note.contains("Wake:") && !note.contains("Delivery:"));
        let mut downgraded = message();
        let mut payload = facts_json();
        payload["wakeDowngraded"] = serde_json::json!("rate_limited");
        downgraded.control_payload = Some(payload);
        let (facts, text) = model_text(&downgraded).unwrap();
        assert!(text.contains("\nDelivery: worker@alpha sent this as a wake, but this host's hourly wake limit for alpha was reached, so it arrived as a note.\n"), "{text}");
        assert_eq!(
            super::view(&downgraded, &facts).unwrap().data["wakeDowngraded"],
            true
        );
    }
}
