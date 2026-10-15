//! Core agent bus protocol enums shared by CLI, management, and routing code.

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::agent_bus::delivery::AgentDeliveryMode;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentMessageKind {
    #[default]
    Agent,
    User,
    Owner,
    /// r219: legacy, decode-only; never produced. Durable message state
    /// (agent-bus-message-state.json) may still hold envelopes of the removed
    /// Task Service; ordinary senders can never claim this kind.
    TaskServiceSystem,
    JobServiceSystem,
    AlarmSystem,
}

impl AgentMessageKind {
    pub fn is_agent(&self) -> bool {
        matches!(self, AgentMessageKind::Agent)
    }

    pub fn is_task_service_system(&self) -> bool {
        matches!(self, AgentMessageKind::TaskServiceSystem)
    }

    pub fn is_job_service_system(&self) -> bool {
        matches!(self, AgentMessageKind::JobServiceSystem)
    }

    pub fn is_alarm_system(&self) -> bool {
        matches!(self, AgentMessageKind::AlarmSystem)
    }

    pub fn sender_label(&self) -> &'static str {
        match self {
            AgentMessageKind::Agent => "cutex",
            AgentMessageKind::User => "user",
            AgentMessageKind::Owner => "owner",
            AgentMessageKind::TaskServiceSystem => "task_service",
            AgentMessageKind::JobServiceSystem => "job_service",
            AgentMessageKind::AlarmSystem => "alarm_service",
        }
    }
}

