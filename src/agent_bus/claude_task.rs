//! "Claude Task" notices: a Claude agent's background subagent or background
//! shell command ended (Claude adapter claude-r23, `input.backgroundSubagents`;
//! components/claude-runtime/docs/BUSY_DELIVERY.md "Background subagents").
//!
//! The adapter adds `notifyParent` to that task's `claude/task/updated`. The
//! Claude observer turns it into ONE Agent Bus message to the same agent: an
//! ordinary message with a reserved control record, a service sender
//! ("Claude Task") and no local sender identity, like a cross-host tell. Its
//! message id is the stable `ctask-<digest of session + noticeId>`, so a notice
//! the adapter repeats (a resume replay, two observers) is queued once. Delivery
//! is after_turn on the bus; Claude delivery reads it at a busy parent's next
//! tool step at once (no busy wait), and an idle parent gets a visible turn.
use crate::agent_bus::delivery::AgentDeliveryMode;
use crate::agent_bus::model::{AgentBusEnvelopeKind, AgentBusMessage, AgentBusSendRequest, AgentMessageKind};
use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const CONTROL_TYPE: &str = "cutex.claude.task.v1";
pub const SOURCE_ID: &str = "Claude Task";
pub const EVENT_TYPE: &str = "claude_task";
pub const MESSAGE_ID_PREFIX: &str = "ctask-";
pub const VIEW_SCHEMA: &str = "cutex.claude-task.v1";
const SUMMARY_CHARS: usize = 4000;
const STATUSES: &[&str] = &["completed", "failed", "stopped", "lost"];

/// One coalesced background command (shell notices).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskEntry {
    pub task_id: String,
    pub status: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// The adapter's `notifyParent`, validated and bounded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeTaskNotice {
    pub notice_id: String,
    pub task_id: String,
    /// `local_agent` (background subagent) or `local_bash` (background command).
    pub task_type: String,
    /// completed | failed | stopped | lost
    pub status: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default)]
    pub summary_truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tasks: Option<Vec<TaskEntry>>,
}

fn identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= 300 && !value.chars().any(char::is_control)
}

fn clip(value: &str, chars: usize) -> String {
    value.chars().take(chars).collect()
}

impl ClaudeTaskNotice {
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            identifier(&self.notice_id)
                && identifier(&self.task_id)
                && matches!(self.task_type.as_str(), "local_agent" | "local_bash")
                && STATUSES.contains(&self.status.as_str())
                && self.description.chars().count() <= 2048
                && self.summary.as_ref().is_none_or(|s| s.chars().count() <= SUMMARY_CHARS)
                && [&self.origin_turn_id, &self.tool_use_id, &self.transcript, &self.reason]
                    .iter()
                    .all(|value| value.as_ref().is_none_or(|v| v.chars().count() <= 600))
                && self.tasks.as_ref().is_none_or(|tasks| {
                    tasks.len() <= 20
                        && tasks.iter().all(|t| {
                            identifier(&t.task_id)
                                && STATUSES.contains(&t.status.as_str())
                                && t.description.chars().count() <= 500
                                && t.summary.as_ref().is_none_or(|s| s.chars().count() <= 500)
                        })
                }),
            "invalid Claude task notice"
        );
        Ok(())
    }

    /// `notifyParent` of a `claude/task/updated` notification, bounded and
    /// validated (None: no notice, or one this version cannot read).
    pub fn from_task_updated(params: &serde_json::Value) -> Option<Self> {
        let raw = params.get("task")?.get("notifyParent")?;
        let mut notice: Self = serde_json::from_value(raw.clone()).ok()?;
        notice.description = clip(&notice.description, 2048);
        if let Some(summary) = notice.summary.as_mut() {
            if summary.chars().count() > SUMMARY_CHARS {
                *summary = clip(summary, SUMMARY_CHARS);
                notice.summary_truncated = true;
            }
        }
        notice.validate().ok()?;
        Some(notice)
    }

    fn kind_label(&self) -> &'static str {
        if self.task_type == "local_agent" {
            "background subagent"
        } else if self.tasks.is_some() {
            "background commands"
        } else {
            "background command"
        }
    }
}

/// Stable bus message id of one notice to one agent.
pub fn message_id(cutex_session_id: &str, notice_id: &str) -> String {
    let digest = Sha256::digest(format!("{cutex_session_id}\0{notice_id}").as_bytes());
    format!("{MESSAGE_ID_PREFIX}{}", &format!("{digest:x}")[..32])
}

