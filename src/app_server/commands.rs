//! Canonical app-server v2 commands used by cutex runtime integrations.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::agent_bus::delivery::AgentDeliveryMode;

use super::client::AppServerClientError;
use super::client::AppServerHandle;

pub const THREAD_START_METHOD: &str = "thread/start";
pub const THREAD_RESUME_METHOD: &str = "thread/resume";
pub const THREAD_READ_METHOD: &str = "thread/read";
pub const MCP_SERVER_TOOL_CALL_METHOD: &str = "mcpServer/tool/call";
pub const TURN_START_METHOD: &str = "turn/start";
pub const TURN_STEER_METHOD: &str = "turn/steer";
pub const TURN_INTERRUPT_METHOD: &str = "turn/interrupt";
pub const THREAD_SETTINGS_UPDATE_METHOD: &str = "thread/settings/update";
pub const THREAD_NAME_SET_METHOD: &str = "thread/name/set";
pub const THREAD_GOAL_SET_METHOD: &str = "thread/goal/set";
pub const THREAD_GOAL_GET_METHOD: &str = "thread/goal/get";
pub const THREAD_GOAL_CLEAR_METHOD: &str = "thread/goal/clear";
pub const THREAD_INTER_AGENT_MESSAGE_METHOD: &str = "thread/inter_agent_message";
pub const THREAD_INTER_AGENT_MESSAGE_STATUS_METHOD: &str = "thread/inter_agent_message/status";
pub const THREAD_CUTEX_ACTIVITY_METHOD: &str = "thread/cutexActivity";

#[derive(Clone)]
pub struct AppServerCommands {
    handle: AppServerHandle,
}

impl AppServerCommands {
    pub fn new(handle: AppServerHandle) -> Self {
        Self { handle }
    }

    pub fn thread_start(&self, params: &ThreadStartParams) -> Result<Value, AppServerClientError> {
        self.request(THREAD_START_METHOD, params)
    }

    pub fn thread_resume(
        &self,
        params: &ThreadResumeParams,
    ) -> Result<Value, AppServerClientError> {
        self.request(THREAD_RESUME_METHOD, params)
    }

    pub fn thread_read(&self, params: &ThreadReadParams) -> Result<Value, AppServerClientError> {
        self.request(THREAD_READ_METHOD, params)
    }

    pub fn turn_start(&self, params: &TurnStartParams) -> Result<Value, AppServerClientError> {
        self.request(TURN_START_METHOD, params)
    }

    pub fn turn_steer(&self, params: &TurnSteerParams) -> Result<Value, AppServerClientError> {
        self.request(TURN_STEER_METHOD, params)
    }

    pub fn turn_interrupt(
        &self,
        params: &TurnInterruptParams,
    ) -> Result<Value, AppServerClientError> {
        self.request(TURN_INTERRUPT_METHOD, params)
    }

    pub fn thread_settings_update(
        &self,
        params: &ThreadSettingsUpdateParams,
    ) -> Result<Value, AppServerClientError> {
        self.request(THREAD_SETTINGS_UPDATE_METHOD, params)
    }

    pub fn thread_name_set(
        &self,
        params: &ThreadNameSetParams,
    ) -> Result<Value, AppServerClientError> {
        self.request(THREAD_NAME_SET_METHOD, params)
    }

    pub fn thread_goal_set(
        &self,
        params: &ThreadGoalSetParams,
    ) -> Result<Value, AppServerClientError> {
        self.request(THREAD_GOAL_SET_METHOD, params)
    }

    pub fn thread_goal_get(
        &self,
        params: &ThreadGoalGetParams,
    ) -> Result<Value, AppServerClientError> {
        self.request(THREAD_GOAL_GET_METHOD, params)
    }

    pub fn thread_goal_clear(
        &self,
        params: &ThreadGoalClearParams,
    ) -> Result<Value, AppServerClientError> {
        self.request(THREAD_GOAL_CLEAR_METHOD, params)
    }

    pub fn thread_inter_agent_message(
        &self,
        params: &ThreadInterAgentMessageParams,
    ) -> Result<ThreadInterAgentMessageResponse, AppServerClientError> {
        let response = self.request(THREAD_INTER_AGENT_MESSAGE_METHOD, params)?;
        serde_json::from_value(response)
            .map_err(|error| AppServerClientError::Protocol(error.to_string()))
    }

