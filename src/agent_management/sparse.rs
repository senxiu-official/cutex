//! Sparse `agent_create` / `member_replace` arguments (agent-create usability
//! P2, docs/research/agent-create-usability-20261011.md §4 in cutex-desktop).
//!
//! An agent names the new agent (and optionally its first task); everything
//! else defaults from the caller and the project. A task is delivered as a
//! delegation from the caller (`task_mode` [`TaskMode::Delegation`], the
//! default since `agent-create-task-delegation`): the agent starts idle
//! (bootstrap only) and the Agent Bus then delegates the task to it, so its
//! result returns to the caller like any `agent_delegate` result
//! ([`delegated_task`]). `task_mode: "message"` keeps the plain start
//! message. Defaults:
//! - profile: the caller's profile; model: that profile's new-session default;
//!   reasoning: `default` (the profile's effort, resolved at create);
//! - sandbox / approval policy: the caller's own envelope (Claude profiles:
//!   full access / never, the only Claude launch shape);
//! - runtime backend from the profile, permission alias from the sandbox;
//! - cwd: the project's `workspace_root`, else the caller's cwd;
//! - groups: the caller's Agent Bus groups.
//!
//! The Agent Bus resolves a sparse request into the full [`AgentManagementRequest`]
//! before the launch policy (`agent_launch_policy_refusal`) and the provider,
//! which therefore stay unchanged. The resolution is stored per action
//! ([`SparseRequestRecord`]) and bound to the digest of the sparse request as
//! given, so an exact retry replays the same full request even after a
//! default changed; the reply's `request_sha256` is the sparse digest.

use serde::{Deserialize, Serialize};

use super::envelope::{ApprovalStrictness, PermissionEnvelope, SandboxLevel};
use super::{
    AgentActionId, AgentManagementError, AgentManagementRequest, AgentManagementSchema,
    AgentOperation, AgentReplacePolicy, AgentStartMode, ManagedAgentSpec, ProjectId,
};
use crate::role_revision::{CutexSessionId, Rfc3339, Sha256};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SparseSchema {
    #[serde(rename = "cutex/agent-management-sparse/v1")]
    V1,
}

pub const SPARSE_SCHEMA: &str = "cutex/agent-management-sparse/v1";

/// The agent a sparse create / replace asks for; every field but `name` is
/// optional and defaults as described in the module docs.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SparseAgentSpec {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Retired (`im-exposure-retired`): still accepted from older callers
    /// and kept in the request digest for exact replays, but ignored; every
    /// managed agent is listed for the owner.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub expose_to_im: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pin: bool,
}

/// How a sparse create / replace hands over its `task`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskMode {
    /// A delegation from the caller to the new agent (its result returns to
    /// the caller). The default; absent from the wire (and the digest).
    #[default]
    Delegation,
    /// The plain start message ("first message") of before
    /// `agent-create-task-delegation`: no tracked result.
    Message,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum SparseOperation {
    Create {
        agent: SparseAgentSpec,
        /// Present: the brief (delegated, or the first message with
        /// `task_mode: message`); absent: the agent starts idle.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_mode: Option<TaskMode>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bootstrap_intent: Option<AgentActionId>,
    },
    Replace {
        predecessor_cutex_session_id: CutexSessionId,
        policy: AgentReplacePolicy,
        successor: SparseAgentSpec,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_mode: Option<TaskMode>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bootstrap_intent: Option<AgentActionId>,
    },
}

impl SparseOperation {
    pub fn agent(&self) -> &SparseAgentSpec {
        match self {
            Self::Create { agent, .. } => agent,
            Self::Replace { successor, .. } => successor,
        }
    }

    fn bootstrap_intent(&self) -> Option<&AgentActionId> {
        match self {
            Self::Create {
                bootstrap_intent, ..
            }
            | Self::Replace {
                bootstrap_intent, ..
            } => bootstrap_intent.as_ref(),
        }
    }

    fn task(&self) -> (&Option<String>, TaskMode) {
        match self {
            Self::Create { task, task_mode, .. } | Self::Replace { task, task_mode, .. } => {
                (task, task_mode.unwrap_or_default())
            }
        }
    }
}

/// The task to delegate from the caller to the agent a sparse request
/// creates (`agent-create-task-delegation`): a non-empty task in
/// [`TaskMode::Delegation`] whose resolved request starts the agent without
/// a start message. `None` for `task_mode: message`, for a Human-reviewed
/// intent (its reviewed start message is kept) and for a resolution stored
/// before this release (an exact retry replays its start message).
pub fn delegated_task<'a>(
    sparse: &'a SparseAgentManagementRequest,
    resolved: &AgentManagementRequest,
) -> Option<&'a str> {
    let (task, mode) = sparse.operation.task();
    let task = task.as_deref().filter(|task| !task.trim().is_empty())?;
    let bootstrap_only = match &resolved.operation {
        AgentOperation::Create { start_mode, .. } | AgentOperation::Replace { start_mode, .. } => {
            *start_mode == AgentStartMode::BootstrapOnly
        }
        _ => false,
    };
    (mode == TaskMode::Delegation && sparse.operation.bootstrap_intent().is_none() && bootstrap_only)
        .then_some(task)
}

/// Wire request of the sparse Agent Management contract. The digest of its
/// canonical serialization is the request digest the caller sees.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SparseAgentManagementRequest {
    pub schema: SparseSchema,
    pub action_id: AgentActionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    #[serde(flatten)]
    pub operation: SparseOperation,
}

#[derive(Deserialize)]
struct SparseWire {
    schema: SparseSchema,
    action_id: AgentActionId,
    #[serde(default)]
    project_id: Option<ProjectId>,
    #[serde(flatten)]
    operation: SparseOperation,
}

impl<'de> Deserialize<'de> for SparseAgentManagementRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("sparse request must be an object"))?;
        let fields: &[&str] = match object.get("operation").and_then(serde_json::Value::as_str) {
            Some("create") => &["agent", "task", "task_mode", "bootstrap_intent"],
            Some("replace") => &[
                "predecessor_cutex_session_id",
                "policy",
                "successor",
                "task",
                "task_mode",
                "bootstrap_intent",
            ],
            _ => return Err(serde::de::Error::custom("unknown sparse operation")),
        };
        if let Some(field) = object.keys().find(|field| {
            !matches!(
                field.as_str(),
                "schema" | "action_id" | "project_id" | "operation"
            ) && !fields.contains(&field.as_str())
        }) {
            return Err(serde::de::Error::custom(format!(
                "unknown sparse request field `{field}`"
            )));
        }
        let wire: SparseWire = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(Self {
            schema: wire.schema,
            action_id: wire.action_id,
            project_id: wire.project_id,
            operation: wire.operation,
        })
    }
}