pub fn is_claude_task(message: &AgentBusMessage) -> bool {
    message.control_type.as_deref() == Some(CONTROL_TYPE)
}

pub fn is_claude_task_request(payload: &AgentBusSendRequest) -> bool {
    payload.control_type.as_deref() == Some(CONTROL_TYPE)
}

/// The bus send request for a notice to `target` (a durable cutex session id).
pub fn send_request(target: &str, notice: &ClaudeTaskNotice) -> anyhow::Result<AgentBusSendRequest> {
    notice.validate()?;
    Ok(AgentBusSendRequest {
        to: target.to_string(),
        all_groups: true,
        kind: AgentBusEnvelopeKind::Message,
        from: Some(SOURCE_ID.into()),
        from_agent_id: None,
        from_session_id: None,
        to_session_id: None,
        content: summary_line(notice),
        delivery_mode: Some(AgentDeliveryMode::AfterTurn),
        queue_only: None,
        trigger_turn: Some(true),
        sender_kind: Some(AgentMessageKind::Agent),
        display_source: None,
        submit_mode: None,
        control_type: Some(CONTROL_TYPE.into()),
        control_payload: Some(serde_json::to_value(notice)?),
        external_action_id: None,
        external_message_id: Some(message_id(target, &notice.notice_id)),
        sender_facts: None,
    })
}

/// Bus send gate: only a service caller (never an MCP-fenced agent, which
/// cannot set control records) can enqueue this shape.
pub fn validate_request(payload: &AgentBusSendRequest, sender: &str) -> anyhow::Result<()> {
    let notice: ClaudeTaskNotice = serde_json::from_value(
        payload.control_payload.clone().context("Claude task notice absent")?,
    )
    .context("invalid Claude task notice")?;
    notice.validate()?;
    ensure!(
        payload.kind == AgentBusEnvelopeKind::Message
            && payload.sender_kind.as_ref().is_none_or(AgentMessageKind::is_agent)
            && payload.from_agent_id.is_none()
            && payload.from_session_id.is_none()
            && payload.display_source.is_none()
            && payload.submit_mode.is_none()
            && payload.external_action_id.is_none()
            && payload.resolved_delivery_mode() == AgentDeliveryMode::AfterTurn
            && payload.external_message_id.as_deref() == Some(message_id(&payload.to, &notice.notice_id).as_str())
            && payload.content == summary_line(&notice)
            && sender == SOURCE_ID,
        "invalid Claude task notice envelope"
    );
    Ok(())
}

/// The message's stored content: one line (audit previews, message lists).
fn summary_line(notice: &ClaudeTaskNotice) -> String {
    let what = if notice.description.is_empty() { notice.task_id.as_str() } else { notice.description.as_str() };
    format!("Your {} {}: {}", notice.kind_label(), notice.status, clip(what, 200))
}

fn provenance(message: &AgentBusMessage) -> anyhow::Result<ClaudeTaskNotice> {
    ensure!(
        message.kind == AgentBusEnvelopeKind::Message
            && message.sender_kind.is_agent()
            && message.from_cutex_session_id.is_none()
            && message.from == SOURCE_ID
            && message.delivery_mode == AgentDeliveryMode::AfterTurn
            && is_claude_task(message),
        "invalid Claude task notice provenance"
    );
    let notice: ClaudeTaskNotice = serde_json::from_value(
        message.control_payload.clone().context("Claude task notice absent")?,
    )?;
    notice.validate()?;
    let owner = message.to_cutex_session_id.as_deref().context("Claude task notice target absent")?;
    ensure!(
        message.external_message_id.as_deref() == Some(message_id(owner, &notice.notice_id).as_str()),
        "Claude task notice identity changed"
    );
    Ok(notice)
}