    pub fn thread_inter_agent_message_status(
        &self,
        params: &ThreadInterAgentMessageStatusParams,
    ) -> Result<ThreadInterAgentMessageStatusResponse, AppServerClientError> {
        let response = self.request(THREAD_INTER_AGENT_MESSAGE_STATUS_METHOD, params)?;
        serde_json::from_value(response)
            .map_err(|error| AppServerClientError::Protocol(error.to_string()))
    }

    pub fn thread_cutex_activity(
        &self,
        params: &ThreadCutexActivityParams,
    ) -> Result<ThreadCutexActivityResponse, AppServerClientError> {
        let response = self.request(THREAD_CUTEX_ACTIVITY_METHOD, params)?;
        serde_json::from_value(response)
            .map_err(|error| AppServerClientError::Protocol(error.to_string()))
    }

    /// `mcpServer/tool/call {threadId, server, tool, arguments}`: call a tool
    /// of one of the thread's MCP servers as that thread (app-server adds
    /// `_meta.threadId`). Used for delegation `stop_jobs` cancels.
    pub fn mcp_server_tool_call(&self, params: &Value) -> Result<Value, AppServerClientError> {
        self.request(MCP_SERVER_TOOL_CALL_METHOD, params)
    }

    fn request<T: Serialize + ?Sized>(
        &self,
        method: &str,
        params: &T,
    ) -> Result<Value, AppServerClientError> {
        let params = serde_json::to_value(params)
            .map_err(|error| AppServerClientError::Protocol(error.to_string()))?;
        self.handle.request(method, params)
    }
}

/// UI-only Cutex activity request. Deliberately has no `turn_id`: omission is
/// the downstream contract for the stable Cutex-owned transient lane.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadCutexActivityParams {
    pub thread_id: String,
    pub delivery: CutexUiActivityDelivery,
    pub activity: CutexUiActivity,
}

pub const CUTEX_UI_ACTIVITY_DELIVERY_SCHEMA: &str = "cutex/ui-activity-delivery/v1";