impl SparseAgentManagementRequest {
    pub fn digest(&self) -> Result<Sha256, AgentManagementError> {
        super::store::request_sha256(self)
    }
}

/// Whether a request body is a sparse request (by its `schema`).
pub fn is_sparse_body(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .is_some_and(|value| value["schema"] == SPARSE_SCHEMA)
}

/// The full request a sparse action resolved to, bound to the sparse digest
/// and the caller. Stored in the Agent Management store
/// (`sparse_requests`) before the provider sees the full request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SparseRequestRecord {
    pub sparse_sha256: Sha256,
    pub caller_cutex_session: CutexSessionId,
    pub request: AgentManagementRequest,
    pub recorded_at: Rfc3339,
}

/// A model-facing refusal (no write): stable code plus an actionable detail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SparseRefusal {
    pub code: &'static str,
    pub detail: String,
}

fn refuse(code: &'static str, detail: impl Into<String>) -> SparseRefusal {
    SparseRefusal {
        code,
        detail: detail.into(),
    }
}

/// What a sparse request defaults from: the caller and its project.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CallerDefaults {
    pub profile: Option<String>,
    pub cwd: String,
    pub groups: Vec<String>,
    /// The caller's envelope (`None`: no explicit permissions, so a child
    /// without explicit sandbox / approval cannot be derived).
    pub sandbox: Option<SandboxLevel>,
    pub approval: Option<ApprovalStrictness>,
    pub envelope_error: Option<String>,
    pub workspace_root: Option<String>,
}

/// One profile as a sparse request needs it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileFacts {
    /// Canonical profile name (requests may name a profile by id).
    pub name: String,
    pub claude: bool,
    pub default_model: Option<String>,
}

fn given(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|value| !value.is_empty())
}

const SANDBOXES: &str = "read-only, workspace-write, danger-full-access";
const APPROVALS: &str = "untrusted, on-request, never";

/// Resolve one sparse agent into the full spec. Pure: every fact comes from
/// `caller` and `profile`.
pub fn resolve_spec(
    agent: &SparseAgentSpec,
    caller: &CallerDefaults,
    profile: impl FnOnce(&str) -> Result<ProfileFacts, SparseRefusal>,
) -> Result<ManagedAgentSpec, SparseRefusal> {
    let requested_profile = given(&agent.profile)
        .or(caller.profile.as_deref())
        .ok_or_else(|| {
            refuse(
                "profile_required",
                "You have no profile to inherit; pass profile (agent_list shows the profiles of your project's agents).",
            )
        })?;
    let facts = profile(requested_profile)?;
    let model = match given(&agent.model).filter(|model| *model != "default") {
        Some(model) => model.to_string(),
        None => facts.default_model.clone().ok_or_else(|| {
            refuse(
                "model_required",
                format!(
                    "Profile '{}' has no default model; pass model.",
                    facts.name
                ),
            )
        })?,
    };
    let reasoning = given(&agent.reasoning)
        .unwrap_or(crate::profiles::reasoning::DEFAULT)
        .to_string();
    let sandbox = match given(&agent.sandbox) {
        Some(value) => SandboxLevel::parse(value).ok_or_else(|| {
            refuse(
                "invalid_sandbox_mode",
                format!("sandbox '{value}' is not one of: {SANDBOXES}. Omit it to inherit yours."),
            )
        })?,
        None if facts.claude => SandboxLevel::FullAccess,
        None => caller.sandbox.ok_or_else(|| {
            refuse(
                "creator_permissions_unknown",
                caller
                    .envelope_error
                    .clone()
                    .unwrap_or_else(|| "your own sandbox is unknown; pass sandbox".into()),
            )
        })?,
    };
    let approval = match given(&agent.approval_policy) {
        Some(value) => ApprovalStrictness::parse(value).ok_or_else(|| {
            refuse(
                "invalid_approval_policy",
                format!(
                    "approval_policy '{value}' is not one of: {APPROVALS}. Omit it to inherit yours."
                ),
            )
        })?,
        None if facts.claude => ApprovalStrictness::Never,
        None => caller.approval.ok_or_else(|| {
            refuse(
                "creator_permissions_unknown",
                caller
                    .envelope_error
                    .clone()
                    .unwrap_or_else(|| "your own approval policy is unknown; pass approval_policy".into()),
            )
        })?,
    };
    let cwd = given(&agent.cwd)
        .map(str::to_string)
        .or_else(|| caller.workspace_root.clone())
        .unwrap_or_else(|| caller.cwd.clone());
    let cwd = crate::session::metadata::normalize_session_cwd(&cwd);
    let mut groups = crate::agent_bus::identity::normalize_agent_groups(caller.groups.clone());
    if groups.is_empty() {
        let hash = crate::agent_bus::identity::fnv1a_hex(&cwd);
        groups.push(format!("project:{}", &hash[..8.min(hash.len())]));
    }
    Ok(ManagedAgentSpec {
        name: agent.name.clone(),
        cwd,
        profile: Some(facts.name),
        runtime_backend: if facts.claude {
            "claude_subscription"
        } else {
            "host"
        }
        .into(),
        model,
        reasoning,
        permissions: sandbox.as_str().into(),
        approval_policy: approval.as_str().into(),
        sandbox_mode: sandbox.as_str().into(),
        groups,
        expose_to_im: agent.expose_to_im,
        pin: agent.pin,
    })
}

fn start(task: &Option<String>) -> (AgentStartMode, Option<String>) {
    match task.as_deref().filter(|task| !task.trim().is_empty()) {
        Some(task) => (AgentStartMode::CustomMessage, Some(task.to_string())),
        None => (AgentStartMode::BootstrapOnly, None),
    }
}

/// The start of a freshly resolved request: a delegated task is no start
/// message (the agent starts idle; [`delegated_task`] delegates it).
/// A Human-reviewed intent keeps its reviewed start message.
fn start_for(
    task: &Option<String>,
    mode: Option<TaskMode>,
    reviewed: bool,
) -> (AgentStartMode, Option<String>) {
    match mode.unwrap_or_default() {
        TaskMode::Delegation if !reviewed => (AgentStartMode::BootstrapOnly, None),
        _ => start(task),
    }
}

