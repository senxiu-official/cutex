//! r217h1 wake wait log (owner: decide after a few days whether the "never
//! early" option is worth keeping). One structured stderr line per wake read
//! by a Codex recipient (`cutex-wake-read {json}`, in the log of the process
//! that runs the delivery bridge: `journalctl --user -u cutex-management-api`),
//! and for a wake read early (`early_next_step`) one more line when that turn
//! ends (`cutex-wake-counterfactual {json}`: `would_have_waited_ms` = turn end
//! - queued, the wait the r217 after-turn delivery would have had). No message
//! content. In-memory only: a turn that ends before the bridge saw the receipt,
//! or in another process, gets no counterfactual line.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::OnceLock;

use serde_json::json;

struct EarlyRead {
    message_id: String,
    /// r217h2: the delegation of a result read early (`None`: a wake).
    ask_id: Option<String>,
    producer: &'static str,
    wait_secs: Option<f64>,
    queued_at_ms: i64,
    read_at_ms: i64,
}

/// `(recipient, turn id)` → wakes read early in that turn.
fn early_reads() -> &'static Mutex<HashMap<(String, String), Vec<EarlyRead>>> {
    static EARLY: OnceLock<Mutex<HashMap<(String, String), Vec<EarlyRead>>>> = OnceLock::new();
    EARLY.get_or_init(Default::default)
}

const KEEP_MS: i64 = 48 * 3600 * 1000;

fn rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}

/// The producer kind of a wake (agent / delegation / job / alarm / owner;
/// r219 removed the Task Service: its messages are acknowledged unread and
/// never reach this log).
pub fn producer_kind(message: &crate::agent_bus::model::AgentBusMessage) -> &'static str {
    use crate::agent_bus::model::AgentMessageKind as Kind;
    if message.from == crate::agent_bus::alarms::ALARM_SYSTEM_SENDER || message.sender_kind.is_alarm_system() {
        return "alarm";
    }
    if message
        .control_type
        .as_deref()
        .is_some_and(|control| control.contains("ask") || control.contains("delegation"))
    {
        return "delegation";
    }
    match message.sender_kind {
        Kind::JobServiceSystem => "job",
        Kind::Owner | Kind::User => "owner",
        _ => "agent",
    }
}

/// Log one wake read (`path`: `idle_turn` | `turn_end` | `early_next_step`).
#[allow(clippy::too_many_arguments)]
pub fn record_read(
    message_id: &str,
    recipient: &str,
    producer: &'static str,
    wait: Option<std::time::Duration>,
    queued_at_epoch_secs: u64,
    path: &str,
    turn_id: &str,
) {
    let read_at_ms = chrono::Utc::now().timestamp_millis();
    let queued_at_ms = (queued_at_epoch_secs as i64).saturating_mul(1000);
    let wait_secs = wait.map(|wait| wait.as_secs_f64());
    eprintln!(
        "cutex-wake-read {}",
        json!({
            "messageId": message_id,
            "recipient": recipient,
            "producer": producer,
            "wakeBusyWaitSecs": wait_secs,
            "queuedAt": rfc3339(queued_at_ms),
            "readAt": rfc3339(read_at_ms),
            "waitedMs": (read_at_ms - queued_at_ms).max(0),
            "path": path,
            "turnId": turn_id,
        })
    );
    if path != "early_next_step" || turn_id.is_empty() {
        return;
    }
    if let Ok(mut early) = early_reads().lock() {
        early.retain(|_, reads| reads.iter().any(|read| read_at_ms - read.read_at_ms < KEEP_MS));
        early
            .entry((recipient.to_string(), turn_id.to_string()))
            .or_default()
            .push(EarlyRead {
                message_id: message_id.to_string(),
                ask_id: None,
                producer,
                wait_secs,
                queued_at_ms,
                read_at_ms,
            });
    }
}

/// r217h2: log one delegation result (answer / question / state item) that
/// reached its requester, as `cutex-wake-read` with producer
/// `delegation_result` (`path`: `idle_exclusive` = its own exclusive turn at
/// once, `turn_end` = its own exclusive turn after the requester's busy turn
/// ended, `early_next_step` = read at the busy turn's next step, native
/// `soon`). `messageId` is the results event's native message id; `queuedAt`
/// is when the result became pending. An early read also gets the
/// counterfactual line at that turn's end, like a wake.
#[allow(clippy::too_many_arguments)]
pub fn record_result_read(
    ask_id: &str,
    kind: &str,
    message_id: &str,
    requester: &str,
    wait: Option<std::time::Duration>,
    queued_at_ms: i64,
    read_at_ms: i64,
    path: &str,
    turn_id: &str,
) {
    let wait_secs = wait.map(|wait| wait.as_secs_f64());
    eprintln!(
        "cutex-wake-read {}",
        json!({
            "messageId": message_id,
            "askId": ask_id,
            "kind": kind,
            "recipient": requester,
            "producer": "delegation_result",
            "wakeBusyWaitSecs": wait_secs,
            "queuedAt": rfc3339(queued_at_ms),
            "readAt": rfc3339(read_at_ms),
            "waitedMs": (read_at_ms - queued_at_ms).max(0),
            "path": path,
            "turnId": turn_id,
        })
    );
    if path != "early_next_step" || turn_id.is_empty() {
        return;
    }
    if let Ok(mut early) = early_reads().lock() {
        early.retain(|_, reads| reads.iter().any(|read| read_at_ms - read.read_at_ms < KEEP_MS));
        early
            .entry((requester.to_string(), turn_id.to_string()))
            .or_default()
            .push(EarlyRead {
                message_id: message_id.to_string(),
                ask_id: Some(ask_id.to_string()),
                producer: "delegation_result",
                wait_secs,
                queued_at_ms,
                read_at_ms,
            });
    }
}

/// A runtime notification of `recipient`: on `turn/completed` of a turn that
/// read wakes early, log their counterfactual after-turn wait.
pub fn observe(recipient: &str, method: &str, params: &serde_json::Value) {
    if method != "turn/completed" {
        return;
    }
    let Some(turn_id) = params.pointer("/turn/id").and_then(serde_json::Value::as_str) else {
        return;
    };
    let Some(reads) = early_reads()
        .lock()
        .ok()
        .and_then(|mut early| early.remove(&(recipient.to_string(), turn_id.to_string())))
    else {
        return;
    };
    let ended_at_ms = chrono::Utc::now().timestamp_millis();
    for read in reads {
        let mut line = json!({
            "messageId": read.message_id,
            "recipient": recipient,
            "producer": read.producer,
            "wakeBusyWaitSecs": read.wait_secs,
            "queuedAt": rfc3339(read.queued_at_ms),
            "readAt": rfc3339(read.read_at_ms),
            "waitedMs": (read.read_at_ms - read.queued_at_ms).max(0),
            "turnId": turn_id,
            "turnEndAt": rfc3339(ended_at_ms),
            "wouldHaveWaitedMs": (ended_at_ms - read.queued_at_ms).max(0),
        });
        if let Some(ask_id) = read.ask_id {
            line["askId"] = json!(ask_id);
        }
        eprintln!("cutex-wake-counterfactual {line}");
    }
}