/// Strict transport metadata for the UI-only Cutex activity lane. The
/// activity remains separate so downstream ingestion cannot confuse recovery
/// mechanics with model-visible content.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CutexUiActivityDelivery {
    pub schema: CutexUiActivityDeliverySchema,
    pub class: CutexUiActivityDeliveryClass,
    pub recovered: bool,
    pub batch_id: String,
    pub batch_index: u32,
    pub batch_size: u32,
    pub source_checkpoint: CutexUiActivityCheckpoint,
    pub batch_checkpoint: CutexUiActivityCheckpoint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum CutexUiActivityDeliverySchema {
    #[serde(rename = "cutex/ui-activity-delivery/v1")]
    V1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CutexUiActivityDeliveryClass {
    Live,
    CatchUp,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CutexUiActivityCheckpoint {
    pub stream_id: String,
    pub cursor: String,
    pub sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadCutexActivityResponse {
    pub submission_id: String,
    pub disposition: CutexUiActivityIngestionDisposition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CutexUiActivityIngestionDisposition {
    Accepted,
    Duplicate,
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
#[allow(clippy::large_enum_variant)] // Frozen wire variants serialize directly without indirection.
pub enum CutexUiActivity {
    ManagedAgentActivity(ManagedAgentActivityItem),
    OutboundInterAgentMessage(OutboundInterAgentMessageItem),
    // r219: the Task Service variants (taskAssignmentActivity /
    // taskWatchdogActivity) are removed; the activity projector maps a pending
    // entry of either type to a skip when it loads its state.
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedAgentActivityItem {
    pub id: String,
    pub event_id: String,
    pub sequence: u64,
    pub occurred_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase_event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<ManagedAgentActionPhase>,
    pub operation: ManagedAgentOperation,
    pub status: ManagedAgentActivityStatus,
    pub managed_agent_id: String,
    pub managed_agent_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_agent_metadata: Option<ParticipantPresentationMetadata>,
    pub managed_agent_role: Option<String>,
    pub initial_task_preview: Option<String>,
    pub detail: Option<String>,
    pub runtime_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predecessor_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predecessor_agent_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predecessor_metadata: Option<ParticipantPresentationMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub successor_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub successor_agent_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub successor_metadata: Option<ParticipantPresentationMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace_policy: Option<ManagedAgentReplacePolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation_mode: Option<ManagedAgentRotationMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority_epoch: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ManagedAgentActionPhase {
    Prepared,
    PrivateCwdReady,
    NativeBootstrapPending,
    NativeSessionCaptured,
    Adopted,
    Configured,
    Online,
    Ready,
    MessagePending,
    MessageQueued,
    PredecessorClosing,
    PredecessorClosed,
    AuthorityTransferPending,
    AuthorityTransferred,
    SuccessorReady,
    Complete,
    NoWrite,
    OwnerActionRequired,
    Failure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ManagedAgentReplacePolicy {
    CloseBeforeCreate,
    CloseAfterReady,
    KeepOld,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ManagedAgentRotationMode {
    ClosePredecessorThenCreateWithMessage,
    RetainPredecessorWithMessage,
    RetainPredecessorBootstrapOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ManagedAgentOperation {
    Create,
    Online,
    Offline,
    Restart,
    Replace,
    Close,
    DirectorRotate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ManagedAgentActivityStatus {
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboundInterAgentMessageItem {
    pub id: String,
    pub event_id: String,
    pub sequence: u64,
    pub occurred_at_ms: i64,
    pub sender_agent_id: String,
    pub sender_agent_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_metadata: Option<ParticipantPresentationMetadata>,
    pub recipient_agent_id: String,
    pub recipient_agent_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient_metadata: Option<ParticipantPresentationMetadata>,
    #[serde(default)]
    pub other_recipient_agent_ids: Vec<String>,
    pub delivery_mode: AgentDeliveryMode,
    pub status: OutboundInterAgentMessageStatus,
    pub content_preview: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum OutboundInterAgentMessageStatus {
    Sending,
    Sent,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Replaces the thread runtime workspace roots (cwd + project memory dir).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_workspace_roots: Option<Vec<PathBuf>>,
    /// cutex-only: resolved by the runtime manager against `config/read`
    /// before sending (appends file-citation / project memory sections).
    #[serde(skip)]
    pub launch_augmentation: Option<crate::memory::LaunchAugmentation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approvals_reviewer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<BTreeMap<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub developer_instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub personality: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ephemeral: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_start_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_source: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadResumeParams {
    pub thread_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_turns: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initial_turns_page: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Replaces the thread runtime workspace roots (cwd + project memory dir).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_workspace_roots: Option<Vec<PathBuf>>,
    /// cutex-only: resolved by the runtime manager against `config/read`
    /// before sending (appends file-citation / project memory sections).
    #[serde(skip)]
    pub launch_augmentation: Option<crate::memory::LaunchAugmentation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approvals_reviewer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<BTreeMap<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub developer_instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub personality: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadReadParams {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_turns: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type")]
pub enum UserInput {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(rename = "text_elements")]
        text_elements: Vec<Value>,
    },
    #[serde(rename = "image")]
    Image {
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        url: String,
    },
    #[serde(rename = "localImage")]
    LocalImage {
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        path: String,
    },
    #[serde(rename = "skill")]
    Skill { name: String, path: String },
    #[serde(rename = "mention")]
    Mention { name: String, path: String },
}

impl UserInput {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            text_elements: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartParams {
    pub thread_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_user_message_id: Option<String>,
    pub input: Vec<UserInput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approvals_reviewer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_policy: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub personality: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnSteerParams {
    pub thread_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_user_message_id: Option<String>,
    pub input: Vec<UserInput>,
    pub expected_turn_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnInterruptParams {
    pub thread_id: String,
    pub turn_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSettingsUpdateParams {
    pub thread_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approvals_reviewer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_policy: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collaboration_mode: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub personality: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadNameSetParams {
    pub thread_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ThreadGoalStatus {
    Active,
    Paused,
    Blocked,
    UsageLimited,
    BudgetLimited,
    Complete,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadGoalSetParams {
    pub thread_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub objective: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<ThreadGoalStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadGoalGetParams {
    pub thread_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadGoalClearParams {
    pub thread_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadInterAgentMessageParams {
    pub thread_id: String,
    pub message_id: String,
    pub author: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_metadata: Option<ParticipantPresentationMetadata>,
    pub recipient: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient_metadata: Option<ParticipantPresentationMetadata>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub other_recipients: Vec<String>,
    pub content: String,
    pub delivery_mode: AgentDeliveryMode,
}

/// Presentation-only identity supplied by authenticated Cutex integrations.
/// Every field is optional so historical and non-Cutex participants remain
/// wire-compatible. None of these values establishes model or task authority.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParticipantPresentationMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cutex_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_backend: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadInterAgentMessageResponse {
    pub submission_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadInterAgentMessageStatusParams {
    pub thread_id: String,
    pub messages: Vec<ThreadInterAgentMessageStatusQuery>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadInterAgentMessageStatusQuery {
    pub message_id: String,
    pub semantic_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadInterAgentMessageStatusResponse {
    pub schema: String,
    pub thread_id: String,
    pub statuses: Vec<ThreadInterAgentMessageStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadInterAgentMessageDeliveryState {
    Unknown,
    Pending,
    ContextPersisted,
    Conflict,
    RetryableError,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadInterAgentMessageStatus {
    pub message_id: String,
    pub state: ThreadInterAgentMessageDeliveryState,
    pub semantic_sha256: String,
    #[serde(default)]
    pub receipt: Option<InterAgentContextPersistedReceipt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InterAgentContextPersistedReceipt {
    #[serde(default)]
    pub schema: Option<String>,
    pub receipt_id: String,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub semantic_sha256: Option<String>,
    pub response_item_id: String,
    pub turn_id: String,
    pub rollout_ordinal: u64,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn turn_commands_use_native_ids_and_input_shape() {
        let turn = TurnStartParams {
            thread_id: "thread-1".to_string(),
            client_user_message_id: Some("message-1".to_string()),
            input: vec![UserInput::text("hello")],
            cwd: None,
            approval_policy: None,
            approvals_reviewer: None,
            sandbox_policy: None,
            permissions: None,
            model: None,
            service_tier: None,
            effort: None,
            summary: None,
            personality: None,
            output_schema: None,
        };

        assert_eq!(
            serde_json::to_value(turn).expect("serialize turn"),
            json!({
                "threadId": "thread-1",
                "clientUserMessageId": "message-1",
                "input": [{
                    "type": "text",
                    "text": "hello",
                    "text_elements": []
                }]
            })
        );
    }

    #[test]
    fn settings_distinguish_omitted_and_cleared_service_tier() {
        let omitted = ThreadSettingsUpdateParams {
            thread_id: "thread-1".to_string(),
            ..Default::default()
        };
        let cleared = ThreadSettingsUpdateParams {
            thread_id: "thread-1".to_string(),
            service_tier: Some(None),
            ..Default::default()
        };

        assert!(serde_json::to_value(omitted)
            .expect("serialize settings")
            .get("serviceTier")
            .is_none());
        assert_eq!(
            serde_json::to_value(cleared).expect("serialize settings")["serviceTier"],
            Value::Null
        );
    }

    #[test]
    fn inter_agent_command_uses_delivery_contract_without_legacy_trigger() {
        let params = ThreadInterAgentMessageParams {
            thread_id: "thread-1".to_string(),
            message_id: "message-1".to_string(),
            author: "/root/sender".to_string(),
            author_metadata: None,
            recipient: "/root/recipient".to_string(),
            recipient_metadata: None,
            other_recipients: Vec::new(),
            content: "hello".to_string(),
            delivery_mode: AgentDeliveryMode::AfterTurn,
        };
        let value = serde_json::to_value(params).expect("serialize inter-agent message");

        assert_eq!(value["messageId"], "message-1");
        assert_eq!(value["deliveryMode"], "after_turn");
        assert!(value.get("triggerTurn").is_none());
        assert!(value.get("otherRecipients").is_none());
    }

    #[test]
    fn goal_status_uses_native_camel_case_values() {
        assert_eq!(
            serde_json::to_value(ThreadGoalStatus::UsageLimited).expect("serialize status"),
            "usageLimited"
        );
    }
}