/// Resolve a whole sparse request (pure; see [`resolve_spec`]).
pub fn resolve_request(
    sparse: &SparseAgentManagementRequest,
    caller: &CallerDefaults,
    profile: impl FnOnce(&str) -> Result<ProfileFacts, SparseRefusal>,
) -> Result<AgentManagementRequest, SparseRefusal> {
    let spec = resolve_spec(sparse.operation.agent(), caller, profile)?;
    let operation = match &sparse.operation {
        SparseOperation::Create {
            task,
            task_mode,
            bootstrap_intent,
            ..
        } => {
            let (start_mode, frozen_message) =
                start_for(task, *task_mode, bootstrap_intent.is_some());
            AgentOperation::Create {
                spec,
                bootstrap_intent: bootstrap_intent.clone(),
                start_mode,
                frozen_message,
            }
        }
        SparseOperation::Replace {
            predecessor_cutex_session_id,
            policy,
            task,
            task_mode,
            bootstrap_intent,
            ..
        } => {
            let (start_mode, frozen_message) =
                start_for(task, *task_mode, bootstrap_intent.is_some());
            AgentOperation::Replace {
                bootstrap_intent: bootstrap_intent.clone(),
                predecessor_cutex_session_id: predecessor_cutex_session_id.clone(),
                policy: *policy,
                successor: spec,
                start_mode,
                frozen_message,
            }
        }
    };
    Ok(AgentManagementRequest {
        schema: AgentManagementSchema::V1,
        action_id: sparse.action_id.clone(),
        project_id: sparse.project_id.clone(),
        operation,
    })
}

/// The reviewed request a sparse call referencing a Human-reviewed intent
/// stands for: the operation, project, Replace predecessor / policy, every
/// field the call gives and its task must match the review; omitted fields
/// take the reviewed values. Any difference is `bootstrap_intent_conflict`
/// (no write). Pure apart from canonicalizing a given profile name.
fn reviewed_request(
    sparse: &SparseAgentManagementRequest,
    reviewed: &AgentManagementRequest,
    profile: impl FnOnce(&str) -> Result<ProfileFacts, SparseRefusal>,
) -> Result<AgentManagementRequest, SparseRefusal> {
    let conflict = |what: &str| {
        refuse(
            "bootstrap_intent_conflict",
            format!(
                "the Human-reviewed intent '{}' has another {what}; omit it or pass the reviewed value. Nothing was created.",
                sparse.action_id.as_str()
            ),
        )
    };
    if sparse
        .project_id
        .as_ref()
        .is_some_and(|project| reviewed.project_id.as_ref() != Some(project))
    {
        return Err(conflict("project_id"));
    }
    let (spec, start_mode, frozen_message, task) = match (&sparse.operation, &reviewed.operation) {
        (
            SparseOperation::Create { task, .. },
            AgentOperation::Create {
                spec,
                start_mode,
                frozen_message,
                ..
            },
        ) => (spec, start_mode, frozen_message, task),
        (
            SparseOperation::Replace {
                predecessor_cutex_session_id,
                policy,
                task,
                ..
            },
            AgentOperation::Replace {
                predecessor_cutex_session_id: reviewed_predecessor,
                policy: reviewed_policy,
                successor,
                start_mode,
                frozen_message,
                ..
            },
        ) => {
            if predecessor_cutex_session_id != reviewed_predecessor {
                return Err(conflict("predecessor_cutex_session_id"));
            }
            if policy != reviewed_policy {
                return Err(conflict("policy"));
            }
            (successor, start_mode, frozen_message, task)
        }
        _ => return Err(conflict("operation")),
    };
    let agent = sparse.operation.agent();
    if agent.name != spec.name {
        return Err(conflict("name"));
    }
    if let Some(given) = given(&agent.profile) {
        let reviewed_profile = spec.profile.as_deref().unwrap_or_default();
        if given != reviewed_profile && profile(given)?.name != reviewed_profile {
            return Err(conflict("profile"));
        }
    }
    if given(&agent.model).is_some_and(|model| model != "default" && model != spec.model) {
        return Err(conflict("model"));
    }
    if given(&agent.reasoning).is_some_and(|reasoning| reasoning != spec.reasoning) {
        return Err(conflict("reasoning"));
    }
    if let Some(value) = given(&agent.sandbox) {
        if SandboxLevel::parse(value).is_none() || SandboxLevel::parse(value) != SandboxLevel::parse(&spec.sandbox_mode) {
            return Err(conflict("sandbox"));
        }
    }
    if let Some(value) = given(&agent.approval_policy) {
        if ApprovalStrictness::parse(value).is_none()
            || ApprovalStrictness::parse(value) != ApprovalStrictness::parse(&spec.approval_policy)
        {
            return Err(conflict("approval_policy"));
        }
    }
    if given(&agent.cwd).is_some_and(|cwd| {
        crate::session::metadata::normalize_session_cwd(cwd)
            != crate::session::metadata::normalize_session_cwd(&spec.cwd)
    }) {
        return Err(conflict("cwd"));
    }
    // `expose_to_im` is retired (`im-exposure-retired`): accepted, ignored.
    if agent.pin && !spec.pin {
        return Err(conflict("pin"));
    }
    if let (AgentStartMode::CustomMessage, Some(task)) = start(task) {
        if *start_mode != AgentStartMode::CustomMessage
            || frozen_message.as_deref() != Some(task.as_str())
        {
            return Err(conflict("task"));
        }
    }
    Ok(reviewed.clone())
}

/// A sparse request bound to its full resolution.
#[derive(Clone, Debug)]
pub struct ResolvedSparse {
    pub request: AgentManagementRequest,
    pub sparse_sha256: Sha256,
    /// False when an earlier call of the same action already stored it.
    pub new: bool,
}

/// A sparse request admitted for its caller: the frozen full request bound
/// to the action id, and the sparse digest the reply carries.
#[derive(Clone, Debug)]
pub struct AdmittedSparse {
    pub request: AgentManagementRequest,
    pub sparse_sha256: Sha256,
}

/// Bus side, before the launch policy (approval gate) and the provider:
/// resolve `sparse` for `caller` from the live stores (an earlier identical
/// call's stored resolution is replayed), then authorize the caller and bind
/// action id + caller + resolution in one store transaction. The policy and
/// the provider therefore only ever see the frozen first resolution: an
/// approval asked for it is found again by the exact retry, whatever the
/// defaults became meanwhile (R4), and a refused caller binds nothing (R2).
pub fn admit(
    caller: &CutexSessionId,
    sparse: &SparseAgentManagementRequest,
) -> Result<AdmittedSparse, SparseRefusal> {
    let store = super::AgentManagementStore::open_default().map_err(|_| unavailable())?;
    admit_with(
        &store,
        caller,
        sparse,
        || {
            let sessions = crate::session::store::load_cutex_session_store_shared()
                .map_err(|_| unavailable())?;
            sessions.sessions.get(caller.as_str()).cloned().ok_or_else(|| {
                refuse("stale_runtime_identity", "the calling agent has no durable session")
            })
        },
        live_profile_facts,
    )
}

