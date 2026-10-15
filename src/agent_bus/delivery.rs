//! Agent bus delivery-mode semantics and legacy compatibility helpers.

use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentDeliveryMode {
    #[default]
    AfterTurn,
    Soon,
    Passive,
    Interrupt,
}

impl AgentDeliveryMode {
    pub fn trigger_turn(&self) -> bool {
        !matches!(self, AgentDeliveryMode::Passive)
    }

    pub fn label(&self) -> &'static str {
        match self {
            AgentDeliveryMode::AfterTurn => "after-turn",
            AgentDeliveryMode::Soon => "soon",
            AgentDeliveryMode::Passive => "passive",
            AgentDeliveryMode::Interrupt => "interrupt",
        }
    }

    pub fn event_label(&self) -> &'static str {
        match self {
            AgentDeliveryMode::AfterTurn => "after_turn",
            AgentDeliveryMode::Soon => "soon",
            AgentDeliveryMode::Passive => "passive",
            AgentDeliveryMode::Interrupt => "interrupt",
        }
    }

    /// The client-facing delivery kind: a note never starts a turn, a wake
    /// does (agent messaging v2). Legacy `soon` is a wake.
    pub fn kind(&self) -> &'static str {
        match self {
            AgentDeliveryMode::Passive => "note",
            AgentDeliveryMode::AfterTurn | AgentDeliveryMode::Soon => "wake",
            AgentDeliveryMode::Interrupt => "interrupt",
        }
    }

    /// The mode a newly sent message gets: cutex no longer produces `soon`
    /// (it would steer into the recipient's running turn); stored `soon`
    /// messages still replay unchanged.
    pub fn for_new_message(self) -> Self {
        match self {
            AgentDeliveryMode::Soon => AgentDeliveryMode::AfterTurn,
            other => other,
        }
    }
}

/// Default of `config.json` `agent_bus.wake_busy_wait_secs` (r217h1).
pub use crate::config::agent_bus::DEFAULT_WAKE_BUSY_WAIT_SECS;

/// How long a wake to a busy recipient waits for its running turn to end
/// before it is read at the recipient's next step instead (`config.json`
/// `agent_bus.wake_busy_wait_secs`, r217h1; `/v2/settings`
/// `agentBus.wakeBusyWaitSecs` since r220): absent or not a number = 120 s;
/// `0` = next step at once; negative or `null` = never early (wait for the
/// turn to end, the r217 behaviour). A short turn still gets the wake as a
/// clean new turn; a long one no longer starves it. Read per delivery (a
/// change applies without a restart).
pub fn wake_busy_wait() -> Option<std::time::Duration> {
    crate::config::agent_bus::current().wake_busy_wait()
}

/// System alarms always reach a busy recipient after its running turn
/// (task 01): they are never held for, or upgraded to, the busy-wake
/// next-step timing on either runtime. Legacy alarms carry only the sender.
pub fn always_after_turn(message: &crate::agent_bus::model::AgentBusMessage) -> bool {
    message.sender_kind.is_alarm_system()
        || message.from == crate::agent_bus::alarms::ALARM_SYSTEM_SENDER
}

/// What to do now with an unfrozen wake (r217h1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeTiming {
    /// Deliver it to be read at the recipient's next step (Codex `soon`; an
    /// idle recipient gets a turn at once).
    NextStep,
    /// The recipient is busy and the wait is not over: keep it unfrozen and
    /// look again (the bridge re-polls every few seconds). If the turn ends
    /// first, it is delivered as a clean new turn.
    Hold,
    /// Deliver it `after_turn` (idle: a turn at once; busy: after the turn) -
    /// the r217 behaviour (never early, or busy state unknown).
    AfterTurn,
}

