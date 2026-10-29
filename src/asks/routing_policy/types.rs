use super::validate_id;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Full immutable canonical reference. Neither names nor legacy bare IDs are
/// accepted by the new HTTP types. Host existence/access is a caller check.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", try_from = "RawRef")]
pub struct RoutingRef {
    host_id: String,
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawRef {
    host_id: String,
    id: String,
}

impl TryFrom<RawRef> for RoutingRef {
    type Error = RoutingError;
    fn try_from(value: RawRef) -> RoutingResult<Self> {
        Self::new(value.host_id, value.id)
    }
}
impl RoutingRef {
    pub fn new(host_id: impl Into<String>, id: impl Into<String>) -> RoutingResult<Self> {
        let host_id = host_id.into();
        let id = id.into();
        validate_id(&host_id)?;
        validate_id(&id)?;
        Ok(Self { host_id, id })
    }
    pub fn host_id(&self) -> &str {
        &self.host_id
    }
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// Authentication facts, never a wire DTO. Construct only after checking the
/// authenticated credential and project management/membership scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoutingActor {
    Owner,
    Agent {
        reference: RoutingRef,
        name: Option<String>,
    },
}
impl RoutingActor {
    pub fn reference(&self) -> Option<&RoutingRef> {
        match self {
            Self::Owner => None,
            Self::Agent { reference, .. } => Some(reference),
        }
    }
    pub fn is_owner(&self) -> bool {
        matches!(self, Self::Owner)
    }
    pub fn audit(&self) -> RoutingActorAudit {
        match self {
            Self::Owner => RoutingActorAudit {
                kind: ActorKind::Owner,
                host_id: None,
                session_id: None,
                name: None,
                redacted: false,
            },
            Self::Agent { reference, name } => RoutingActorAudit {
                kind: ActorKind::Agent,
                host_id: Some(reference.host_id.clone()),
                session_id: Some(reference.id.clone()),
                name: name.clone(),
                redacted: false,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutingContext {
    pub project_id: String,
    pub authority_epoch: u64,
    pub current_director: Option<RoutingRef>,
    pub actor: RoutingActor,
    /// Existing verified operator grant may permit routing, never decisions
    /// or issuing grants. Do not derive this from a display role/name.
    pub operator_may_route: bool,
}
impl RoutingContext {
    pub fn validate(&self) -> RoutingResult<()> {
        validate_id(&self.project_id)?;
        // Existing effective_authority_epoch uses 0 for a project which
        // never had a Director. A nonempty seat can never have epoch 0.
        if (self.authority_epoch == 0 && self.current_director.is_some())
            || self.authority_epoch > 9_007_199_254_740_991
        {
            return Err(RoutingError::InvalidAuthorityEpoch);
        }
        Ok(())
    }
    pub fn management_authority(&self) -> RoutingResult<DecisionAuthority> {
        self.validate()?;
        if self.actor.is_owner() {
            return Ok(DecisionAuthority::Owner);
        }
        if self.actor.reference().is_some()
            && self.actor.reference() == self.current_director.as_ref()
        {
            return Ok(DecisionAuthority::Director);
        }
        Err(RoutingError::NotAuthorized)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ActorKind {
    Owner,
    Agent,
}

/// Exactly five wire fields, including nulls. Read projections may redact a
/// historical agent without erasing its persisted decision authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RoutingActorAudit {
    pub kind: ActorKind,
    pub host_id: Option<String>,
    pub session_id: Option<String>,
    pub name: Option<String>,
    pub redacted: bool,
}
impl RoutingActorAudit {
    pub fn redact(&self) -> Self {
        if self.kind == ActorKind::Owner {
            return self.clone();
        }
        Self {
            kind: ActorKind::Agent,
            host_id: None,
            session_id: None,
            name: None,
            redacted: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MemberMode {
    #[default]
    ApprovalRequired,
    AllowlistedOnly,
    Open,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectDelegationPolicy {
    pub project_id: String,
    pub revision: u64,
    pub member_mode: MemberMode,
    pub updated_by: Option<RoutingActorAudit>,
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdatePolicyRequest {
    pub action_id: String,
    pub expected_revision: u64,
    pub member_mode: MemberMode,
}

/// Internal snapshot built from actual persisted ask/attempt facts. No client
/// may choose parent/root/source or hash a different brief than it delivers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RoutingRequest {
    pub project_id: String,
    pub delegation_ref: RoutingRef,
    pub requester_ref: Option<RoutingRef>,
    pub target_ref: RoutingRef,
    pub parent: Option<RoutingRef>,
    pub root: Option<RoutingRef>,
    pub source_attempt_id: Option<String>,
    pub source_turn_id: Option<String>,
    pub brief: String,
    /// Canonical attachment identities + immutable content digests/versions,
    /// not merely filenames or mutable artifact display labels.
    pub attachments: Vec<Value>,
}
impl RoutingRequest {
    pub fn validate(&self) -> RoutingResult<()> {
        validate_id(&self.project_id)?;
        for id in [&self.source_attempt_id, &self.source_turn_id]
            .into_iter()
            .flatten()
        {
            validate_id(id)?;
        }
        Ok(())
    }
    pub fn digest(&self) -> RoutingResult<String> {
        self.validate()?;
        let value = serde_json::json!({"domain":"delegation-routing-v1", "request":self});
        Ok(format!(
            "sha256:{}",
            crate::approvals::request_digest(&value)
        ))
    }
}

/// Verified lineage and current root state, not request parameters. Unknown
/// roots cannot exercise a grant; fresh manual approval remains possible.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RootStatus {
    VerifiedOpen(RoutingRef),
    Closed(RoutingRef),
    Unknown,
}
impl RootStatus {
    pub(super) fn check_request(&self, request: &RoutingRequest) -> RoutingResult<()> {
        match self {
            Self::Closed(_) => Err(RoutingError::RootClosed),
            Self::VerifiedOpen(root) if request.root.as_ref() != Some(root) => {
                Err(RoutingError::ScopeMismatch)
            }
            _ => Ok(()),
        }
    }
    pub(super) fn require_open(&self, root: &RoutingRef) -> RoutingResult<()> {
        match self {
            Self::VerifiedOpen(actual) if actual == root => Ok(()),
            Self::Closed(_) => Err(RoutingError::RootClosed),
            _ => Err(RoutingError::RootUnverified),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GrantPurpose {
    Review,
}
fn one() -> u32 {
    1
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateGrantRequest {
    pub action_id: String,
    pub requester_ref: RoutingRef,
    pub target_refs: Vec<RoutingRef>,
    pub root_delegation_ref: RoutingRef,
    pub purpose: GrantPurpose,
    #[serde(default = "one")]
    pub max_uses: u32,
    pub expires_at: Option<DateTime<Utc>>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RevokeGrantRequest {
    pub action_id: String,
    pub expected_revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegationGrant {
    pub id: String,
    pub project_id: String,
    pub requester_ref: RoutingRef,
    pub target_refs: Vec<RoutingRef>,
    pub root_delegation_ref: RoutingRef,
    pub purpose: GrantPurpose,
    pub max_uses: u32,
    pub used_count: u32,
    pub expires_at: Option<DateTime<Utc>>,
    pub authority_epoch: u64,
    pub policy_revision: u64,
    pub revision: u64,
    pub created_by: RoutingActorAudit,
    pub created_at: DateTime<Utc>,
    pub revoked_by: Option<RoutingActorAudit>,
    pub revoked_at: Option<DateTime<Utc>>,
    /// Terminal invalidation: explicitly reopening the old root must not
    /// restore grants issued during its previous lifetime.
    #[serde(default)]
    pub root_closed_at: Option<DateTime<Utc>>,
    pub consumption: Option<GrantConsumption>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrantConsumption {
    pub grant_id: String,
    pub action_id: String,
    pub requester_ref: RoutingRef,
    pub delegation_ref: RoutingRef,
    pub request_digest: String,
    pub consumed_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalState {
    Pending,
    Approved,
    Denied,
    Stale,
    Cancelled,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DecisionAuthority {
    Owner,
    Director,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RoutingDecision {
    Approve,
    Deny,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DecideRoutingRequest {
    pub action_id: String,
    pub decision: RoutingDecision,
    pub expected_digest: String,
    pub expected_revision: u64,
    pub decision_note: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutingApproval {
    pub id: String,
    pub project_id: String,
    pub delegation_ref: RoutingRef,
    pub request_digest: String,
    pub policy_revision: u64,
    pub authority_epoch: u64,
    pub revision: u64,
    pub state: ApprovalState,
    pub requested_by: RoutingActorAudit,
    pub decided_by: Option<RoutingActorAudit>,
    pub decision_authority: Option<DecisionAuthority>,
    pub decision_note: Option<String>,
    pub created_at: DateTime<Utc>,
    pub decided_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteAuthorization<'a> {
    None,
    Grant(&'a str),
    Approval(&'a str),
    ApprovalAndGrant {
        approval_id: &'a str,
        grant_id: &'a str,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouteBasis {
    Owner,
    Director,
    Operator,
    ToDirector,
    Open,
    Grant {
        grant_id: String,
    },
    Approval {
        approval_id: String,
    },
    ApprovedGrant {
        approval_id: String,
        grant_id: String,
    },
}

/// Captured under the authority/ask lock; actual body/target are rehashed at
/// admission. No later turn can supply replacement ancestry/origin facts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutingBinding {
    pub request: RoutingRequest,
    pub approval_id: Option<String>,
    pub grant_id: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouteVerdict {
    Pass(RouteBasis),
    ApprovalRequired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ApprovalActions {
    pub approve: bool,
    pub deny: bool,
}

pub type RoutingResult<T> = Result<T, RoutingError>;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoutingError {
    InvalidIdentifier,
    InvalidAuthorityEpoch,
    NotAuthorized,
    ScopeMismatch,
    PolicyRevisionConflict,
    GrantRevisionConflict,
    RevisionOverflow,
    InvalidGrant,
    GrantNotFound,
    GrantRevoked,
    GrantExpired,
    GrantExhausted,
    GrantStale,
    RootClosed,
    RootUnverified,
    TargetNotGranted,
    ActionPayloadConflict,
    ApprovalNotFound,
    ApprovalStale,
    ApprovalDenied,
    ApprovalAlreadyDecided,
    ApprovalRevisionConflict,
    DigestMismatch,
    InvalidDecisionNote,
    ResourceExists,
    ReviewerGrantRequired,
}
impl RoutingError {
    pub fn code(self) -> &'static str {
        match self {
            Self::InvalidIdentifier => "invalid_routing_identifier",
            Self::InvalidAuthorityEpoch => "invalid_authority_epoch",
            Self::NotAuthorized => "routing_not_authorized",
            Self::ScopeMismatch => "routing_scope_mismatch",
            Self::PolicyRevisionConflict => "policy_revision_conflict",
            Self::GrantRevisionConflict => "grant_revision_conflict",
            Self::RevisionOverflow => "routing_revision_overflow",
            Self::InvalidGrant => "invalid_delegation_grant",
            Self::GrantNotFound => "delegation_grant_not_found",
            Self::GrantRevoked => "delegation_grant_revoked",
            Self::GrantExpired => "delegation_grant_expired",
            Self::GrantExhausted => "delegation_grant_exhausted",
            Self::GrantStale => "delegation_grant_stale",
            Self::RootClosed => "delegation_root_closed",
            Self::RootUnverified => "delegation_root_unverified",
            Self::TargetNotGranted => "delegation_target_not_granted",
            Self::ActionPayloadConflict => "action_id_payload_conflict",
            Self::ApprovalNotFound => "routing_approval_not_found",
            Self::ApprovalStale => "delegation_approval_stale",
            Self::ApprovalDenied => "delegation_routing_denied",
            Self::ApprovalAlreadyDecided => "approval_already_decided",
            Self::ApprovalRevisionConflict => "approval_revision_conflict",
            Self::DigestMismatch => "approval_digest_mismatch",
            Self::InvalidDecisionNote => "invalid_decision_note",
            Self::ResourceExists => "routing_resource_exists",
            Self::ReviewerGrantRequired => "delegation_reviewer_grant_required",
        }
    }
}
impl std::fmt::Display for RoutingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}
impl std::error::Error for RoutingError {}