fn admit_with(
    store: &super::AgentManagementStore,
    caller: &CutexSessionId,
    sparse: &SparseAgentManagementRequest,
    caller_record: impl FnOnce() -> Result<crate::session::model::CutexSessionRecord, SparseRefusal>,
    profile: impl FnOnce(&str) -> Result<ProfileFacts, SparseRefusal>,
) -> Result<AdmittedSparse, SparseRefusal> {
    let resolved = resolve_with(store, caller, sparse, caller_record, profile)?;
    let sparse_sha256 = resolved.sparse_sha256.clone();
    let request = record_with(store, caller, resolved)?;
    Ok(AdmittedSparse {
        request,
        sparse_sha256,
    })
}

fn resolve_with(
    store: &super::AgentManagementStore,
    caller: &CutexSessionId,
    sparse: &SparseAgentManagementRequest,
    caller_record: impl FnOnce() -> Result<crate::session::model::CutexSessionRecord, SparseRefusal>,
    profile: impl FnOnce(&str) -> Result<ProfileFacts, SparseRefusal>,
) -> Result<ResolvedSparse, SparseRefusal> {
    let sparse_sha256 = sparse
        .digest()
        .map_err(|_| refuse("invalid_arguments", "request not serializable"))?;
    let snapshot = store.snapshot().map_err(|_| unavailable())?;
    if let Some(stored) = snapshot.sparse_requests.get(&sparse.action_id) {
        return if &stored.caller_cutex_session == caller && stored.sparse_sha256 == sparse_sha256 {
            Ok(ResolvedSparse {
                request: stored.request.clone(),
                sparse_sha256,
                new: false,
            })
        } else {
            Err(payload_conflict(&sparse.action_id))
        };
    }
    if snapshot.actions.contains_key(&sparse.action_id) {
        return Err(payload_conflict(&sparse.action_id));
    }
    // A Human-reviewed intent fixes the exact request: what the call gives
    // must match it, omitted fields take the reviewed values (R1).
    if let Some(intent_id) = sparse.operation.bootstrap_intent() {
        if intent_id != &sparse.action_id {
            return Err(refuse(
                "bootstrap_intent_requires_exact_action",
                "bootstrap_intent must equal action_id (the reviewed intent's id); nothing was created",
            ));
        }
        let Some(intent) = snapshot.bootstrap_intents.get(intent_id) else {
            return Err(refuse(
                "bootstrap_intent_missing",
                "no Human-reviewed intent has this id; nothing was created",
            ));
        };
        let request = reviewed_request(sparse, &intent.request, profile)?;
        return Ok(ResolvedSparse {
            request,
            sparse_sha256,
            new: true,
        });
    }
    let record = caller_record()?;
    let record = &record;
    let (sandbox, approval, envelope_error) = match PermissionEnvelope::from_record(record) {
        Ok(envelope) => (Some(envelope.sandbox), Some(envelope.approval), None),
        Err(error) => (None, None, Some(error)),
    };
    let project = super::provider::managed_project_for(&snapshot, caller, sparse.project_id.as_ref());
    let defaults = CallerDefaults {
        profile: record.profile.clone().filter(|profile| !profile.trim().is_empty()),
        cwd: crate::session::metadata::cutex_session_launch_cwd(record).to_string(),
        groups: record.agent_groups.clone(),
        sandbox,
        approval,
        envelope_error,
        workspace_root: project
            .and_then(|project| snapshot.project_states.get(&project))
            .and_then(|state| state.workspace_root.clone()),
    };
    let request = resolve_request(sparse, &defaults, profile)?;
    Ok(ResolvedSparse {
        request,
        sparse_sha256,
        new: true,
    })
}

/// Bind a new resolution to its action id (after authorizing the caller, in
/// the same transaction). A concurrent identical call that stored first wins
/// (its full request is returned and used).
fn record_with(
    store: &super::AgentManagementStore,
    caller: &CutexSessionId,
    resolved: ResolvedSparse,
) -> Result<AgentManagementRequest, SparseRefusal> {
    if !resolved.new {
        return Ok(resolved.request);
    }
    let action_id = resolved.request.action_id.clone();
    // Same order as the provider: mutation fence, then the store lock.
    let _mutation = store.lock_mutations().map_err(|_| unavailable())?;
    store
        .with_state(true, |mut state| {
            if let Some(stored) = state.sparse_requests.get(&action_id) {
                let request = (stored.caller_cutex_session == *caller
                    && stored.sparse_sha256 == resolved.sparse_sha256)
                    .then(|| stored.request.clone())
                    .ok_or_else(|| payload_conflict(&action_id));
                return Ok((state, request, false));
            }
            if state.actions.contains_key(&action_id) {
                return Ok((state, Err(payload_conflict(&action_id)), false));
            }
            // R2: authorize before binding, in the same transaction, so a
            // caller the provider would refuse never takes the action id.
            if let Err(error) =
                super::provider::authorize_sparse_binding(&state, caller, &resolved.request)
            {
                return Ok((state, Err(authorization_refusal(&error)), false));
            }
            state.sparse_requests.insert(
                action_id.clone(),
                SparseRequestRecord {
                    sparse_sha256: resolved.sparse_sha256.clone(),
                    caller_cutex_session: caller.clone(),
                    request: resolved.request.clone(),
                    recorded_at: super::store::now(),
                },
            );
            Ok((state, Ok(resolved.request.clone()), true))
        })
        .map_err(|_| unavailable())?
}

/// A provider authorization error as a sparse refusal (specific reason
/// token as the code, as the provider's no-write answer would carry it).
fn authorization_refusal(error: &AgentManagementError) -> SparseRefusal {
    let code = match error {
        AgentManagementError::Conflict(reason) => reason,
        other => other.code(),
    };
    let (_, detail) = error.wire();
    refuse(code, format!("{detail}; nothing was created"))
}

/// The full request's digest as the provider records it (the reply carries
/// the sparse digest instead).
pub fn full_sha256(request: &AgentManagementRequest) -> Option<Sha256> {
    super::store::request_sha256(request).ok()
}

fn unavailable() -> SparseRefusal {
    refuse(
        "persistence_unavailable",
        "Agent Management state is unavailable; nothing was created",
    )
}