pub const JOB_SERVICE_COMPLETION_SCHEMA: &str = "cutex.job_service.completion.v1";
pub const JOB_SERVICE_COMPLETION_MAX_BODY_BYTES: usize = 32 * 1024;
pub const JOB_SERVICE_COMPLETION_MAX_SUMMARY_BYTES: usize = 2 * 1024;
pub const JOB_SERVICE_COMPLETION_MAX_OUTPUT_REF_BYTES: usize = 2 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobServiceTerminalStatus {
    Exited,
    Failed,
    Cancelled,
    Interrupted,
    LaunchUnknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JobServiceCompletionRequest {
    pub schema: String,
    pub event_id: String,
    pub job_id: String,
    pub job_revision: u64,
    pub terminal_status: JobServiceTerminalStatus,
    pub result_sha256: String,
    pub target_cutex_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_reference: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JobServiceCompletionQuery {
    pub schema: String,
    pub event_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobServiceCompletionDisposition {
    Pending,
    Delivered,
    Archived,
    Orphaned,
    NotFound,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JobServiceCompletionReceipt {
    pub schema: String,
    pub status: String,
    pub event_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    pub disposition: JobServiceCompletionDisposition,
    pub deduplicated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a4_receipt: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UserSubmitMode {
    Queue,
    #[default]
    NextToolCall,
    Interrupt,
}

impl UserSubmitMode {
    pub fn label(&self) -> &'static str {
        match self {
            UserSubmitMode::Queue => "queue",
            UserSubmitMode::NextToolCall => "next_tool_call",
            UserSubmitMode::Interrupt => "interrupt",
        }
    }

    pub fn delivery_boundary(&self) -> &'static str {
        match self {
            UserSubmitMode::Queue => "after_current_turn",
            UserSubmitMode::NextToolCall => "next_model_boundary",
            UserSubmitMode::Interrupt => "interrupt_current_turn",
        }
    }

    pub fn turn_acceptance(&self) -> &'static str {
        match self {
            UserSubmitMode::Queue => "after_turn",
            UserSubmitMode::NextToolCall => "next_boundary",
            UserSubmitMode::Interrupt => "interrupt",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentBusEnvelopeKind {
    #[default]
    Message,
    Control,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentRegistrationClass {
    Persistent,
    Ephemeral,
    #[default]
    LocalOnly,
}

impl AgentRegistrationClass {
    pub fn label(&self) -> &'static str {
        match self {
            AgentRegistrationClass::Persistent => "persistent",
            AgentRegistrationClass::Ephemeral => "ephemeral",
            AgentRegistrationClass::LocalOnly => "local_only",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentBusAgent {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub base_name: Option<String>,
    #[serde(default, alias = "thread_name")]
    pub thread_name: Option<String>,
    #[serde(default)]
    pub path_key: Option<String>,
    #[serde(default, alias = "session_id")]
    pub session_id: Option<String>,
    /// Stable durable identity projected only for the current runtime endpoint.
    #[serde(
        default,
        alias = "cutexSessionId",
        skip_serializing_if = "Option::is_none"
    )]
    pub cutex_session_id: Option<String>,
    pub profile: String,
    pub cwd: String,
    pub pid: u32,
    #[serde(default, alias = "hostId", skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default, alias = "registration_class")]
    pub registration_class: AgentRegistrationClass,
    pub last_seen_epoch_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentBusRegisterRequest {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub base_name: Option<String>,
    #[serde(default, alias = "thread_name")]
    pub thread_name: Option<String>,
    #[serde(default)]
    pub path_key: Option<String>,
    #[serde(default, alias = "session_id")]
    pub session_id: Option<String>,
    pub profile: String,
    pub cwd: String,
    #[serde(default)]
    pub pid: u32,
    #[serde(default, alias = "host_id")]
    pub host_id: Option<String>,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default, alias = "registration_class")]
    pub registration_class: AgentRegistrationClass,
}

/// Returns the stable logical recipient bytes used by the ordinary-message
/// semantic digest on both sides of the Agent Bus bridge.
///
/// Runtime IDs and display names may change when a durable Agent is restarted.
/// A nonblank base name is therefore authoritative when the registration
/// provides one. The remaining fallbacks preserve registrations that predate
/// base names without inferring identity from presentation metadata.
pub fn canonical_recipient_label<'a>(
    base_name: Option<&'a str>,
    registration_name: &'a str,
    runtime_id: &'a str,
) -> &'a str {
    base_name
        .filter(|label| !label.trim().is_empty())
        .or_else(|| (!registration_name.trim().is_empty()).then_some(registration_name))
        .unwrap_or(runtime_id)
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct AgentBusRegistry {
    #[serde(default)]
    pub agents: std::collections::HashMap<String, AgentBusAgent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentGroupUpdateMode {
    #[default]
    Set,
    Add,
    Remove,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentBusGroupUpdateRequest {
    pub target: String,
    pub groups: Vec<String>,
    #[serde(default)]
    pub mode: AgentGroupUpdateMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentBusGroupUpdateResponse {
    #[serde(default)]
    pub ok: bool,
    #[serde(default, alias = "agentId")]
    pub agent_id: Option<String>,
    #[serde(default, alias = "agentName")]
    pub agent_name: Option<String>,
    #[serde(default)]
    pub groups: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentBusHeartbeatRequest {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentBusUnregisterRequest {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentBusUnregisterResponse {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub removed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentBusMessage {
    pub id: String,
    #[serde(default)]
    pub kind: AgentBusEnvelopeKind,
    pub from: String,
    pub to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_cutex_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_cutex_session_id: Option<String>,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub delivery_mode: AgentDeliveryMode,
    pub trigger_turn: bool,
    pub created_at_epoch_secs: u64,
    #[serde(default)]
    pub sender_kind: AgentMessageKind,
    #[serde(default)]
    pub display_source: Option<String>,
    #[serde(default)]
    pub submit_mode: Option<UserSubmitMode>,
    #[serde(default, alias = "controlType")]
    pub control_type: Option<String>,
    #[serde(default, alias = "controlPayload")]
    pub control_payload: Option<Value>,
    #[serde(default, alias = "externalActionId")]
    pub external_action_id: Option<String>,
    #[serde(default, alias = "externalMessageId")]
    pub external_message_id: Option<String>,
}

/// Message of the removed Task Service (r219): the HTTP 410 route stubs and the
/// terminal failure of a legacy Task Service message use it.
pub const TASK_SERVICE_REMOVED_MESSAGE: &str = "the Task Service was removed in r219";

impl AgentBusMessage {
    /// r219: a message of the removed Task Service (its sender kind or a
    /// `cutex.task_service.*` control type). It still decodes from durable or
    /// queued state but is never delivered: the startup redrive and both
    /// app-server bridges finish it with [`task_service_removed_error`].
    pub fn is_removed_task_service_message(&self) -> bool {
        self.sender_kind.is_task_service_system()
            || self
                .control_type
                .as_deref()
                .is_some_and(|control_type| control_type.starts_with("cutex.task_service."))
    }
}

/// The terminal error recorded for a legacy Task Service message (r219).
pub fn task_service_removed_error() -> Value {
    serde_json::json!({
        "code": "task_service_removed",
        "message": TASK_SERVICE_REMOVED_MESSAGE,
        "retryable": false
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentBusSendRequest {
    pub to: String,
    #[serde(default, alias = "allGroups")]
    pub all_groups: bool,
    #[serde(default)]
    pub kind: AgentBusEnvelopeKind,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default, alias = "fromAgentId")]
    pub from_agent_id: Option<String>,
    #[serde(default, alias = "fromSessionId")]
    pub from_session_id: Option<String>,
    #[serde(default, alias = "toSessionId")]
    pub to_session_id: Option<String>,
    #[serde(default)]
    pub content: String,
    #[serde(default, alias = "deliveryMode")]
    pub delivery_mode: Option<AgentDeliveryMode>,
    #[serde(default, alias = "queueOnly")]
    pub queue_only: Option<bool>,
    #[serde(default, alias = "triggerTurn")]
    pub trigger_turn: Option<bool>,
    #[serde(
        default,
        alias = "senderKind",
        alias = "sender_type",
        alias = "senderType"
    )]
    pub sender_kind: Option<AgentMessageKind>,
    #[serde(default, alias = "displaySource")]
    pub display_source: Option<String>,
    #[serde(default, alias = "submitMode")]
    pub submit_mode: Option<UserSubmitMode>,
    #[serde(default, alias = "controlType")]
    pub control_type: Option<String>,
    #[serde(default, alias = "controlPayload")]
    pub control_payload: Option<Value>,
    #[serde(default, alias = "externalActionId")]
    pub external_action_id: Option<String>,
    #[serde(default, alias = "externalMessageId")]
    pub external_message_id: Option<String>,
    /// Facts about the sender that cutex itself established (in-process
    /// callers only; the HTTP send routes clear a caller-supplied value).
    #[serde(
        default,
        alias = "senderFacts",
        skip_serializing_if = "Option::is_none"
    )]
    pub sender_facts: Option<AgentSenderFacts>,
}

/// Sender facts shown to the recipient's model as `Sender:` header lines and
/// to clients as flags (agent messaging v2). Never part of the semantic hash.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSenderFacts {
    /// The sender's capabilities are narrower than the recipient's.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub narrower_capabilities: bool,
    /// The recipient cannot see the sender in `agent_list`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reply_only: bool,
    /// Consecutive wake round-trips between this pair (wakes only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_chain: Option<u32>,
    /// A requested wake was delivered as a note by the loop guard.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub wake_downgraded: bool,
    /// The sender's native turn the send came from (a tool call inside a turn).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_turn_id: Option<String>,
}

impl AgentBusSendRequest {
    pub fn resolved_delivery_mode(&self) -> AgentDeliveryMode {
        if let Some(mode) = self.delivery_mode.clone() {
            return mode;
        }
        if self.queue_only == Some(true) {
            return AgentDeliveryMode::Passive;
        }
        match self.trigger_turn {
            Some(false) => AgentDeliveryMode::Passive,
            Some(true) | None => AgentDeliveryMode::Soon,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentBusRecentSend {
    pub id: String,
    pub kind: AgentBusEnvelopeKind,
    pub from: String,
    pub to: String,
    pub to_name: String,
    pub delivery_mode: AgentDeliveryMode,
    pub trigger_turn: bool,
    pub queued: bool,
    pub created_at_epoch_secs: u64,
    pub sender_kind: AgentMessageKind,
    pub display_source: Option<String>,
    pub submit_mode: Option<UserSubmitMode>,
    pub control_type: Option<String>,
    #[allow(dead_code)]
    pub control_payload: Option<Value>,
    pub external_action_id: Option<String>,
    pub external_message_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AgentBusSendOutcome {
    pub record: AgentBusRecentSend,
    pub deduplicated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentBusPollResponse {
    pub messages: Vec<AgentBusMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentBusAckRequest {
    pub agent_id: String,
    pub message_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentBusAckResponse {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub acked: usize,
}

#[derive(Debug, Deserialize)]
pub struct AgentBusSendResponse {
    pub id: String,
    #[serde(default)]
    pub from: Option<String>,
    pub to: String,
    #[serde(default)]
    pub to_name: Option<String>,
    #[serde(default)]
    pub from_session_id: Option<String>,
    #[serde(default)]
    pub to_session_id: Option<String>,
    #[serde(default)]
    pub from_runtime_agent_id: Option<String>,
    #[serde(default)]
    pub to_runtime_agent_id: Option<String>,
    #[serde(default)]
    pub from_cutex_session_id: Option<String>,
    #[serde(default)]
    pub to_cutex_session_id: Option<String>,
    #[serde(default)]
    pub delivery_mode: Option<AgentDeliveryMode>,
    pub trigger_turn: bool,
    pub queued: bool,
    #[serde(default, alias = "queueDurability")]
    pub queue_durability: Option<String>,
    #[serde(default, alias = "deliveryState")]
    pub delivery_state: Option<String>,
    #[serde(default, alias = "requiredAckLevel")]
    pub required_ack_level: Option<String>,
    #[serde(default)]
    pub deduplicated: bool,
    #[serde(default)]
    pub external_action_id: Option<String>,
    #[serde(default)]
    pub external_message_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_response_accepts_camel_case_durability_fields() {
        let mut value = serde_json::json!({"id":"alarm-test","to":"runtime","trigger_turn":true,"queued":true,
            "queueDurability":"durable_v2","deliveryState":"pending","requiredAckLevel":"local_inbox"});
        let parsed: AgentBusSendResponse = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(parsed.queue_durability.as_deref(), Some("durable_v2"));
        assert_eq!(parsed.delivery_state.as_deref(), Some("pending"));
        assert_eq!(parsed.required_ack_level.as_deref(), Some("local_inbox"));
        for (camel, snake) in [
            ("queueDurability", "queue_durability"),
            ("deliveryState", "delivery_state"),
            ("requiredAckLevel", "required_ack_level"),
        ] {
            let field = value.as_object_mut().unwrap().remove(camel).unwrap();
            value[snake] = field;
        }
        let parsed: AgentBusSendResponse = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.delivery_state.as_deref(), Some("pending"));
        assert_eq!(parsed.required_ack_level.as_deref(), Some("local_inbox"));
    }

    #[test]
    fn agent_message_kind_default_and_labels_match_wire_compatibility() {
        assert_eq!(AgentMessageKind::default(), AgentMessageKind::Agent);
        assert_eq!(AgentMessageKind::Agent.sender_label(), "cutex");
        assert_eq!(AgentMessageKind::User.sender_label(), "user");
        assert_eq!(AgentMessageKind::Owner.sender_label(), "owner");
        assert_eq!(
            AgentMessageKind::TaskServiceSystem.sender_label(),
            "task_service"
        );
        assert!(AgentMessageKind::Agent.is_agent());
        assert!(!AgentMessageKind::User.is_agent());
        assert!(AgentMessageKind::TaskServiceSystem.is_task_service_system());
    }

    #[test]
    fn agent_registration_class_default_and_labels_match_wire_values() {
        assert_eq!(
            AgentRegistrationClass::default(),
            AgentRegistrationClass::LocalOnly
        );
        assert_eq!(AgentRegistrationClass::Persistent.label(), "persistent");
        assert_eq!(AgentRegistrationClass::Ephemeral.label(), "ephemeral");
        assert_eq!(AgentRegistrationClass::LocalOnly.label(), "local_only");
    }

    #[test]
    fn user_submit_mode_default_and_labels_match_observer_payloads() {
        assert_eq!(UserSubmitMode::default(), UserSubmitMode::NextToolCall);
        assert_eq!(UserSubmitMode::Queue.label(), "queue");
        assert_eq!(
            UserSubmitMode::Queue.delivery_boundary(),
            "after_current_turn"
        );
        assert_eq!(UserSubmitMode::Queue.turn_acceptance(), "after_turn");
        assert_eq!(
            UserSubmitMode::NextToolCall.delivery_boundary(),
            "next_model_boundary"
        );
        assert_eq!(
            UserSubmitMode::Interrupt.delivery_boundary(),
            "interrupt_current_turn"
        );
    }

    #[test]
    fn send_request_resolves_legacy_delivery_mode_fields() {
        let request: AgentBusSendRequest = serde_json::from_value(serde_json::json!({
            "to": "worker",
            "triggerTurn": false
        }))
        .expect("legacy request should parse");
        assert_eq!(request.resolved_delivery_mode(), AgentDeliveryMode::Passive);
        let request: AgentBusSendRequest = serde_json::from_value(serde_json::json!({
            "to": "worker",
            "deliveryMode": "after_turn"
        }))
        .expect("deliveryMode request should parse");
        assert_eq!(
            request.resolved_delivery_mode(),
            AgentDeliveryMode::AfterTurn
        );
    }
}

#[cfg(test)]
mod job_service_completion_tests {
    use super::*;

    #[test]
    fn job_service_completion_request_is_strict_and_system_kind_is_distinct() {
        let json = serde_json::json!({
            "schema": JOB_SERVICE_COMPLETION_SCHEMA,
            "eventId": "event-1",
            "jobId": "job-1",
            "jobRevision": 2,
            "terminalStatus": "exited",
            "resultSha256": "a".repeat(64),
            "targetCutexSessionId": "cutex.11111111-1111-4111-8111-111111111111"
        });
        let parsed: JobServiceCompletionRequest = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(parsed.job_revision, 2);
        let mut changed = json;
        changed["modelSuppliedPrincipal"] = serde_json::json!("forged");
        assert!(serde_json::from_value::<JobServiceCompletionRequest>(changed).is_err());
        assert!(AgentMessageKind::JobServiceSystem.is_job_service_system());
        assert!(!AgentMessageKind::TaskServiceSystem.is_job_service_system());
    }
}