/// Model text of a queued notice (the Claude inbox).
pub fn model_text(message: &AgentBusMessage) -> anyhow::Result<(ClaudeTaskNotice, String)> {
    let notice = provenance(message)?;
    let mut text = format!(
        "Message Type: MESSAGE\nSender: {SOURCE_ID} (cutex: one of your own background tasks ended)\n"
    );
    let label = notice.kind_label();
    let what = match notice.status.as_str() {
        "completed" => format!("Your {label} finished."),
        "failed" => format!("Your {label} failed."),
        "stopped" => format!("Your {label} was stopped before it finished (by the owner or a stop request)."),
        _ => format!(
            "Your {label} was lost: {} Its work may be partial; check its effects before you run it again.",
            notice.reason.as_deref().unwrap_or("the Claude runtime restarted while it ran.")
        ),
    };
    text.push_str(&what);
    text.push('\n');
    if !notice.description.is_empty() {
        text.push_str(&format!("Task: {}\n", notice.description));
    }
    text.push_str(&format!("Task id: {}\n", notice.task_id));
    if let Some(transcript) = &notice.transcript {
        text.push_str(&format!("Transcript: {transcript}\n"));
    }
    if let Some(tasks) = &notice.tasks {
        text.push_str("Commands:\n");
        for task in tasks {
            text.push_str(&format!("- {} ({}): {}\n", task.task_id, task.status, task.description));
        }
    }
    text.push_str("This message is your visible turn for it: act on the result now if needed.\n");
    match &notice.summary {
        Some(summary) if notice.task_type == "local_agent" => {
            text.push_str("Its final reply");
            if notice.summary_truncated {
                text.push_str(" (cut at 4000 characters)");
            }
            text.push_str(":\n");
            text.push_str(summary);
        }
        Some(summary) => {
            text.push_str("Summary:\n");
            text.push_str(summary);
        }
        None => text.push_str("No final reply was reported."),
    }
    Ok((notice, text))
}