fn payload_conflict(action_id: &AgentActionId) -> SparseRefusal {
    refuse(
        "action_id_payload_conflict",
        format!(
            "action_id '{}' was already used with other arguments; nothing was created. Omit action_id (or use a new one) for a different request.",
            action_id.as_str()
        ),
    )
}

/// Profile facts from the live profile store and new-session defaults.
fn live_profile_facts(requested: &str) -> Result<ProfileFacts, SparseRefusal> {
    let store = crate::profiles::settings_api::load_store().map_err(|_| unavailable())?;
    let account = crate::profiles::settings_api::find(&store, requested.trim()).ok_or_else(|| {
        refuse(
            "unknown_profile",
            crate::profiles::identity::unknown_profile_message(
                requested,
                &crate::profiles::identity::available_profile_names(&store),
            ),
        )
    })?;
    let configured = crate::config::store::load_codez_config_checked()
        .ok()
        .and_then(|config| config.new_session_defaults.get(&account.name).and_then(|d| d.model.clone()));
    let default_model = configured
        .or_else(|| {
            crate::config::new_session::inherited(&account.name)
                .ok()
                .and_then(|defaults| defaults.model)
        })
        .filter(|model| !model.trim().is_empty());
    Ok(ProfileFacts {
        name: account.name.clone(),
        claude: account.cli_kind == crate::profiles::model::CliKind::Claude,
        default_model,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn caller() -> CallerDefaults {
        CallerDefaults {
            profile: Some("Zephyr".into()),
            cwd: if cfg!(windows) { "F:\\test-1".into() } else { "/work/test-1".into() },
            groups: vec!["project:ac657226".into()],
            sandbox: Some(SandboxLevel::WorkspaceWrite),
            approval: Some(ApprovalStrictness::OnRequest),
            envelope_error: None,
            workspace_root: None,
        }
    }

    fn codex(name: &str) -> Result<ProfileFacts, SparseRefusal> {
        Ok(ProfileFacts {
            name: if name == "zephyr-id" { "Zephyr".into() } else { name.into() },
            claude: name == "Claude",
            default_model: Some(format!("{name}-model")),
        })
    }

    fn sparse(value: serde_json::Value) -> SparseAgentManagementRequest {
        let mut body = json!({"schema":SPARSE_SCHEMA,"action_id":"a-1"});
        body.as_object_mut().unwrap().extend(value.as_object().unwrap().clone());
        serde_json::from_value(body).unwrap()
    }

    #[test]
    fn name_and_task_resolve_to_a_full_valid_spec_from_the_caller() {
        let request = resolve_request(
            &sparse(json!({"operation":"create","agent":{"name":"quokka"},"task":"map the repo"})),
            &caller(),
            codex,
        )
        .unwrap();
        request.validate().unwrap();
        let AgentOperation::Create { spec, start_mode, frozen_message, bootstrap_intent } = &request.operation else {
            panic!("create expected")
        };
        // agent-create-task-delegation: the task is delegated, not a start message.
        assert_eq!(*start_mode, AgentStartMode::BootstrapOnly);
        assert!(frozen_message.is_none());
        assert!(bootstrap_intent.is_none());
        assert_eq!(spec.name, "quokka");
        assert_eq!(spec.profile.as_deref(), Some("Zephyr"));
        assert_eq!(spec.model, "Zephyr-model");
        assert_eq!(spec.reasoning, "default");
        assert_eq!(spec.runtime_backend, "host");
        assert_eq!((spec.sandbox_mode.as_str(), spec.permissions.as_str()), ("workspace-write", "workspace-write"));
        assert_eq!(spec.approval_policy, "on-request");
        assert_eq!(spec.cwd, caller().cwd);
        assert_eq!(spec.groups, ["project:ac657226"]);
    }

    #[test]
    fn no_task_is_bootstrap_only_and_workspace_root_wins_over_the_caller_cwd() {
        let mut defaults = caller();
        let root = if cfg!(windows) { "F:\\project" } else { "/project" };
        defaults.workspace_root = Some(root.into());
        defaults.groups.clear();
        let request = resolve_request(
            &sparse(json!({"operation":"create","agent":{"name":"emu"},"task":"  "})),
            &defaults,
            codex,
        )
        .unwrap();
        let AgentOperation::Create { spec, start_mode, frozen_message, .. } = &request.operation else { panic!() };
        assert_eq!(*start_mode, AgentStartMode::BootstrapOnly);
        assert!(frozen_message.is_none());
        assert_eq!(spec.cwd, root);
        assert_eq!(spec.groups.len(), 1);
        assert!(spec.groups[0].starts_with("project:"));
        request.validate().unwrap();
    }

    #[test]
    fn explicit_fields_win_and_aliases_are_canonical() {
        let cwd = if cfg!(windows) { "F:\\elsewhere" } else { "/elsewhere" };
        let request = resolve_request(
            &sparse(json!({"operation":"create","agent":{"name":"dingo","profile":"zephyr-id","model":"glm-5.3",
                "reasoning":"max","sandbox":"read-only","approval_policy":"on-failure","cwd":cwd,"pin":true}})),
            &caller(),
            codex,
        )
        .unwrap();
        let AgentOperation::Create { spec, .. } = &request.operation else { panic!() };
        assert_eq!(spec.profile.as_deref(), Some("Zephyr"));
        assert_eq!((spec.model.as_str(), spec.reasoning.as_str()), ("glm-5.3", "max"));
        assert_eq!((spec.sandbox_mode.as_str(), spec.permissions.as_str()), ("read-only", "read-only"));
        assert_eq!(spec.approval_policy, "on-request");
        assert_eq!(spec.cwd, cwd);
        assert!(spec.pin);
    }

    #[test]
    fn claude_profiles_get_their_backend_and_launch_shape() {
        let request = resolve_request(
            &sparse(json!({"operation":"create","agent":{"name":"kiwi","profile":"Claude"}})),
            &caller(),
            codex,
        )
        .unwrap();
        let AgentOperation::Create { spec, .. } = &request.operation else { panic!() };
        assert_eq!(spec.runtime_backend, "claude_subscription");
        assert_eq!(spec.sandbox_mode, "danger-full-access");
        assert_eq!(spec.approval_policy, "never");
    }

    #[test]
    fn refusals_name_the_field_and_the_allowed_values() {
        let check = |agent: serde_json::Value, defaults: CallerDefaults, code: &str| {
            let error = resolve_request(&sparse(json!({"operation":"create","agent":agent})), &defaults, |name| {
                if name == "nomodel" {
                    Ok(ProfileFacts { name: name.into(), claude: false, default_model: None })
                } else if name == "ghost" {
                    Err(refuse("unknown_profile", "Profile not found"))
                } else {
                    codex(name)
                }
            })
            .unwrap_err();
            assert_eq!(error.code, code, "{}", error.detail);
            error.detail
        };
        assert!(check(json!({"name":"a","sandbox":"full"}), caller(), "invalid_sandbox_mode").contains("danger-full-access"));
        assert!(check(json!({"name":"a","approval_policy":"ask"}), caller(), "invalid_approval_policy").contains("on-request"));
        check(json!({"name":"a","profile":"nomodel"}), caller(), "model_required");
        check(json!({"name":"a","profile":"ghost"}), caller(), "unknown_profile");
        let mut no_profile = caller();
        no_profile.profile = None;
        check(json!({"name":"a"}), no_profile, "profile_required");
        let mut no_envelope = caller();
        no_envelope.sandbox = None;
        no_envelope.envelope_error = Some("the creating agent has no explicit sandbox".into());
        assert!(check(json!({"name":"a"}), no_envelope, "creator_permissions_unknown").contains("explicit sandbox"));
    }

    #[test]
    fn replace_successor_resolves_the_same_way() {
        let request = resolve_request(
            &sparse(json!({"operation":"replace","predecessor_cutex_session_id":"cutex.old","policy":"close_after_ready",
                "successor":{"name":"wombat-2"},"task":"continue"})),
            &caller(),
            codex,
        )
        .unwrap();
        request.validate().unwrap();
        let AgentOperation::Replace { successor, policy, start_mode, .. } = &request.operation else { panic!() };
        assert_eq!(successor.name, "wombat-2");
        assert_eq!(*policy, AgentReplacePolicy::CloseAfterReady);
        assert_eq!(*start_mode, AgentStartMode::BootstrapOnly);
        let replace = sparse(json!({"operation":"replace","predecessor_cutex_session_id":"cutex.old","policy":"close_after_ready",
            "successor":{"name":"wombat-2"},"task":"continue"}));
        assert_eq!(delegated_task(&replace, &request), Some("continue"), "member_replace shares the rule");
    }

    /// agent-create-task-delegation: a task is delegated by default;
    /// `task_mode: message` keeps the start message; no task, a reviewed
    /// intent or a stored pre-release resolution delegates nothing.
    #[test]
    fn task_mode_selects_delegation_or_the_start_message() {
        let delegated = sparse(json!({"operation":"create","agent":{"name":"quokka"},"task":"map the repo"}));
        let request = resolve_request(&delegated, &caller(), codex).unwrap();
        assert_eq!(delegated_task(&delegated, &request), Some("map the repo"));
        let explicit = sparse(json!({"operation":"create","agent":{"name":"quokka"},"task":"map the repo","task_mode":"delegation"}));
        assert_eq!(resolve_request(&explicit, &caller(), codex).unwrap(), request);
        assert_ne!(explicit.digest().unwrap(), delegated.digest().unwrap(), "task_mode is part of the request");

        let message = sparse(json!({"operation":"create","agent":{"name":"quokka"},"task":"map the repo","task_mode":"message"}));
        let old = resolve_request(&message, &caller(), codex).unwrap();
        let AgentOperation::Create { start_mode, frozen_message, .. } = &old.operation else { panic!() };
        assert_eq!(*start_mode, AgentStartMode::CustomMessage);
        assert_eq!(frozen_message.as_deref(), Some("map the repo"));
        assert_eq!(delegated_task(&message, &old), None);
        // A resolution stored before this release (start message) replays as is.
        assert_eq!(delegated_task(&delegated, &old), None);

        // A Human-reviewed intent keeps its reviewed start message.
        let reviewed = sparse(json!({"action_id":"intent-1","operation":"create","agent":{"name":"quokka"},
            "task":"map the repo","bootstrap_intent":"intent-1"}));
        let intent = resolve_request(&reviewed, &caller(), codex).unwrap();
        let AgentOperation::Create { start_mode, .. } = &intent.operation else { panic!() };
        assert_eq!(*start_mode, AgentStartMode::CustomMessage);
        assert_eq!(delegated_task(&reviewed, &intent), None);

        let idle = sparse(json!({"operation":"create","agent":{"name":"quokka"},"task":" "}));
        assert_eq!(delegated_task(&idle, &resolve_request(&idle, &caller(), codex).unwrap()), None);
        // The digest of a call without task_mode is unchanged (exact replays).
        let without: serde_json::Value = serde_json::to_value(&delegated).unwrap();
        assert!(without.get("task_mode").is_none(), "{without}");
        let bogus = serde_json::from_value::<SparseAgentManagementRequest>(
            json!({"schema":SPARSE_SCHEMA,"action_id":"a-1","operation":"create","agent":{"name":"q"},"task_mode":"note"}));
        assert!(bogus.is_err());
    }

    fn creator(id: &CutexSessionId) -> crate::session::model::CutexSessionRecord {
        let cwd = if cfg!(windows) { "C:\\work\\project" } else { "/work/project" };
        let mut record = crate::session::model::CutexSessionRecord::new(
            id.as_str().to_string(),
            None,
            "host".into(),
            cwd.into(),
            None,
        )
        .unwrap();
        record.profile = Some("Zephyr".into());
        record.agent_groups = vec!["project:abcdef12".into()];
        crate::config::new_thread::LaunchPermissions {
            sandbox: "workspace-write".into(),
            approval: "on-request".into(),
            approvals_reviewer: None,
            network_access: false,
            writable_roots: Vec::new(),
        }
        .apply_to_record(&mut record);
        record
    }

    #[test]
    fn a_stored_resolution_replays_exactly_and_binds_its_action_id() {
        let caller = session("c1");
        let (root, store, _) = project_store(&caller);
        let request = sparse(json!({"operation":"create","agent":{"name":"quokka"},"task":"t"}));
        let first = resolve_with(&store, &caller, &request, || Ok(creator(&caller)), codex).unwrap();
        assert!(first.new);
        let full = record_with(&store, &caller, first.clone()).unwrap();
        assert_eq!(full, first.request);
        let AgentOperation::Create { spec, .. } = &full.operation else { panic!() };
        assert_eq!(spec.sandbox_mode, "workspace-write");
        assert_eq!(spec.groups, ["project:abcdef12"]);

        // The same call later replays the stored request, even though the
        // profile's default model changed meanwhile.
        let replay = resolve_with(&store, &caller, &request, || panic!("caller not read on replay"), |name| {
            Ok(ProfileFacts { name: name.into(), claude: false, default_model: Some("new-default".into()) })
        })
        .unwrap();
        assert!(!replay.new);
        assert_eq!(replay.request, full);
        assert_eq!(replay.sparse_sha256, first.sparse_sha256);
        assert_eq!(record_with(&store, &caller, replay).unwrap(), full);

        // Other arguments, or another caller, under the same action_id conflict.
        let changed = sparse(json!({"operation":"create","agent":{"name":"quokka"},"task":"other"}));
        let error = resolve_with(&store, &caller, &changed, || Ok(creator(&caller)), codex).unwrap_err();
        assert_eq!(error.code, "action_id_payload_conflict");
        assert!(error.detail.contains("Omit action_id"), "{}", error.detail);
        let other = CutexSessionId::new("cutex.00000000-0000-0000-0000-0000000000c2".to_string()).unwrap();
        assert_eq!(
            resolve_with(&store, &other, &request, || Ok(creator(&other)), codex).unwrap_err().code,
            "action_id_payload_conflict"
        );
        // A racing identical call that recorded first wins; a different one conflicts.
        let mut racing = resolve_with(&store, &caller, &sparse(json!({"action_id":"a-2","operation":"create","agent":{"name":"emu"}})), || Ok(creator(&caller)), codex).unwrap();
        racing.request.action_id = AgentActionId::new("a-1").unwrap();
        assert_eq!(record_with(&store, &caller, racing).unwrap_err().code, "action_id_payload_conflict");
        assert_eq!(store.snapshot().unwrap().sparse_requests.len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn session(suffix: &str) -> CutexSessionId {
        CutexSessionId::new(format!("cutex.00000000-0000-0000-0000-0000000000{suffix}")).unwrap()
    }

    /// A temp store with project `p-1` whose Primary Director is `director`.
    fn project_store(director: &CutexSessionId) -> (std::path::PathBuf, super::super::AgentManagementStore, ProjectId) {
        let root = std::env::temp_dir().join(format!("cutex-sparse-{}", uuid::Uuid::new_v4()));
        let store = super::super::AgentManagementStore::open(&root).unwrap();
        let project = ProjectId::new("p-1").unwrap();
        store
            .with_state(true, |mut state| {
                state.schema = super::super::AgentManagementStoreSchema::V2;
                state.projects.insert(
                    project.clone(),
                    super::super::ProjectAuthority {
                        project_id: project.clone(),
                        authorized_director_session: director.clone(),
                        authority_epoch: 1,
                        updated_at: super::super::store::now(),
                    },
                );
                Ok((state, (), true))
            })
            .unwrap();
        (root, store, project)
    }

    /// Store a Human-reviewed v2 intent for `request` (its action id).
    fn store_intent(store: &super::super::AgentManagementStore, director: &CutexSessionId, request: AgentManagementRequest) {
        let sha = |c: &str| Sha256::new(c.repeat(64)).unwrap();
        let review = super::super::BootstrapIntentReview {
            version: 2,
            request,
            director: director.clone(),
            authority_sha256: sha("a"),
            store_revision: 1,
            native_home: "native".into(),
            bundle_manifest: "bundle.json".into(),
            bundle_sha256: sha("b"),
            configuration: crate::launch::stock::StockConfiguration {
                selected_projection: None,
                private_chatgpt_auth: None,
                profile_name: "Zephyr".into(),
                profile_id: uuid::Uuid::nil().to_string(),
                inherited: false,
                profile_sha256: sha("c"),
                account_sha256: sha("d"),
                model: "Zephyr-model".into(),
                reasoning: None,
                model_provider: "openai".into(),
                provider: crate::launch::stock::DummyProvider {
                    name: "n".into(),
                    base_url: "http://127.0.0.1:1/v1".into(),
                    wire_api: "responses".into(),
                    requires_openai_auth: false,
                    supports_websockets: false,
                },
                sandbox: "workspace-write".into(),
                approval: "on-request".into(),
                approvals_reviewer: None,
                network_access: false,
                writable_roots: Vec::new(),
            },
            runtime_groups: Vec::new(),
            expires_at_unix: chrono::Utc::now().timestamp() + 3600,
            job_mcp: None,
        };
        store
            .with_state(true, |mut state| {
                state.bootstrap_intents.insert(review.request.action_id.clone(), review);
                Ok((state, (), true))
            })
            .unwrap();
        store.snapshot().unwrap();
    }

    fn no_sparse_side_effects(store: &super::super::AgentManagementStore) {
        let snapshot = store.snapshot().unwrap();
        assert!(snapshot.sparse_requests.is_empty(), "{:?}", snapshot.sparse_requests.keys());
        assert!(snapshot.actions.is_empty());
    }

    /// R1 (review 2026-10-11): a sparse call that references a reviewed
    /// intent must match it (operation, project, predecessor / policy,
    /// explicit fields, task); only omitted fields take the reviewed values.
    /// A Create naming the reviewed Replace's successor must not turn into
    /// that Replace (which would close the predecessor).
    #[test]
    fn an_intent_reference_must_match_the_reviewed_operation_and_fields() {
        let director = session("d1");
        let (root, store, project) = project_store(&director);
        let mut reviewed = resolve_request(
            &sparse(json!({"action_id":"intent-r","operation":"replace","predecessor_cutex_session_id":"cutex.old",
                "policy":"close_before_create","successor":{"name":"new-agent"},"bootstrap_intent":"intent-r"})),
            &caller(),
            codex,
        )
        .unwrap();
        reviewed.project_id = Some(project.clone());
        store_intent(&store, &director, reviewed.clone());
        let resolve = |body: serde_json::Value| {
            let mut body = body;
            body["action_id"] = json!("intent-r");
            body["bootstrap_intent"] = json!("intent-r");
            resolve_with(&store, &director, &sparse(body), || Ok(creator(&director)), codex)
        };
        let replace = |extra: serde_json::Value| {
            let mut body = json!({"operation":"replace","predecessor_cutex_session_id":"cutex.old",
                "policy":"close_before_create","successor":{"name":"new-agent"}});
            for (key, value) in extra.as_object().unwrap() {
                if key == "successor" {
                    body["successor"].as_object_mut().unwrap().extend(value.as_object().unwrap().clone());
                } else {
                    body[key] = value.clone();
                }
            }
            body
        };
        for (label, body) in [
            ("create of a reviewed replace", json!({"operation":"create","agent":{"name":"new-agent"}})),
            ("other predecessor", replace(json!({"predecessor_cutex_session_id":"cutex.other"}))),
            ("other policy", replace(json!({"policy":"keep_old"}))),
            ("other project", replace(json!({"project_id":"p-2"}))),
            ("other name", replace(json!({"successor":{"name":"else"}}))),
            ("other model", replace(json!({"successor":{"model":"glm-9"}}))),
            ("other reasoning", replace(json!({"successor":{"reasoning":"max"}}))),
            ("other sandbox", replace(json!({"successor":{"sandbox":"read-only"}}))),
            ("other approval", replace(json!({"successor":{"approval_policy":"never"}}))),
            ("other cwd", replace(json!({"successor":{"cwd":if cfg!(windows) { "F:\\else" } else { "/else" }}}))),
            ("other profile", replace(json!({"successor":{"profile":"Other"}}))),
            ("pin not reviewed", replace(json!({"successor":{"pin":true}}))),
            ("task not reviewed", replace(json!({"task":"do something else"}))),
        ] {
            let error = resolve(body).expect_err(label);
            assert_eq!(error.code, "bootstrap_intent_conflict", "{label}: {}", error.detail);
        }
        // Omitted fields take the reviewed values; explicit equal ones (an
        // alias included) are accepted.
        let same = resolve(replace(json!({"project_id":"p-1","successor":{"profile":"zephyr-id","model":"Zephyr-model",
            "sandbox":"workspace-write","approval_policy":"on-failure","cwd":caller().cwd}})))
        .unwrap();
        assert_eq!(same.request, reviewed);
        assert_eq!(resolve(replace(json!({}))).unwrap().request, reviewed);
        no_sparse_side_effects(&store);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// R2 (review 2026-10-11): an unauthorized caller's sparse call binds
    /// nothing, so the Director can still use the same action_id; a
    /// reviewed intent binds only for its Director.
    #[test]
    fn an_unauthorized_caller_cannot_bind_another_directors_action_id() {
        let director = session("d1");
        let intruder = session("e1");
        let (root, store, project) = project_store(&director);
        let request = sparse(json!({"action_id":"planned","project_id":"p-1","operation":"create","agent":{"name":"quokka"}}));
        let resolved = resolve_with(&store, &intruder, &request, || Ok(creator(&intruder)), codex).unwrap();
        let error = record_with(&store, &intruder, resolved).unwrap_err();
        assert_eq!(error.code, "not_authorized_director", "{}", error.detail);
        no_sparse_side_effects(&store);
        let resolved = resolve_with(&store, &director, &request, || Ok(creator(&director)), codex).unwrap();
        let full = record_with(&store, &director, resolved).unwrap();
        assert_eq!(full.project_id.as_ref(), Some(&project));
        assert_eq!(store.snapshot().unwrap().sparse_requests[&AgentActionId::new("planned").unwrap()].caller_cutex_session, director);

        // A reviewed intent: only its Director may bind it.
        let mut reviewed = resolve_request(
            &sparse(json!({"action_id":"intent-c","operation":"create","agent":{"name":"emu"},"bootstrap_intent":"intent-c"})),
            &caller(),
            codex,
        )
        .unwrap();
        reviewed.project_id = Some(project.clone());
        store_intent(&store, &director, reviewed.clone());
        let by_name = sparse(json!({"action_id":"intent-c","operation":"create","agent":{"name":"emu"},"bootstrap_intent":"intent-c"}));
        let resolved = resolve_with(&store, &intruder, &by_name, || Ok(creator(&intruder)), codex);
        if let Ok(resolved) = resolved {
            assert_eq!(record_with(&store, &intruder, resolved).unwrap_err().code, "bootstrap_intent_conflict");
        }
        assert!(!store.snapshot().unwrap().sparse_requests.contains_key(&AgentActionId::new("intent-c").unwrap()));
        let resolved = resolve_with(&store, &director, &by_name, || Ok(creator(&director)), codex).unwrap();
        assert_eq!(record_with(&store, &director, resolved).unwrap(), reviewed);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// R4 (review 2026-10-11): admission (what the route runs before the
    /// launch policy / approval gate) binds the first resolution, so the
    /// retry after an owner approval gets the request that approval was
    /// asked for even though the profile default changed while it waited.
    #[test]
    fn the_first_resolution_is_frozen_before_the_approval_gate() {
        let director = session("d1");
        let (root, store, _) = project_store(&director);
        let request = sparse(json!({"action_id":"needs-approval","operation":"create","agent":{"name":"quokka"}}));
        let first = admit_with(&store, &director, &request, || Ok(creator(&director)), codex).unwrap();
        let AgentOperation::Create { spec, .. } = &first.request.operation else { panic!() };
        assert_eq!(spec.model, "Zephyr-model");
        // The gate answered approval_required here; the owner changes the
        // profile default, then approves; the caller retries exactly.
        let approval_digest = |request: &AgentManagementRequest| {
            crate::approvals::request_digest(&json!({"project_id": request.project_id, "operation": request.operation}))
        };
        let retry = admit_with(&store, &director, &request, || panic!("frozen: caller not read"), |name| {
            Ok(ProfileFacts { name: name.into(), claude: false, default_model: Some("changed-default".into()) })
        })
        .unwrap();
        assert_eq!(retry.request, first.request);
        assert_eq!(retry.sparse_sha256, first.sparse_sha256);
        assert_eq!(approval_digest(&retry.request), approval_digest(&first.request));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sparse_wire_is_strict_and_its_digest_is_canonical() {
        for bad in [
            json!({"schema":SPARSE_SCHEMA,"action_id":"a","operation":"create","agent":{"name":"x"},"spec":{}}),
            json!({"schema":SPARSE_SCHEMA,"action_id":"a","operation":"create","agent":{"name":"x","groups":[]}}),
            json!({"schema":SPARSE_SCHEMA,"action_id":"a","operation":"close","cutex_session_id":"cutex.x"}),
            json!({"schema":"cutex/agent-management/v1","action_id":"a","operation":"create","agent":{"name":"x"}}),
        ] {
            assert!(serde_json::from_value::<SparseAgentManagementRequest>(bad.clone()).is_err(), "{bad}");
        }
        let a = sparse(json!({"operation":"create","agent":{"name":"x","pin":false},"task":"t"}));
        let b = sparse(json!({"task":"t","agent":{"name":"x"},"operation":"create"}));
        assert_eq!(a.digest().unwrap(), b.digest().unwrap());
        assert!(is_sparse_body(serde_json::to_vec(&a).unwrap().as_slice()));
    }
}