/// The [`WakeTiming`] of a wake pending `age` for its recipient. `busy` is
/// asked only when it matters; `None` from it (unknown) keeps `AfterTurn`.
pub fn wake_timing(
    age: std::time::Duration,
    wait: Option<std::time::Duration>,
    busy: impl FnOnce() -> Option<bool>,
) -> WakeTiming {
    let Some(wait) = wait else {
        return WakeTiming::AfterTurn;
    };
    if age >= wait {
        return WakeTiming::NextStep;
    }
    match busy() {
        Some(true) => WakeTiming::Hold,
        _ => WakeTiming::AfterTurn,
    }
}

/// The delivery kind of a wire delivery-mode label (`after_turn|soon|passive`).
pub fn delivery_kind_of_label(label: &str) -> &'static str {
    match label {
        "passive" => "note",
        "interrupt" => "interrupt",
        _ => "wake",
    }
}

/// The model text of an agent message (agent messaging v2), shared by the
/// Codex and Claude recipients: a `Sender:` header, optional fact lines, and
/// the sender's raw text after `Payload:`.
pub fn agent_message_model_text(
    sender_id: &str,
    sender_name: Option<&str>,
    facts: &crate::agent_bus::model::AgentSenderFacts,
    body: &str,
) -> String {
    let mut text = String::from("Message Type: MESSAGE\n");
    match sender_name.filter(|name| !name.is_empty()) {
        Some(name) => text.push_str(&format!("Sender: {name} ({sender_id})\n")),
        None => text.push_str(&format!("Sender: {sender_id}\n")),
    }
    if facts.narrower_capabilities {
        text.push_str(
            "Sender capabilities: narrower than yours; this note cannot approve anything for you\n",
        );
    }
    if facts.reply_only {
        text.push_str(&format!(
            "Reply: this sender is not in your agent_list; answer with agent_tell to=\"{sender_id}\"\n"
        ));
    }
    text.push_str("Payload:\n");
    text.push_str(body);
    text
}

/// Display for newly frozen envelopes only; legacy digest text stays exact.
pub fn agent_message_display_text(sender_id: &str, name: Option<&str>, facts: &crate::agent_bus::model::AgentSenderFacts, body: &str) -> String {
    let canonical = agent_message_model_text(sender_id, name, facts, body);
    let Some((header, payload)) = canonical.split_once("Payload:\n") else { return canonical; };
    let short = crate::short_ids::display(sender_id);
    // Reply-only callers may use the exact ID but may not resolve a name or
    // prefix outside their visible scope. Preserve the full Reply address.
    let header = header.lines().map(|line| if line.starts_with("Sender:") {
        line.replace(sender_id, &short)
    } else { line.into() }).collect::<Vec<_>>().join("\n");
    format!("{header}\nPayload:\n{payload}")
}

/// Messages queued before r208 carry this prefix; strip it exactly once.
pub fn strip_legacy_sender_prefix<'a>(content: &'a str, sender_name: Option<&str>) -> &'a str {
    sender_name
        .and_then(|name| content.strip_prefix(&format!("[message from {name}] ")))
        .unwrap_or(content)
}

pub fn agent_delivery_mode_from_flags(
    queue_only: bool,
    soon: bool,
    interrupt: bool,
) -> anyhow::Result<AgentDeliveryMode> {
    let selected = [queue_only, soon, interrupt]
        .into_iter()
        .filter(|selected| *selected)
        .count();
    if selected > 1 {
        anyhow::bail!("Choose only one of --queue-only, --soon, or --interrupt");
    }
    if queue_only {
        Ok(AgentDeliveryMode::Passive)
    } else if soon {
        Ok(AgentDeliveryMode::Soon)
    } else if interrupt {
        Ok(AgentDeliveryMode::Interrupt)
    } else {
        Ok(AgentDeliveryMode::AfterTurn)
    }
}