/// Display view (clients without a renderer for the schema show the text).
pub fn view(
    message: &AgentBusMessage,
    notice: &ClaudeTaskNotice,
) -> anyhow::Result<crate::app_server::external_input::view::StructuredView> {
    use crate::app_server::external_input::view::StructuredView;
    let mut data = serde_json::json!({
        "senderName": SOURCE_ID,
        "senderId": SOURCE_ID,
        "deliveryMode": message.delivery_mode.event_label(),
        "deliveryKind": message.delivery_mode.kind(),
        "taskId": notice.task_id,
        "taskType": notice.task_type,
        "status": notice.status,
        "description": notice.description,
    });
    for (key, value) in [("transcript", &notice.transcript), ("originTurnId", &notice.origin_turn_id)] {
        if let Some(value) = value {
            data[key] = serde_json::json!(value);
        }
    }
    if message.created_at_epoch_secs > 0 {
        data["occurredAtEpochSeconds"] = serde_json::json!(message.created_at_epoch_secs);
    }
    let view = StructuredView { schema: VIEW_SCHEMA.into(), data };
    view.canonical_json()?;
    Ok(view)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn notice_json() -> serde_json::Value {
        json!({"noticeId":"task-a","taskId":"task-a","taskType":"local_agent","status":"completed",
               "description":"Review docs","summary":"All good.","summaryTruncated":false,
               "originTurnId":"turn-1","toolUseId":"toolu_1","transcript":"claude:sid:task:task-a"})
    }

    pub(crate) fn message(owner: &str) -> AgentBusMessage {
        let notice: ClaudeTaskNotice = serde_json::from_value(notice_json()).unwrap();
        let request = send_request(owner, &notice).unwrap();
        AgentBusMessage {
            id: "m1".into(),
            kind: AgentBusEnvelopeKind::Message,
            from: SOURCE_ID.into(),
            to: "runtime-1".into(),
            from_cutex_session_id: None,
            to_cutex_session_id: Some(owner.into()),
            content: request.content.clone(),
            delivery_mode: AgentDeliveryMode::AfterTurn,
            trigger_turn: true,
            created_at_epoch_secs: 1,
            sender_kind: AgentMessageKind::Agent,
            display_source: None,
            submit_mode: None,
            control_type: request.control_type.clone(),
            control_payload: request.control_payload.clone(),
            external_action_id: None,
            external_message_id: request.external_message_id.clone(),
        }
    }

    #[test]
    fn notice_is_read_from_task_updated_and_bounded() {
        let params = json!({"threadId":"t","task":{"taskId":"task-a","status":"completed","notifyParent":notice_json()}});
        let notice = ClaudeTaskNotice::from_task_updated(&params).unwrap();
        assert_eq!(notice.status, "completed");
        assert!(ClaudeTaskNotice::from_task_updated(&json!({"task":{"taskId":"x"}})).is_none());
        let mut long = notice_json();
        long["summary"] = json!("x".repeat(5000));
        let notice = ClaudeTaskNotice::from_task_updated(&json!({"task":{"notifyParent":long}})).unwrap();
        assert_eq!(notice.summary.unwrap().chars().count(), 4000);
        assert!(notice.summary_truncated);
        let mut bad = notice_json();
        bad["status"] = json!("running");
        assert!(ClaudeTaskNotice::from_task_updated(&json!({"task":{"notifyParent":bad}})).is_none());
    }

    #[test]
    fn message_id_is_stable_per_session_and_notice() {
        assert_eq!(message_id("cutex.a", "task-a"), message_id("cutex.a", "task-a"));
        assert_ne!(message_id("cutex.a", "task-a"), message_id("cutex.b", "task-a"));
        assert_ne!(message_id("cutex.a", "task-a"), message_id("cutex.a", "task-b"));
        assert!(message_id("cutex.a", "x").starts_with(MESSAGE_ID_PREFIX));
    }

    #[test]
    fn send_gate_and_provenance() {
        let notice: ClaudeTaskNotice = serde_json::from_value(notice_json()).unwrap();
        let request = send_request("cutex.a", &notice).unwrap();
        assert!(is_claude_task_request(&request));
        validate_request(&request, SOURCE_ID).unwrap();
        assert!(validate_request(&request, "someone").is_err());
        let mut forged = request.clone();
        forged.from_agent_id = Some("agent-1".into());
        assert!(validate_request(&forged, SOURCE_ID).is_err());
        let mut other_id = request.clone();
        other_id.external_message_id = Some("ctask-x".into());
        assert!(validate_request(&other_id, SOURCE_ID).is_err());
        let mut passive = request.clone();
        passive.delivery_mode = Some(AgentDeliveryMode::Passive);
        passive.trigger_turn = Some(false);
        assert!(validate_request(&passive, SOURCE_ID).is_err());
        let message = message("cutex.a");
        model_text(&message).unwrap();
        let mut sender = message.clone();
        sender.from_cutex_session_id = Some("cutex.b".into());
        assert!(model_text(&sender).is_err());
        let mut retargeted = message.clone();
        retargeted.to_cutex_session_id = Some("cutex.b".into());
        assert!(model_text(&retargeted).is_err());
    }

    #[test]
    fn text_says_what_ended_and_carries_the_reply() {
        let (_, text) = model_text(&message("cutex.a")).unwrap();
        assert!(text.starts_with("Message Type: MESSAGE\nSender: Claude Task (cutex: one of your own background tasks ended)\nYour background subagent finished.\nTask: Review docs\n"), "{text}");
        assert!(text.contains("Transcript: claude:sid:task:task-a\n"));
        assert!(text.ends_with("Its final reply:\nAll good."), "{text}");
        let mut lost = message("cutex.a");
        let mut payload = notice_json();
        payload["status"] = json!("lost");
        payload["summary"] = serde_json::Value::Null;
        payload["reason"] = json!("The Claude runtime was closed while it ran.");
        lost.control_payload = Some(payload);
        let (_, text) = model_text(&lost).unwrap();
        assert!(text.contains("Your background subagent was lost: The Claude runtime was closed while it ran. Its work may be partial"), "{text}");
        assert!(text.ends_with("No final reply was reported."));
        let (notice, _) = model_text(&message("cutex.a")).unwrap();
        let view = view(&message("cutex.a"), &notice).unwrap();
        assert_eq!(view.schema, VIEW_SCHEMA);
        assert_eq!(view.data["senderName"], SOURCE_ID);
        assert_eq!(view.data["deliveryKind"], "wake");
        assert_eq!(view.data["status"], "completed");
    }

    #[test]
    fn coalesced_shell_notice_lists_its_commands() {
        let mut message = message("cutex.a");
        let payload = json!({"noticeId":"shells:b1","taskId":"b2","taskType":"local_bash","status":"completed",
            "description":"2 background commands","tasks":[{"taskId":"b1","status":"completed","description":"sleep 1"},
            {"taskId":"b2","status":"failed","description":"make"}]});
        message.control_payload = Some(payload.clone());
        message.external_message_id = Some(message_id("cutex.a", "shells:b1"));
        let (_, text) = model_text(&message).unwrap();
        assert!(text.contains("Your background commands completed") || text.contains("Your background commands finished."), "{text}");
        assert!(text.contains("- b1 (completed): sleep 1\n- b2 (failed): make\n"), "{text}");
    }
}
