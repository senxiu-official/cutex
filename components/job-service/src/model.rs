use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const CONTRACT: &str = "cutex/job-service-core/v1";
pub const GRANT_CONTRACT: &str = "cutex/job-execution-grant/v1";
pub const CALLER_GRANT_CONTRACT: &str = "cutex/job-caller-grant/v1";
pub const COMPLETION_CONTRACT: &str = "cutex.job_service.completion.v1";
pub const COMPLETION_CONTRACT_V2: &str = "cutex.job_service.completion.v2";

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompletionWireVersion {
    #[default]
    V1,
    V2,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JobRequest {
    pub action_id: String,
    pub argv: Vec<String>,
    pub cwd: String,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    pub subscriber_cutex_session_id: String,
    pub origin: ExecutionOrigin,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionOrigin {
    pub runtime_agent_id: String,
    pub native_thread_id: String,
    pub permission_profile_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PersistedJobRequest {
    pub action_id: String,
    pub argument_count: usize,
    /// Human display only: argv joined with spaces, bounded to
    /// [`COMMAND_SUMMARY_MAX_CHARS`] chars. Absent on records written before it existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_summary: Option<String>,
    pub cwd: String,
    pub environment_names: Vec<String>,
    pub subscriber_cutex_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<ExecutionOrigin>,
}

pub const COMMAND_SUMMARY_MAX_CHARS: usize = 300;

/// Bounded, char-safe display summary of argv (joined with spaces, no shell quoting).
pub fn command_summary(argv: &[String]) -> String {
    argv.join(" ")
        .chars()
        .take(COMMAND_SUMMARY_MAX_CHARS)
        .collect()
}

impl From<&JobRequest> for PersistedJobRequest {
    fn from(request: &JobRequest) -> Self {
        Self {
            action_id: request.action_id.clone(),
            argument_count: request.argv.len(),
            command_summary: Some(command_summary(&request.argv)),
            cwd: request.cwd.clone(),
            environment_names: request.environment.keys().cloned().collect(),
            subscriber_cutex_session_id: request.subscriber_cutex_session_id.clone(),
            origin: Some(request.origin.clone()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionGrantPayload {
    pub schema: String,
    pub grant_id: String,
    pub request_sha256: String,
    pub subject_cutex_session_id: String,
    pub operating_system_uid: u32,
    pub cwd: String,
    pub sandbox_state: Value,
    pub launcher_path: String,
    pub launcher_sha256: String,
    pub issued_at_epoch_secs: u64,
    pub expires_at_epoch_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionGrant {
    pub payload: ExecutionGrantPayload,
    pub hmac_sha256: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CallerOperation {
    Query,
    Cancel,
    ReadOutput,
    PrepareRemote,
    RegisterRemote,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CallerGrantPayload {
    pub schema: String,
    pub grant_id: String,
    pub subject_cutex_session_id: String,
    pub operating_system_uid: u32,
    pub operation: CallerOperation,
    pub job_id: String,
    pub issued_at_epoch_secs: u64,
    pub expires_at_epoch_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CallerGrant {
    pub payload: CallerGrantPayload,
    pub hmac_sha256: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Registered,
    Queued,
    Stale,
    LaunchPending,
    Running,
    Exited,
    Failed,
    Cancelled,
    Interrupted,
    LaunchUnknown,
}

impl JobState {
    pub fn terminal(self) -> bool {
        matches!(
            self,
            Self::Exited | Self::Failed | Self::Cancelled | Self::Interrupted | Self::LaunchUnknown
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StreamSummary {
    pub retained_bytes: u64,
    pub observed_bytes: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionObservation {
    pub basis: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_observed_at_epoch_millis: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_observed_at_epoch_millis: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_run_duration_millis: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompletionFactsV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_resources: Option<ExternalResources>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_id: Option<String>,
    pub facts_version: u8,
    pub action_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<ExecutionObservation>,
    pub stdout: StreamSummary,
    pub stderr: StreamSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FrozenCompletionRequestV2 {
    pub schema: String,
    pub event_id: String,
    pub job_id: String,
    pub job_revision: u64,
    pub terminal_status: String,
    pub result_sha256: String,
    pub target_cutex_session_id: String,
    pub facts: CompletionFactsV1,
    pub output_reference: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct JobRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<ExternalJob>,
    pub schema: String,
    pub job_id: String,
    pub revision: u64,
    pub request_sha256: String,
    pub request: PersistedJobRequest,
    pub state: JobState,
    pub created_at_epoch_secs: u64,
    pub updated_at_epoch_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_start_ticks: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<ExecutionObservation>,
    pub stdout: StreamSummary,
    pub stderr: StreamSummary,
    pub output_reference: String,
    #[serde(default)]
    pub completion_delivery: CompletionDeliverySummary,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompletionDeliveryState {
    #[default]
    Disabled,
    AwaitingTerminal,
    Ready,
    Sending,
    RetryPending,
    AcceptedPending,
    Archived,
    Delivered,
    Orphaned,
    Unavailable,
    AuthorizationFailed,
    Conflict,
    OperatorRequired,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CompletionDeliverySummary {
    /// Whether this daemon has a configured completion delivery worker.
    #[serde(default)]
    pub enabled: bool,
    pub state: CompletionDeliveryState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OutboxRecord {
    pub event_id: String,
    pub job_id: String,
    pub job_revision: u64,
    pub subscriber_cutex_session_id: String,
    pub terminal_state: JobState,
    pub result_sha256: String,
    pub output_reference: String,
    /// Absent means the exact legacy v1 request builder and digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_version: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frozen_request_v2: Option<FrozenCompletionRequestV2>,
    pub acknowledged: bool,
    #[serde(default)]
    pub delivery_state: CompletionDeliveryState,
    #[serde(default)]
    pub attempt_count: u32,
    #[serde(default)]
    pub next_attempt_at_epoch_millis: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_attempt_at_epoch_millis: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<CompletionReceipt>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompletionReceipt {
    pub schema: String,
    pub status: String,
    pub event_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    pub disposition: String,
    pub deduplicated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a4_receipt: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SubmitReceipt {
    pub status: String,
    pub job: JobRecord,
    pub deduplicated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OutputPage {
    pub job_id: String,
    pub stream: String,
    pub from_offset: u64,
    pub next_offset: u64,
    pub bytes_hex: String,
    pub gap: bool,
    #[serde(default)]
    pub omitted_bytes: u64,
    #[serde(default)]
    pub eof: bool,
    pub truncated: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum JobError {
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("unauthorized: {0}")]
    Unauthorized(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("I/O failure: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization failure: {0}")]
    Serde(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteJobRequest {
    pub source: String,
    pub correlation_token: String,
    pub remote_id: String,
    pub remote_name: Option<String>,
    pub project: Option<String>,
    pub queue: Option<String>,
    pub remote_output_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExternalJob {
    pub source: String,
    pub remote_id: String,
    pub remote_name: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub correlation_token: String,
    pub project: Option<String>,
    pub queue: Option<String>,
    pub remote_output_path: Option<String>,
    #[serde(default)]
    pub stale: bool,
    pub registered_at: u64,
    pub last_source_event_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub human_audit: Vec<ExternalHumanAudit>,
    #[serde(default)]
    pub source_events: Vec<ExternalSourceReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ExternalResources>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExternalHumanAudit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<ExternalAuditActor>,
    pub at: u64,
    pub previous_revision: u64,
    pub state: JobState,
    pub exit_code: Option<i32>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrepareRemoteReceipt {
    pub source: String,
    pub correlation_token: String,
    pub suggested_job_name: String,
    /// Name and mail-event arguments only (pre-mail-address field, unchanged).
    pub submission_arguments: Vec<String>,
    /// PBS always needs a `-M` destination for these mails; when
    /// `notificationStatus` is `configured`, `pbsArgs` already contains it.
    pub notification_destination_required: bool,
    pub expires_at: u64,
    /// Complete `qsub` arguments to pass verbatim: `-N NAME -m abe`, plus
    /// `-M ADDRESS` when the owner configured the notification mailbox.
    #[serde(default)]
    pub pbs_args: Vec<String>,
    /// Owner-configured notification address, if any.
    #[serde(default)]
    pub notification_address: Option<String>,
    /// `configured`, `mailNotConfigured` or `mailConfigUnreadable`.
    #[serde(default)]
    pub notification_status: String,
    /// Plain-language hint for the agent to relay to the owner when no address is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_action: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PreparedRemote {
    pub source: String,
    pub subscriber: String,
    pub expires_at: u64,
    pub job_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExternalAuditActor {
    pub credential_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
}
impl Default for ExternalAuditActor {
    fn default() -> Self {
        Self {
            credential_kind: "job_service_root".into(),
            device_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExternalResources {
    pub cpu_seconds: Option<u64>,
    pub walltime_seconds: Option<u64>,
    pub memory_bytes: Option<u64>,
    pub vmem_bytes: Option<u64>,
    pub ncpus: Option<u64>,
    pub jobfs_bytes: Option<u64>,
    pub cpu_percent: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExternalReason {
    WalltimeExceeded,
    Aborted,
    NonzeroExit,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExternalEventLookup {
    pub source: String,
    pub remote_id: String,
    pub correlation_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExternalVerificationList {
    pub limit: usize,
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalVerificationPage {
    pub targets: Vec<ExternalVerificationTarget>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalVerificationTarget {
    pub current_state: JobState,
    pub remote_id: String,
    pub correlation_token: String,
    pub remote_name: Option<String>,
    pub project: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalEventLookupReceipt {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<ExternalVerificationTarget>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExternalSourceEvent {
    pub source: String,
    pub remote_id: String,
    pub correlation_token: String,
    pub event_id: String,
    pub state: JobState,
    pub exit_code: Option<i32>,
    pub reason: Option<ExternalReason>,
    pub resources: Option<ExternalResources>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExternalSourceReceipt {
    pub event_id_sha256: String,
    pub event_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalEventReceipt {
    pub status: String,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
}

pub fn normalize_nci_remote_id(value: &str) -> Result<String, JobError> {
    let digits = value.strip_suffix(".gadi-pbs").unwrap_or(value);
    if digits.is_empty() || digits.len() > 20 || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return Err(JobError::Invalid(
            "NCI remote ID must be digits with optional .gadi-pbs suffix".into(),
        ));
    }
    Ok(format!("{digits}.gadi-pbs"))
}