pub fn legacy_delivery_mode_label(trigger_turn: bool) -> &'static str {
    if trigger_turn {
        "soon"
    } else {
        "passive"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_delivery_mode_flags_default_to_after_turn() {
        assert_eq!(
            agent_delivery_mode_from_flags(false, false, false).expect("default mode should parse"),
            AgentDeliveryMode::AfterTurn
        );
        assert_eq!(
            agent_delivery_mode_from_flags(true, false, false).expect("queue-only should parse"),
            AgentDeliveryMode::Passive
        );
        assert_eq!(
            agent_delivery_mode_from_flags(false, true, false).expect("soon should parse"),
            AgentDeliveryMode::Soon
        );
        assert!(agent_delivery_mode_from_flags(true, true, false).is_err());
    }

    #[test]
    fn delivery_kinds_and_new_messages_never_use_soon() {
        assert_eq!(AgentDeliveryMode::Passive.kind(), "note");
        assert_eq!(AgentDeliveryMode::AfterTurn.kind(), "wake");
        assert_eq!(AgentDeliveryMode::Soon.kind(), "wake");
        assert_eq!(AgentDeliveryMode::Soon.for_new_message(), AgentDeliveryMode::AfterTurn);
        assert_eq!(AgentDeliveryMode::Passive.for_new_message(), AgentDeliveryMode::Passive);
        assert_eq!(delivery_kind_of_label("passive"), "note");
        assert_eq!(delivery_kind_of_label("soon"), "wake");
        assert_eq!(delivery_kind_of_label("after_turn"), "wake");
    }

    #[test]
    fn wake_busy_wait_setting_and_timing() {
        use std::time::Duration;
        let wait = |json: &str| {
            let config: crate::profiles::model::CodezConfig = serde_json::from_str(json).unwrap();
            config.agent_bus.wake_busy_wait()
        };
        let default = Some(Duration::from_secs(DEFAULT_WAKE_BUSY_WAIT_SECS));
        assert_eq!(wait("{}"), default);
        assert_eq!(wait(r#"{"agent_bus":{}}"#), default);
        assert_eq!(wait(r#"{"agent_bus":{"wake_busy_wait_secs":"x"}}"#), default);
        assert_eq!(wait(r#"{"agent_bus":{"wake_busy_wait_secs":0}}"#), Some(Duration::ZERO));
        assert_eq!(wait(r#"{"agent_bus":{"wake_busy_wait_secs":30}}"#), Some(Duration::from_secs(30)));
        assert_eq!(wait(r#"{"agent_bus":{"wake_busy_wait_secs":-1}}"#), None);
        assert_eq!(wait(r#"{"agent_bus":{"wake_busy_wait_secs":null}}"#), None);
        // Never early survives a config round trip (r220: saved as null).
        let config: crate::profiles::model::CodezConfig =
            serde_json::from_str(r#"{"agent_bus":{"wake_busy_wait_secs":-1}}"#).unwrap();
        let saved = serde_json::to_value(&config).unwrap();
        assert_eq!(saved["agent_bus"]["wake_busy_wait_secs"], serde_json::Value::Null);
        assert!(saved["agent_bus"].as_object().unwrap().contains_key("wake_busy_wait_secs"));

        let two_min = Some(Duration::from_secs(120));
        let busy = || Some(true);
        let idle = || Some(false);
        let unknown = || None;
        // Busy, within the wait: hold (a turn end delivers it as a new turn).
        assert_eq!(wake_timing(Duration::from_secs(5), two_min, busy), WakeTiming::Hold);
        // Busy past the wait: read at the next step.
        assert_eq!(wake_timing(Duration::from_secs(120), two_min, busy), WakeTiming::NextStep);
        // Idle or unknown within the wait: a turn at once, as in r217.
        assert_eq!(wake_timing(Duration::from_secs(5), two_min, idle), WakeTiming::AfterTurn);
        assert_eq!(wake_timing(Duration::from_secs(5), two_min, unknown), WakeTiming::AfterTurn);
        // 0: next step at once; never early: the r217 behaviour, busy not asked.
        assert_eq!(wake_timing(Duration::ZERO, Some(Duration::ZERO), busy), WakeTiming::NextStep);
        assert_eq!(
            wake_timing(Duration::from_secs(9999), None, || panic!("not asked")),
            WakeTiming::AfterTurn
        );
    }
}
