use std::fmt;

use serde::{Deserialize, Serialize};

use crate::role_revision::{CutexSessionId, Rfc3339, Sha256};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Hash, Serialize)]
#[serde(transparent)]
pub struct AgentActionId(String);

impl AgentActionId {
    pub fn new(value: impl Into<String>) -> Result<Self, AgentManagementError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= 256
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/' | b'@')
            });
        if !valid {
            return Err(AgentManagementError::InvalidRequest("invalid_action_id"));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for AgentActionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for AgentActionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AgentManagementSchema {
    #[serde(rename = "cutex/agent-management/v1")]
    V1,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AgentManagementStoreSchema {
    #[serde(rename = "cutex/agent-management-store/v1")]
    V1,
    /// Private candidate writers only: reviewed bootstrap intents must never be
    /// dropped by an older writer or interpreted as a legacy default launch.
    #[serde(rename = "cutex/agent-management-store/v2")]
    V2,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AgentManagementReceiptSchema {
    #[serde(rename = "cutex/agent-management-receipt/v1")]
    V1,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AgentManagementFailureSchema {
    #[serde(rename = "cutex/agent-management-failure/v1")]
    V1,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum LegacyDirectorOwnershipImportSchema {
    #[serde(rename = "cutex/agent-management/legacy-director-ownership-import/v1")]
    V1,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum LegacyDirectorOwnershipImportReceiptSchema {
    #[serde(rename = "cutex/agent-management/legacy-director-ownership-import-receipt/v1")]
    V1,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AgentReservationReconciliationSchema {
    #[serde(rename = "cutex/agent-management/reservation-reconciliation/v1")]
    V1,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AgentReservationReconciliationReceiptSchema {
    #[serde(rename = "cutex/agent-management/reservation-reconciliation-receipt/v1")]
    V1,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ProjectId(String);

impl ProjectId {
    pub fn new(value: impl Into<String>) -> Result<Self, AgentManagementError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= 128
            && value
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | ':'));
        if !valid {
            return Err(AgentManagementError::InvalidRequest("invalid_project_id"));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ProjectId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for ProjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStartMode {
    BootstrapOnly,
    CustomMessage,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentReplacePolicy {
    CloseBeforeCreate,
    CloseAfterReady,
    KeepOld,
}

/// r219: legacy, decode-only; never produced.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectorRotateMode {
    ClosePredecessorThenCreateWithMessage,
    RetainPredecessorWithMessage,
    RetainPredecessorBootstrapOnly,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentOperatorGrant {
    pub project_id: ProjectId,
    pub operator_cutex_session_id: CutexSessionId,
    pub grant_revision: u64,
    pub granted_at: Rfc3339,
    pub granted_by_primary_director_session: CutexSessionId,
    /// Distinguishes a dedicated Human/Management grant from an Agent action.
    /// The Director field remains the exact project-authority anchor.
    #[serde(default)]
    pub performed_by_human_management: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentOperatorRosterProjection {
    pub grant_revision: u64,
    #[serde(default)]
    pub operators: Vec<AgentOperatorGrant>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentOperatorAuditKind {
    Granted,
    Revoked,
    /// r219: legacy, decode-only; never produced.
    RetainedAfterDirectorRotation,
    RemovedForPrimaryDirector,
    /// r219: legacy, decode-only; never produced.
    RemovedByDirectorRotation,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentOperatorAuditEvent {
    pub event_id: String,
    pub action_id: AgentActionId,
    pub project_id: ProjectId,
    pub operator_cutex_session_id: CutexSessionId,
    pub kind: AgentOperatorAuditKind,
    pub previous_grant_revision: u64,
    pub grant_revision: u64,
    pub primary_director_cutex_session_id: CutexSessionId,
    /// True only when the dedicated authenticated Human/Management control
    /// plane performed the write. The Director above remains the authority
    /// anchor and is not misrepresented as the actor.
    #[serde(default)]
    pub performed_by_human_management: bool,
    pub committed_at: Rfc3339,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedAgentSpec {
    pub name: String,
    /// Normalized on input (`session::metadata::normalize_session_cwd`).
    #[serde(deserialize_with = "crate::session::metadata::deserialize_session_cwd")]
    pub cwd: String,
    /// Configured profile intent; None inherits the effective next-launch profile.
    /// Create requests still require an explicit nonempty profile.
    pub profile: Option<String>,
    pub runtime_backend: String,
    pub model: String,
    pub reasoning: String,
    pub permissions: String,
    pub approval_policy: String,
    pub sandbox_mode: String,
    pub groups: Vec<String>,
    #[serde(default)]
    pub expose_to_im: bool,
    #[serde(default)]
    pub pin: bool,
}

impl ManagedAgentSpec {
    pub fn validate(&self) -> Result<(), AgentManagementError> {
        if self
            .profile
            .as_deref()
            .is_none_or(|profile| profile.trim().is_empty())
        {
            return Err(AgentManagementError::InvalidRequest("invalid_profile"));
        }
        for (value, code) in [
            (&self.name, "invalid_agent_name"),
            (&self.cwd, "invalid_agent_cwd"),
            (&self.runtime_backend, "invalid_runtime_backend"),
            (&self.model, "invalid_model"),
            (&self.permissions, "invalid_permissions"),
            (&self.approval_policy, "invalid_approval_policy"),
            (&self.sandbox_mode, "invalid_sandbox_mode"),
        ] {
            if value.trim().is_empty() {
                return Err(AgentManagementError::InvalidRequest(code));
            }
        }
        let path = std::path::Path::new(&self.cwd);
        if !path.is_absolute() || path.parent().is_none() {
            return Err(AgentManagementError::InvalidRequest("invalid_agent_cwd"));
        }
        let normalized = crate::agent_bus::identity::normalize_agent_groups(self.groups.clone());
        if normalized.is_empty() || normalized != self.groups {
            return Err(AgentManagementError::InvalidRequest("invalid_agent_groups"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentOperation {
    Create {
        spec: ManagedAgentSpec,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bootstrap_intent: Option<AgentActionId>,
        start_mode: AgentStartMode,
        #[serde(default)]
        frozen_message: Option<String>,
    },
    QueryManaged,
    Online {
        cutex_session_id: CutexSessionId,
    },
    Offline {
        cutex_session_id: CutexSessionId,
    },
    Restart {
        cutex_session_id: CutexSessionId,
    },
    /// Legacy wire name: reversible soft archive since r264. Permanent
    /// retirement is a separate owner-only request, never an AgentOperation.
    Close {
        cutex_session_id: CutexSessionId,
    },
    Replace {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bootstrap_intent: Option<AgentActionId>,
        predecessor_cutex_session_id: CutexSessionId,
        policy: AgentReplacePolicy,
        successor: ManagedAgentSpec,
        start_mode: AgentStartMode,
        #[serde(default)]
        frozen_message: Option<String>,
    },
    GrantOperator {
        operator_cutex_session_id: CutexSessionId,
        expected_grant_revision: u64,
    },
    RevokeOperator {
        operator_cutex_session_id: CutexSessionId,
        expected_grant_revision: u64,
    },
    /// r219: legacy, decode-only; never produced. Persisted actions, phase
    /// events, receipts and bootstrap intents may still name it; a new
    /// request with it is refused with `tool_removed` (see `validate`).
    DirectorRotate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bootstrap_intent: Option<AgentActionId>,
        expected_predecessor_cutex_session: CutexSessionId,
        expected_authority_epoch: u64,
        mode: DirectorRotateMode,
        successor: ManagedAgentSpec,
        #[serde(default)]
        frozen_message: Option<String>,
    },
}

impl AgentOperation {
    pub fn bootstrap_intent(&self) -> Option<&AgentActionId> {
        match self {
            Self::Create {
                bootstrap_intent, ..
            }
            | Self::Replace {
                bootstrap_intent, ..
            }
            | Self::DirectorRotate {
                bootstrap_intent, ..
            } => bootstrap_intent.as_ref(),
            _ => None,
        }
    }

    pub fn bootstrap_spec(&self) -> Option<&ManagedAgentSpec> {
        match self {
            Self::Create { spec, .. } => Some(spec),
            Self::Replace { successor, .. } | Self::DirectorRotate { successor, .. } => {
                Some(successor)
            }
            _ => None,
        }
    }
    pub fn kind(&self) -> AgentOperationKind {
        match self {
            Self::Create { .. } => AgentOperationKind::Create,
            Self::QueryManaged => AgentOperationKind::QueryManaged,
            Self::Online { .. } => AgentOperationKind::Online,
            Self::Offline { .. } => AgentOperationKind::Offline,
            Self::Restart { .. } => AgentOperationKind::Restart,
            Self::Close { .. } => AgentOperationKind::Close,
            Self::Replace { .. } => AgentOperationKind::Replace,
            Self::GrantOperator { .. } => AgentOperationKind::GrantOperator,
            Self::RevokeOperator { .. } => AgentOperationKind::RevokeOperator,
            Self::DirectorRotate { .. } => AgentOperationKind::DirectorRotate,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentOperationKind {
    Create,
    QueryManaged,
    Online,
    Offline,
    Restart,
    Close,
    Replace,
    GrantOperator,
    RevokeOperator,
    /// r219: legacy, decode-only; never produced.
    DirectorRotate,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AgentManagementRequest {
    pub schema: AgentManagementSchema,
    pub action_id: AgentActionId,
    /// Optional project selector. This value narrows authority already held by
    /// the authenticated caller; it never grants authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    #[serde(flatten)]
    pub operation: AgentOperation,
}

#[derive(Deserialize)]
struct AgentManagementRequestWire {
    schema: AgentManagementSchema,
    action_id: AgentActionId,
    #[serde(default)]
    project_id: Option<ProjectId>,
    #[serde(flatten)]
    operation: AgentOperation,
}

impl<'de> Deserialize<'de> for AgentManagementRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let object = value.as_object().ok_or_else(|| {
            serde::de::Error::custom("Agent Management request must be an object")
        })?;
        let operation = object
            .get("operation")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| serde::de::Error::custom("operation must be a string"))?;
        let operation_fields: &[&str] = match operation {
            "create" => &["spec", "start_mode", "frozen_message", "bootstrap_intent"],
            "query_managed" => &[],
            "online" | "offline" | "restart" | "close" => &["cutex_session_id"],
            "replace" => &[
                "bootstrap_intent",
                "predecessor_cutex_session_id",
                "policy",
                "successor",
                "start_mode",
                "frozen_message",
            ],
            "grant_operator" | "revoke_operator" => {
                &["operator_cutex_session_id", "expected_grant_revision"]
            }
            "director_rotate" => &[
                "bootstrap_intent",
                "expected_predecessor_cutex_session",
                "expected_authority_epoch",
                "mode",
                "successor",
                "frozen_message",
            ],
            _ => {
                return Err(serde::de::Error::custom(
                    "unknown Agent Management operation",
                ))
            }
        };
        for field in object.keys() {
            if !matches!(
                field.as_str(),
                "schema" | "action_id" | "project_id" | "operation"
            ) && !operation_fields.contains(&field.as_str())
            {
                return Err(serde::de::Error::custom(format!(
                    "unknown Agent Management request field `{field}`"
                )));
            }
        }
        let wire: AgentManagementRequestWire =
            serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(Self {
            schema: wire.schema,
            action_id: wire.action_id,
            project_id: wire.project_id,
            operation: wire.operation,
        })
    }
}

impl AgentManagementRequest {
    /// Checks a new request. r219: `director_rotate` was removed; the owner
    /// assigns the project Director, so such a request is refused with
    /// `tool_removed` before anything is recorded.
    pub fn validate(&self) -> Result<(), AgentManagementError> {
        if matches!(self.operation, AgentOperation::DirectorRotate { .. }) {
            return Err(AgentManagementError::Removed(DIRECTOR_ROTATE_REMOVED));
        }
        self.validate_recorded()
    }

    /// Structural checks for a request an earlier release may have recorded
    /// (for example in a bootstrap intent). A legacy `director_rotate` still
    /// decodes here (r219: decode-only); it never executes.
    pub(crate) fn validate_recorded(&self) -> Result<(), AgentManagementError> {
        if self
            .operation
            .bootstrap_intent()
            .is_some_and(|id| id != &self.action_id)
        {
            return Err(AgentManagementError::InvalidRequest(
                "bootstrap_intent_requires_exact_action",
            ));
        }
        match &self.operation {
            AgentOperation::Create {
                spec,
                start_mode,
                frozen_message,
                ..
            } => {
                spec.validate()?;
                validate_start(*start_mode, frozen_message.as_deref())
            }
            AgentOperation::QueryManaged
            | AgentOperation::Online { .. }
            | AgentOperation::Offline { .. }
            | AgentOperation::Restart { .. }
            | AgentOperation::Close { .. } => Ok(()),
            AgentOperation::Replace {
                successor,
                start_mode,
                frozen_message,
                ..
            } => {
                successor.validate()?;
                validate_start(*start_mode, frozen_message.as_deref())
            }
            AgentOperation::GrantOperator {
                expected_grant_revision,
                ..
            }
            | AgentOperation::RevokeOperator {
                expected_grant_revision,
                ..
            } => {
                if *expected_grant_revision > crate::role_revision::MAX_JSON_SAFE_INTEGER {
                    return Err(AgentManagementError::InvalidRequest(
                        "invalid_operator_grant_revision",
                    ));
                }
                Ok(())
            }
            AgentOperation::DirectorRotate { successor, .. } => successor.validate(),
        }
    }
}

fn validate_start(mode: AgentStartMode, message: Option<&str>) -> Result<(), AgentManagementError> {
    match (
        mode,
        message.map(str::trim).filter(|value| !value.is_empty()),
    ) {
        (AgentStartMode::BootstrapOnly, None) | (AgentStartMode::CustomMessage, Some(_)) => Ok(()),
        (AgentStartMode::BootstrapOnly, Some(_)) => Err(AgentManagementError::InvalidRequest(
            "bootstrap_only_forbids_custom_message",
        )),
        (AgentStartMode::CustomMessage, None) => Err(AgentManagementError::InvalidRequest(
            "custom_message_requires_frozen_message",
        )),
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManagementInvocation {
    pub caller_cutex_session: CutexSessionId,
    pub caller_runtime_agent_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManagementMessageMetadata {
    pub schema: AgentManagementSchema,
    /// Backward-compatible provenance for the project's Primary Director.
    pub requested_by_director: CutexSessionId,
    /// Present when an Agent Operator, rather than the Primary Director,
    /// requested the lifecycle action that emitted this message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_by_operator: Option<CutexSessionId>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectAuthorityRequest {
    pub schema: AgentManagementSchema,
    pub action_id: AgentActionId,
    pub project_id: ProjectId,
    pub authorized_director_session: CutexSessionId,
    #[serde(default)]
    pub expected_authorized_director_session: Option<CutexSessionId>,
    #[serde(default)]
    pub expected_authority_epoch: Option<u64>,
}

/// Root-only one-time migration request for a Director whose project authority
/// predates explicit Agent Management ownership. Runtime/spec facts are loaded
/// from the authoritative durable session store and are intentionally absent
/// from this request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyDirectorOwnershipImportRequest {
    pub schema: LegacyDirectorOwnershipImportSchema,
    pub action_id: AgentActionId,
    pub project_id: ProjectId,
    pub director_cutex_session_id: CutexSessionId,
    pub expected_authorized_director_session: CutexSessionId,
    pub expected_authority_epoch: u64,
}

/// Provider input produced only by the trusted root administration adapter
/// after exact durable-session lookup and lifecycle validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LegacyDirectorOwnershipEvidence {
    pub director_cutex_session_id: CutexSessionId,
    pub native_session_id: String,
    pub durable_session_revision: u64,
    pub runtime_generation: u64,
    pub spec: ManagedAgentSpec,
}

/// Root-only CAS request for terminalizing one legacy, provably pre-effect
/// reservation. None of these selectors are evidence that the successor is
/// absent; the provider obtains that proof independently.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentReservationReconciliationRequest {
    pub schema: AgentReservationReconciliationSchema,
    pub action_id: AgentActionId,
    pub project_id: ProjectId,
    pub target_action_id: AgentActionId,
    pub expected_target_request_sha256: Sha256,
    pub expected_phase: AgentActionPhase,
    pub expected_reserved_agent_name: String,
    pub expected_reserved_agent_cwd: String,
    pub expected_authorized_director_session: CutexSessionId,
    pub expected_authority_epoch: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentReservationReconciliationEvidence {
    ProvenAbsent { reason: String },
    Present { reason: String },
    Ambiguous { reason: String },
    Unavailable { reason: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectAuthority {
    pub project_id: ProjectId,
    pub authorized_director_session: CutexSessionId,
    pub authority_epoch: u64,
    pub updated_at: Rfc3339,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedAgentRecord {
    /// Historical creation provenance, absent for explicit Human durable import.
    /// Current membership is authoritative and is stored separately.
    pub project_id: Option<ProjectId>,
    /// Historical Primary Director provenance; Human import impersonates none.
    pub created_by_director_session: Option<CutexSessionId>,
    /// Present when creation was requested by a delegated Agent Operator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by_operator_session: Option<CutexSessionId>,
    pub cutex_session_id: CutexSessionId,
    pub native_session_id: String,
    pub spec: ManagedAgentSpec,
    pub created_at: Rfc3339,
    #[serde(default)]
    pub retired_at: Option<Rfc3339>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRuntimeObservation {
    pub cutex_session_id: CutexSessionId,
    pub native_session_id: String,
    pub active: bool,
    pub cwd: String,
    pub profile: String,
    pub runtime_backend: String,
    pub model: String,
    pub reasoning: String,
    pub permissions: String,
    pub approval_policy: String,
    pub sandbox_mode: String,
    pub groups: Vec<String>,
    pub runtime_generation: u64,
    /// Display version of the exact launch bundle's runtime artifact, when the
    /// machine-local release registry knows its hash.
    #[serde(default)]
    pub runtime_version: Option<String>,
    #[serde(default)]
    pub runtime_agent_ids: Vec<String>,
    pub app_server_runtime: bool,
    #[serde(default)]
    pub agent_bus_endpoint_ids: Vec<String>,
}

/// Provider-owned identity for one durable runtime occurrence.
///
/// Offline captures this before stopping. A historical scope timeout can
/// reconstruct it only from its exact successful PID receipt and a binding
/// predating the original action. Recovery proves physical absence and clears
/// that exact claim before reopening the action with an all-absent fence.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeOccurrenceFence {
    pub runtime_generation: u64,
    #[serde(default)]
    pub current_runtime_agent_id: Option<String>,
    #[serde(default)]
    pub agent_bus_endpoint_ids: Vec<String>,
    #[serde(default)]
    pub pending_launch_id: Option<String>,
    #[serde(default)]
    pub app_server_launch_claim_id: Option<String>,
    #[serde(default)]
    pub alden_session_name: Option<String>,
    #[serde(default)]
    pub alden_pid: Option<u32>,
    #[serde(default)]
    pub runtime_pid: Option<u32>,
    #[serde(default)]
    pub app_server_pid: Option<u32>,
    #[serde(default)]
    pub app_server_endpoint: Option<String>,
    #[serde(default)]
    pub app_server_connected: bool,
}

impl RuntimeOccurrenceFence {
    pub fn is_proven_absent(&self) -> bool {
        self.current_runtime_agent_id.is_none()
            && self.agent_bus_endpoint_ids.is_empty()
            && self.pending_launch_id.is_none()
            && self.app_server_launch_claim_id.is_none()
            && self.alden_pid.is_none()
            && self.runtime_pid.is_none()
            && self.app_server_pid.is_none()
            && self.app_server_endpoint.is_none()
            && !self.app_server_connected
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentActionPhase {
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
    /// r219: legacy (Director rotation), decode-only; never produced.
    AuthorityTransferPending,
    /// r219: legacy (Director rotation), decode-only; never produced.
    AuthorityTransferred,
    SuccessorReady,
    Complete,
    NoWrite,
    OwnerActionRequired,
    Failure,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentActionRecord {
    pub action_id: AgentActionId,
    pub request_sha256: Sha256,
    pub operation: AgentOperationKind,
    pub project_id: ProjectId,
    pub caller_cutex_session: CutexSessionId,
    /// Frozen when this new action is authorized. None on legacy actions stays
    /// unknown on recovery; never recompute the creator's role after handover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<crate::session::model::SessionCreatedBy>,
    pub phase: AgentActionPhase,
    #[serde(default)]
    pub phase_sequence: u64,
    #[serde(default)]
    pub reserved_agent_name: Option<String>,
    #[serde(default)]
    pub reserved_agent_cwd: Option<String>,
    #[serde(default)]
    pub known_successor_cutex_session: Option<CutexSessionId>,
    #[serde(default)]
    pub known_native_session_id: Option<String>,
    /// The provider proved that no native session/runtime was created, either
    /// at a pre-spawn boundary or by authoritative historical reconciliation.
    /// It consumes this bit durably before permitting one launch attempt.
    #[serde(default)]
    pub native_bootstrap_retryable: bool,
    /// Offline's original occurrence, or the all-absent fence committed before
    /// reopening a reconciled lifecycle action. Missing legacy evidence never
    /// authorizes an effect by itself.
    #[serde(default)]
    pub historical_runtime_occurrence_fence: Option<RuntimeOccurrenceFence>,
    #[serde(default)]
    pub external_message_id: Option<String>,
    #[serde(default)]
    pub response: Option<AgentManagementResponse>,
    pub created_at: Rfc3339,
    pub updated_at: Rfc3339,
}

/// One exact, durably committed phase of an Agent Management action. This is
/// provider state, not presentation state: consumers may enrich it, but must
/// never infer phases that do not occur here.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManagementPhaseEvent {
    pub event_id: String,
    pub action_id: AgentActionId,
    pub project_id: ProjectId,
    pub operation: AgentOperationKind,
    pub phase: AgentActionPhase,
    pub phase_sequence: u64,
    pub committed_at: Rfc3339,
    pub presentation_owner_cutex_session_id: CutexSessionId,
    /// The lifecycle subject is distinct from the Director/TUI presentation
    /// owner. Before a create-like operation has captured a durable session,
    /// the frozen requested name remains the authoritative display identity.
    #[serde(default)]
    pub subject_cutex_session_id: Option<CutexSessionId>,
    #[serde(default)]
    pub subject_agent_name: Option<String>,
    #[serde(default)]
    pub predecessor_cutex_session_id: Option<CutexSessionId>,
    #[serde(default)]
    pub successor_cutex_session_id: Option<CutexSessionId>,
    #[serde(default)]
    pub replace_policy: Option<AgentReplacePolicy>,
    /// r219: legacy (Director rotation), decode-only; always `None` now.
    #[serde(default)]
    pub rotation_mode: Option<DirectorRotateMode>,
    #[serde(default)]
    pub authority_epoch: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentManagementResult {
    Created {
        agent: ManagedAgentRecord,
        observation: AgentRuntimeObservation,
        #[serde(default)]
        message_id: Option<String>,
    },
    QueryManaged {
        authority: ProjectAuthority,
        agents: Vec<ManagedAgentRecord>,
        #[serde(default)]
        operators: AgentOperatorRosterProjection,
    },
    Lifecycle {
        agent: ManagedAgentRecord,
        observation: AgentRuntimeObservation,
    },
    Replaced {
        predecessor_cutex_session_id: CutexSessionId,
        successor: ManagedAgentRecord,
        observation: AgentRuntimeObservation,
        #[serde(default)]
        message_id: Option<String>,
    },
    OperatorGranted {
        grant: AgentOperatorGrant,
        roster: AgentOperatorRosterProjection,
    },
    OperatorRevoked {
        operator_cutex_session_id: CutexSessionId,
        roster: AgentOperatorRosterProjection,
    },
    /// r219: legacy, decode-only; never produced.
    DirectorRotated {
        predecessor_cutex_session_id: CutexSessionId,
        successor: ManagedAgentRecord,
        observation: AgentRuntimeObservation,
        authority: ProjectAuthority,
        #[serde(default)]
        message_id: Option<String>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManagementReceipt {
    pub schema: AgentManagementReceiptSchema,
    pub action_id: AgentActionId,
    pub request_sha256: Sha256,
    pub operation: AgentOperationKind,
    pub project_id: ProjectId,
    pub completed_at: Rfc3339,
    pub result: AgentManagementResult,
    /// Archive disposition of the lifecycle target (Replace: predecessor).
    #[serde(default, rename = "archiveKind", skip_serializing_if = "Option::is_none")]
    pub archive_kind: Option<super::AgentArchiveKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restorable: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureRoutingStatus {
    Routable,
    Unrouted,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManagementFailureEvent {
    pub schema: AgentManagementFailureSchema,
    pub event_id: String,
    pub action_id: AgentActionId,
    pub project_id: ProjectId,
    pub operation: AgentOperationKind,
    pub code: String,
    pub detail: String,
    pub routing_status: FailureRoutingStatus,
    #[serde(default)]
    pub route_to_director_session: Option<CutexSessionId>,
    /// Exact durable target from an authenticated lifecycle request, when the
    /// request already contained one. Pre-identity operations intentionally
    /// leave this absent rather than fabricating a timeline identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_cutex_session_id: Option<CutexSessionId>,
    pub created_at: Rfc3339,
}

/// Append-only audit evidence for one successful root reconciliation. The
/// original failure event remains in the journal.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentReservationReconciliationEvent {
    pub event_id: String,
    pub action_id: AgentActionId,
    pub request_sha256: Sha256,
    pub project_id: ProjectId,
    pub target_action_id: AgentActionId,
    pub target_request_sha256: Sha256,
    pub reserved_agent_name: String,
    pub reserved_agent_cwd: String,
    pub previous_phase: AgentActionPhase,
    pub terminal_phase: AgentActionPhase,
    pub absence_evidence: String,
    pub committed_at: Rfc3339,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentManagementOutcome {
    Complete {
        receipt: AgentManagementReceipt,
    },
    NoWrite {
        code: String,
        detail: String,
    },
    OwnerActionRequired {
        failure: AgentManagementFailureEvent,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManagementResponse {
    pub schema: AgentManagementSchema,
    pub action_id: AgentActionId,
    pub outcome: AgentManagementOutcome,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectAuthorityReceipt {
    pub schema: AgentManagementReceiptSchema,
    pub action_id: AgentActionId,
    pub request_sha256: Sha256,
    pub authority: ProjectAuthority,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProjectAuthorityOutcome {
    Complete { receipt: ProjectAuthorityReceipt },
    OwnerActionRequired { code: String, detail: String },
    NoWrite { code: String, detail: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectAuthorityResponse {
    pub schema: AgentManagementSchema,
    pub action_id: AgentActionId,
    pub outcome: ProjectAuthorityOutcome,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyDirectorOwnershipImportReceipt {
    pub schema: LegacyDirectorOwnershipImportReceiptSchema,
    pub action_id: AgentActionId,
    pub request_sha256: Sha256,
    pub authority: ProjectAuthority,
    pub agent: ManagedAgentRecord,
    pub durable_session_revision: u64,
    pub runtime_generation: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum LegacyDirectorOwnershipImportOutcome {
    Complete {
        receipt: LegacyDirectorOwnershipImportReceipt,
        replayed: bool,
    },
    NoWrite {
        code: String,
        detail: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyDirectorOwnershipImportResponse {
    pub schema: LegacyDirectorOwnershipImportSchema,
    pub action_id: AgentActionId,
    pub outcome: LegacyDirectorOwnershipImportOutcome,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentReservationReconciliationReceipt {
    pub schema: AgentReservationReconciliationReceiptSchema,
    pub action_id: AgentActionId,
    pub request_sha256: Sha256,
    pub project_id: ProjectId,
    pub target_action_id: AgentActionId,
    pub target_request_sha256: Sha256,
    pub reserved_agent_name: String,
    pub reserved_agent_cwd: String,
    pub reconciled_at: Rfc3339,
    pub reconciliation_event_id: String,
    pub target_response: AgentManagementResponse,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentReservationReconciliationOutcome {
    Complete {
        receipt: AgentReservationReconciliationReceipt,
        replayed: bool,
    },
    NoWrite {
        code: String,
        detail: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentReservationReconciliationResponse {
    pub schema: AgentReservationReconciliationSchema,
    pub action_id: AgentActionId,
    pub outcome: AgentReservationReconciliationOutcome,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentManagementError {
    InvalidRequest(&'static str),
    /// A deterministic, pre-effect request rejection with an actionable detail.
    InvalidRequestDetail(String),
    Unauthorized,
    NotAuthorizedDirector,
    ProjectSelectionRequired,
    ProjectNotAuthorized,
    NotFound(&'static str),
    Conflict(&'static str),
    /// A conflict with an actionable detail. The detail starts with a stable
    /// reason token (for example `unresolved_agent_reservation`).
    ConflictDetail(String),
    OwnerActionRequired(String),
    PersistenceUnavailable,
    InvalidStore,
    External(String),
    ReconciliationPresent(String),
    ReconciliationAmbiguous(String),
    ReconciliationUnavailable(String),
    /// The operation or tool was removed (wire code `tool_removed`).
    Removed(&'static str),
}

/// r219: refusal detail for a new `director_rotate` request (wire code
/// `tool_removed`).
pub const DIRECTOR_ROTATE_REMOVED: &str =
    "director_rotate was removed in r219: the owner assigns the project Director.";

/// r219: failure code of a `director_rotate` action an older release left
/// unfinished; recovery finishes it without any further effect.
pub const DIRECTOR_ROTATE_REMOVED_CODE: &str = "director_rotate_removed";

/// r219: refusal detail for a `repair_director_seat` project mutation (wire
/// code `tool_removed`). The Task Service Director seat copy was removed; the
/// project Director is the Agent Management authority only.
pub const DIRECTOR_SEAT_REPAIR_REMOVED: &str = "repair_director_seat was removed in r219: \
the project Director is the Agent Management authority only.";

impl AgentManagementError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) | Self::InvalidRequestDetail(_) => "invalid_request",
            Self::Unauthorized => "unauthorized",
            Self::NotAuthorizedDirector => "not_authorized_director",
            Self::ProjectSelectionRequired => "project_selection_required",
            Self::ProjectNotAuthorized => "project_not_authorized",
            Self::NotFound(_) => "not_found",
            Self::Conflict(_) | Self::ConflictDetail(_) => "conflict",
            Self::OwnerActionRequired(_) => "owner_action_required",
            Self::PersistenceUnavailable => "persistence_unavailable",
            Self::InvalidStore => "invalid_store",
            Self::External(_) => "external_failure",
            Self::ReconciliationPresent(_) => "reservation_evidence_present",
            Self::ReconciliationAmbiguous(_) => "reservation_evidence_ambiguous",
            Self::ReconciliationUnavailable(_) => "reservation_evidence_unavailable",
            Self::Removed(_) => "tool_removed",
        }
    }

    /// `(code, detail)` of a typed NoWrite answer. Clients render
    /// `code: detail`; `Display` already prefixes the class code, so using it
    /// as the detail read `conflict: conflict: active_agent_collision`.
    ///
    /// A specific reason token (`stale_project_authority`, or the leading
    /// `token: ` of a detailed conflict / invalid request) becomes the code
    /// and the detail is the rest (or the class, e.g. `conflict`, when the
    /// reason has no text). Other errors keep the class code
    /// ([`Self::code`], also used for HTTP statuses) and an unprefixed detail.
    pub fn wire(&self) -> (String, String) {
        let class = self.code();
        match self {
            Self::InvalidRequest(reason) | Self::NotFound(reason) | Self::Conflict(reason)
                if is_reason_token(reason) =>
            {
                ((*reason).to_string(), class.to_string())
            }
            Self::InvalidRequestDetail(reason) | Self::ConflictDetail(reason) => {
                match reason.split_once(": ") {
                    Some((token, rest)) if is_reason_token(token) && !rest.trim().is_empty() => {
                        (token.to_string(), rest.to_string())
                    }
                    _ => (class.to_string(), reason.clone()),
                }
            }
            Self::InvalidRequest(reason)
            | Self::NotFound(reason)
            | Self::Conflict(reason)
            | Self::Removed(reason) => (class.to_string(), (*reason).to_string()),
            Self::OwnerActionRequired(reason)
            | Self::External(reason)
            | Self::ReconciliationPresent(reason)
            | Self::ReconciliationAmbiguous(reason)
            | Self::ReconciliationUnavailable(reason) => (class.to_string(), reason.clone()),
            _ => (class.to_string(), class.to_string()),
        }
    }
}

/// A short machine reason such as `invalid_agent_groups`.
fn is_reason_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

impl fmt::Display for AgentManagementError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(reason)
            | Self::NotFound(reason)
            | Self::Conflict(reason)
            | Self::Removed(reason) => {
                write!(formatter, "{}: {reason}", self.code())
            }
            Self::InvalidRequestDetail(reason)
            | Self::ConflictDetail(reason)
            | Self::OwnerActionRequired(reason)
            | Self::External(reason)
            | Self::ReconciliationPresent(reason)
            | Self::ReconciliationAmbiguous(reason)
            | Self::ReconciliationUnavailable(reason) => {
                write!(formatter, "{}: {reason}", self.code())
            }
            _ => formatter.write_str(self.code()),
        }
    }
}

impl std::error::Error for AgentManagementError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// r219: recorded `director_rotate` history still decodes and passes the
    /// structural check used on load; a new request is refused.
    #[test]
    fn legacy_director_rotate_decodes_but_new_requests_are_refused() {
        let request: AgentManagementRequest = serde_json::from_value(serde_json::json!({
            "schema": "cutex/agent-management/v1",
            "action_id": "legacy-rotate-1",
            "project_id": "project-alpha",
            "operation": "director_rotate",
            "expected_predecessor_cutex_session": "cutex.director",
            "expected_authority_epoch": 3,
            "mode": "close_predecessor_then_create_with_message",
            "successor": {"name":"director-r2","cwd":std::env::temp_dir().join("director-r2"),
                "profile":"p","runtime_backend":"host","model":"m","reasoning":"low",
                "permissions":"read-only","approval_policy":"on-request","sandbox_mode":"read-only",
                "groups":["cutex","agent:director-r2"]},
            "frozen_message": "handoff"
        }))
        .unwrap();
        assert_eq!(request.operation.kind(), AgentOperationKind::DirectorRotate);
        request.validate_recorded().unwrap();
        let refused = request.validate().unwrap_err();
        assert_eq!(refused, AgentManagementError::Removed(DIRECTOR_ROTATE_REMOVED));
        assert_eq!(refused.code(), "tool_removed");
        for (value, kind) in [
            ("retained_after_director_rotation", AgentOperatorAuditKind::RetainedAfterDirectorRotation),
            ("removed_by_director_rotation", AgentOperatorAuditKind::RemovedByDirectorRotation),
        ] {
            assert_eq!(serde_json::from_value::<AgentOperatorAuditKind>(serde_json::json!(value)).unwrap(), kind);
        }
        for value in ["authority_transfer_pending", "authority_transferred"] {
            assert!(serde_json::from_value::<AgentActionPhase>(serde_json::json!(value)).is_ok());
        }
        for value in [
            "close_predecessor_then_create_with_message",
            "retain_predecessor_with_message",
            "retain_predecessor_bootstrap_only",
        ] {
            assert!(serde_json::from_value::<DirectorRotateMode>(serde_json::json!(value)).is_ok());
        }
    }

    #[test]
    fn strict_action_body_allows_only_an_optional_non_authoritative_project_selector() {
        let mut value = serde_json::json!({
            "schema": "cutex/agent-management/v1",
            "action_id": "query-1",
            "operation": "query_managed"
        });
        assert!(serde_json::from_value::<AgentManagementRequest>(value.clone()).is_ok());
        value["project_id"] = serde_json::json!("cutex-project");
        assert!(serde_json::from_value::<AgentManagementRequest>(value.clone()).is_ok());
        for forbidden in [
            "caller_cutex_session",
            "caller_runtime_agent_id",
            "authorized_director_session",
            "authority_epoch",
            "seat",
        ] {
            value[forbidden] = serde_json::json!("forged");
            assert!(serde_json::from_value::<AgentManagementRequest>(value.clone()).is_err());
            value.as_object_mut().unwrap().remove(forbidden);
        }
    }

    #[test]
    fn operator_grant_and_revoke_requests_are_strict_typed_cas_operations() {
        for operation in ["grant_operator", "revoke_operator"] {
            let value = serde_json::json!({
                "schema": "cutex/agent-management/v1",
                "action_id": format!("{operation}-1"),
                "project_id": "project-alpha",
                "operation": operation,
                "operator_cutex_session_id": "cutex.worker",
                "expected_grant_revision": 0
            });
            let request: AgentManagementRequest = serde_json::from_value(value.clone()).unwrap();
            request.validate().unwrap();
            let mut forged = value;
            forged["operator_name"] = serde_json::json!("worker");
            assert!(serde_json::from_value::<AgentManagementRequest>(forged).is_err());
        }
    }

    #[test]
    fn project_selector_is_strict_when_present() {
        assert!(ProjectId::new("project:alpha-1").is_ok());
        for invalid in ["", "project alpha", "../alpha", "/absolute"] {
            assert!(ProjectId::new(invalid).is_err());
            assert!(serde_json::from_value::<ProjectId>(serde_json::json!(invalid)).is_err());
        }
    }

    #[test]
    fn reservation_reconciliation_request_is_closed_and_uses_only_cas_selectors() {
        let value = serde_json::json!({
            "schema": "cutex/agent-management/reservation-reconciliation/v1",
            "action_id": "root-reconcile-1",
            "project_id": "example-project",
            "target_action_id": "legacy-rotation-1",
            "expected_target_request_sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "expected_phase": "owner_action_required",
            "expected_reserved_agent_name": "example-director-v2",
            "expected_reserved_agent_cwd": "/tmp/example-director-v2",
            "expected_authorized_director_session": "cutex.director-r4",
            "expected_authority_epoch": 1
        });
        assert!(
            serde_json::from_value::<AgentReservationReconciliationRequest>(value.clone()).is_ok()
        );
        for forbidden in [
            "successor_absent",
            "native_session_absent",
            "agent_bus_endpoint_absent",
            "force",
            "caller_cutex_session",
        ] {
            let mut changed = value.clone();
            changed[forbidden] = serde_json::json!(true);
            assert!(
                serde_json::from_value::<AgentReservationReconciliationRequest>(changed).is_err(),
                "{forbidden} must not become caller-supplied absence evidence"
            );
        }
    }

    #[test]
    fn custom_message_metadata_preserves_typed_director_provenance() {
        let metadata = AgentManagementMessageMetadata {
            schema: AgentManagementSchema::V1,
            requested_by_director: CutexSessionId::new("cutex.director-r11").unwrap(),
            requested_by_operator: None,
        };
        let encoded = serde_json::to_value(&metadata).unwrap();
        assert_eq!(encoded["schema"], "cutex/agent-management/v1");
        assert_eq!(encoded["requested_by_director"], "cutex.director-r11");
        assert_eq!(
            serde_json::from_value::<AgentManagementMessageMetadata>(encoded).unwrap(),
            metadata
        );
        assert!(
            serde_json::from_value::<AgentManagementMessageMetadata>(serde_json::json!({
                "schema": "cutex/agent-management/v1",
                "requested_by_director": "cutex.director-r11",
                "authority": "system"
            }))
            .is_err()
        );
    }

    #[test]
    fn legacy_director_ownership_import_request_is_closed_and_contains_only_cas_identity() {
        let value = serde_json::json!({
            "schema": "cutex/agent-management/legacy-director-ownership-import/v1",
            "action_id": "import-legacy-director-01",
            "project_id": "cutex-project",
            "director_cutex_session_id": "cutex.director-r2",
            "expected_authorized_director_session": "cutex.director-r2",
            "expected_authority_epoch": 7
        });
        let request = serde_json::from_value::<LegacyDirectorOwnershipImportRequest>(value.clone())
            .expect("strict import request");
        assert_eq!(request.expected_authority_epoch, 7);
        for forbidden in [
            "spec",
            "native_session_id",
            "runtime_agent_id",
            "groups",
            "cwd",
            "prose",
        ] {
            let mut changed = value.clone();
            changed[forbidden] = serde_json::json!("forged");
            assert!(
                serde_json::from_value::<LegacyDirectorOwnershipImportRequest>(changed).is_err(),
                "accepted forbidden field {forbidden}"
            );
        }
    }
}
