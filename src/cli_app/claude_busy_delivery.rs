//! Scheduling uses the shared wake policy; observations below affect logs only.
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use serde_json::{json, Value};
use cutex::agent_bus::delivery::{wake_timing, WakeTiming};

pub(super) fn timing(age: Duration, wait: Option<Duration>, busy: Option<bool>, supported: bool) -> WakeTiming {
    if supported { wake_timing(age, wait, || busy) } else { WakeTiming::AfterTurn }
}

/// Whether a bus message may be held for / read at a busy recipient's next
/// step. System alarms are always after-turn (their Claude ingress envelope
/// requires it), as on Codex.
pub(super) fn candidate(message: &cutex::agent_bus::model::AgentBusMessage, offers_soon: bool) -> bool {
    offers_soon
        && message.delivery_mode == cutex::agent_bus::delivery::AgentDeliveryMode::AfterTurn
        && !cutex::agent_bus::delivery::always_after_turn(message)
}

fn observations() -> &'static Mutex<HashMap<String, Value>> {
    static OBSERVATIONS: OnceLock<Mutex<HashMap<String, Value>>> = OnceLock::new();
    OBSERVATIONS.get_or_init(Default::default)
}

pub(super) fn note(message: &cutex::agent_bus::model::AgentBusMessage, wait: Option<Duration>, busy: Option<&str>) {
    if !message.delivery_mode.trigger_turn() { return; }
    if let Ok(mut rows) = observations().lock() {
        if rows.len() >= 4096 { rows.clear(); }
        let row = rows.entry(message.id.clone()).or_insert_with(|| json!({
            "queuedAt": message.created_at_epoch_secs, "waitSecs": wait.map(|duration| duration.as_secs_f64()),
            "producer": cutex::agent_bus::wake_log::producer_kind(message), "busyTurn": busy,
        }));
        if busy.is_some() { row["busyTurn"] = json!(busy); }
    }
}

pub(super) fn decorate(message: &str, receipt: &mut Value) {
    if let Some(row) = observations().lock().ok().and_then(|rows| rows.get(message).cloned()) {
        receipt["wakeRead"] = row;
    }
}

fn path(busy: Option<&str>, actual: &str) -> &'static str {
    match busy { Some(turn) if turn == actual => "early_next_step", Some(_) => "turn_end", None => "idle_turn" }
}

pub(super) fn read(owner: &str, message: &str, turn: &str, local: Option<&Value>) {
    let info = observations().lock().ok().and_then(|mut rows| rows.remove(message))
        .or_else(|| local.and_then(|receipt| receipt.get("wakeRead")).cloned());
    let Some(info) = info else { return; };
    let producer = match info["producer"].as_str() {
        Some("alarm") => "alarm", Some("delegation") => "delegation", Some("job") => "job",
        Some("owner") => "owner", _ => "agent",
    };
    cutex::agent_bus::wake_log::record_read(message, owner, producer,
        info["waitSecs"].as_f64().and_then(|seconds| Duration::try_from_secs_f64(seconds).ok()),
        info["queuedAt"].as_u64().unwrap_or(0), path(info["busyTurn"].as_str(), turn), turn);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wait_boundary_and_old_runtime_fallback() {
        let wait = Some(Duration::from_secs(120));
        assert_eq!(timing(Duration::from_secs(119), wait, Some(true), true), WakeTiming::Hold);
        assert_eq!(timing(Duration::from_secs(120), wait, Some(true), true), WakeTiming::NextStep);
        assert_eq!(timing(Duration::ZERO, Some(Duration::ZERO), Some(true), true), WakeTiming::NextStep);
        assert_eq!(timing(Duration::from_secs(500), None, Some(true), true), WakeTiming::AfterTurn);
        assert_eq!(timing(Duration::from_secs(500), wait, Some(true), false), WakeTiming::AfterTurn);
        assert_eq!(timing(Duration::ZERO, wait, Some(false), true), WakeTiming::AfterTurn);
        assert_eq!(timing(Duration::ZERO, wait, None, true), WakeTiming::AfterTurn);
        assert_eq!(path(Some("busy"), "busy"), "early_next_step");
        assert_eq!(path(Some("busy"), "following"), "turn_end");
        assert_eq!(path(None, "following"), "idle_turn");
    }

    /// 01 + 07: an alarm to a busy Claude agent past the busy wait (also with
    /// `wake_busy_wait_secs = 0`) stays after-turn; an agent wake does not.
    #[test]
    fn alarms_stay_after_turn_on_busy_claude_agents() {
        use cutex::agent_bus::model::{AgentBusMessage, AgentMessageKind};
        let wake: AgentBusMessage = serde_json::from_value(json!({"id":"m","from":"Eva","to":"runtime",
            "fromCutexSessionId":"cutex.11111111-1111-4111-8111-111111111111",
            "toCutexSessionId":"cutex.22222222-2222-4222-8222-222222222222",
            "content":"hi","deliveryMode":"after_turn","triggerTurn":true,"createdAtEpochSecs":1})).unwrap();
        let mut alarm = wake.clone();
        alarm.sender_kind = AgentMessageKind::AlarmSystem;
        alarm.from = cutex::agent_bus::alarms::ALARM_SYSTEM_SENDER.into();
        let mut legacy_alarm = wake.clone();
        legacy_alarm.from = cutex::agent_bus::alarms::ALARM_SYSTEM_SENDER.into();
        for wait in [Some(Duration::from_secs(120)), Some(Duration::ZERO)] {
            let at = |message: &AgentBusMessage| timing(Duration::from_secs(600), wait, Some(true), candidate(message, true));
            assert_eq!(at(&wake), WakeTiming::NextStep);
            assert_eq!(at(&alarm), WakeTiming::AfterTurn);
            assert_eq!(at(&legacy_alarm), WakeTiming::AfterTurn);
            // Never held either.
            let young = |message: &AgentBusMessage| timing(Duration::ZERO, wait, Some(true), candidate(message, true));
            assert_ne!(young(&alarm), WakeTiming::Hold);
        }
        assert!(!candidate(&wake, false));
    }
}
